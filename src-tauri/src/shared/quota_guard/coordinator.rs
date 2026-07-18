use std::future::Future;
use std::pin::Pin;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};

use crate::types::QuotaGuardSettings;
use super::gate::{AdmissionReason, ProcessGate, ProcessPolicy};
use super::model::{QuotaGuardRuntimeState, TurnKey};

pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 256;

pub(crate) type ControlFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// The serialized coordinator persists state before invoking this transport
/// boundary. Implementations must never hold the session map lock across a
/// protocol await.
pub(crate) trait AppServerControl: Send + Sync {
    fn read_rate_limits(&self, workspace_id: String) -> ControlFuture<'_>;
    fn read_identity(&self, workspace_id: String) -> ControlFuture<'_>;
    fn interrupt_turn(&self, turn: TurnKey) -> ControlFuture<'_>;
    fn read_thread(&self, workspace_id: String, thread_id: String) -> ControlFuture<'_>;
    fn resume_thread(&self, workspace_id: String, thread_id: String) -> ControlFuture<'_>;
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsChanged { pub(crate) previous: QuotaGuardSettings, pub(crate) updated: QuotaGuardSettings }
#[derive(Debug, Clone)]
pub(crate) enum QuotaGuardEvent {
    WorkspaceBound { session_epoch: String, workspace_id: String, canonical_codex_home: String },
    WorkspaceDisconnected { session_epoch: String, workspace_id: String },
    RateLimits { session_epoch: String, workspace_id: String, value: Value },
    TurnStarted { session_epoch: String, workspace_id: String, thread_id: String, turn_id: String },
    TurnCompleted { session_epoch: String, workspace_id: String, thread_id: String, turn_id: String, status: String, error: Option<Value> },
    AccountIdentityChanged { session_epoch: String, workspace_id: String, reason: String },
    PendingLocalStart { request_id: u64, session_epoch: String, workspace_id: String, request_thread_id: Option<String>, expected_thread_id: Option<String>, request_kind: String },
    StartResponse { request_id: u64, session_epoch: String, workspace_id: String, method: String, value: Value },
    StartFailed { request_id: u64, session_epoch: String, workspace_id: String, reason: String },
}

#[derive(Debug)]
pub(crate) enum ActorEvent {
    Observed(QuotaGuardEvent),
    /// Request lifecycle facts must be durably incorporated before the
    /// transport writes/forwards a result.  Raw app-server notifications stay
    /// on the non-blocking `Observed` path.
    ReliableObserved(QuotaGuardEvent, oneshot::Sender<Result<(), String>>),
    SettingsChanged(SettingsChanged, oneshot::Sender<Result<(), String>>),
    Command(QuotaGuardCommand, oneshot::Sender<Result<QuotaGuardPublicState, String>>),
    AppStartupRehydrate,
    FinalizeClosedEpisode { generation: u64 },
    DrainDeadline { generation: u64, deadline: i64 },
    Verify { generation: u64, verify_at: i64 },
    InterruptDeadline { turn: super::model::TurnKey, generation: u64, operation_id: u64, attempt: u8, acknowledgement: bool },
    StartExpiry { request_id: u64, generation: u64 },
    ProvisionalExpiry { turn: super::model::TurnKey, generation: u64, terminal: bool },
    HealthyRevalidate { generation: u64, due_at: i64 },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum QuotaGuardCommand { ApplyActionNow, KeepWaiting, InterruptNow, VerifyNow, RetryClosed }

pub(crate) struct QuotaGuardInner {
    pub(crate) runtime: Arc<Mutex<QuotaGuardRuntimeState>>,
    pub(crate) configured_workspaces: Mutex<BTreeSet<String>>,
    pub(crate) gate: ProcessGate,
    pub(crate) scheduled_healthy_due: StdMutex<Option<i64>>,
    pub(crate) sender: StdMutex<Option<mpsc::Sender<ActorEvent>>>,
    pub(crate) bindings: Mutex<HashMap<String, (String, String)>>,
    pub(crate) overflowed: AtomicBool,
    pub(crate) overflow_notify: Notify,
}

#[derive(Clone)]
pub(crate) struct QuotaGuardHandle { pub(crate) inner: Arc<QuotaGuardInner> }
#[derive(Clone)]
pub(crate) struct QuotaGuardEventSink { inner: Arc<QuotaGuardInner> }
impl Default for QuotaGuardHandle {
    fn default() -> Self {
        Self {
            inner: Arc::new(QuotaGuardInner {
                runtime: Arc::new(Mutex::new(QuotaGuardRuntimeState::default())),
                configured_workspaces: Mutex::new(BTreeSet::new()),
                gate: ProcessGate::default(),
                scheduled_healthy_due: StdMutex::new(None),
                sender: StdMutex::new(None),
                bindings: Mutex::new(HashMap::new()),
                overflowed: AtomicBool::new(false),
                overflow_notify: Notify::new(),
            }),
        }
    }
}

impl QuotaGuardHandle {
    pub(crate) fn gate(&self) -> ProcessGate { self.inner.gate.clone() }
    pub(crate) fn event_sink(&self) -> QuotaGuardEventSink { QuotaGuardEventSink { inner: Arc::clone(&self.inner) } }
    pub(crate) async fn runtime(&self) -> QuotaGuardRuntimeState { self.inner.runtime.lock().await.clone() }
    pub(crate) async fn set_configured_workspaces(&self, workspace_ids: BTreeSet<String>) {
        *self.inner.configured_workspaces.lock().await = workspace_ids;
    }
}

impl QuotaGuardEventSink {
    pub(crate) fn gate(&self) -> ProcessGate { self.inner.gate.clone() }

    /// This is deliberately non-blocking: the stdout reader must never wait
    /// behind durable policy work.  Loss is enforcement-significant, so both a
    /// full and a closed channel synchronously close admission before waking
    /// the actor's overflow reconciliation path.
    pub(crate) fn observe(&self, event: QuotaGuardEvent) -> Result<(), ()> {
        let Some(sender) = self.inner.sender.lock().expect("quota guard sender lock poisoned").clone() else {
            self.fail_closed();
            return Err(());
        };
        match sender.try_send(ActorEvent::Observed(event)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.fail_closed();
                Err(())
            }
        }
    }

    /// The bound dispatch path calls this before writing an inference-start
    /// request.  The acknowledgement means the coordinator persisted the fact,
    /// not merely that it accepted an in-memory message.
    pub(crate) async fn record_pending_start(&self, event: QuotaGuardEvent) -> Result<(), String> {
        if !matches!(event, QuotaGuardEvent::PendingLocalStart { .. }) {
            return Err("quota guard reliable start recording requires PendingLocalStart".into());
        }
        self.record_reliably(event).await
    }

    /// A failed write/timeout is just as important as a successful start:
    /// awaiting this prevents an orphaned durable pending start.
    pub(crate) async fn record_start_failed(&self, event: QuotaGuardEvent) -> Result<(), String> {
        if !matches!(event, QuotaGuardEvent::StartFailed { .. }) {
            return Err("quota guard reliable failure recording requires StartFailed".into());
        }
        self.record_reliably(event).await
    }

    async fn record_reliably(&self, event: QuotaGuardEvent) -> Result<(), String> {
        let Some(sender) = self.inner.sender.lock().expect("quota guard sender lock poisoned").clone() else {
            self.fail_closed();
            return Err("quota guard actor is unavailable".into());
        };
        let (reply, received) = oneshot::channel();
        if sender.send(ActorEvent::ReliableObserved(event, reply)).await.is_err() {
            self.fail_closed();
            return Err("quota guard actor is unavailable".into());
        }
        match received.await {
            Ok(result) => {
                if result.is_err() { self.fail_closed(); }
                result
            }
            Err(_) => {
                self.fail_closed();
                Err("quota guard actor is unavailable".into())
            }
        }
    }

    pub(crate) fn workspace_bound(&self, session_epoch: String, workspace_id: String, canonical_codex_home: String) -> Result<(), ()> {
        self.observe(QuotaGuardEvent::WorkspaceBound { session_epoch, workspace_id, canonical_codex_home })
    }


    fn fail_closed(&self) {
        self.inner.gate.close();
        self.inner.overflowed.store(true, Ordering::SeqCst);
        self.inner.overflow_notify.notify_one();
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlCall {
    ReadRateLimits(String),
    ReadIdentity(String),
    Interrupt(TurnKey),
    ReadThread { workspace_id: String, thread_id: String },
    ResumeThread { workspace_id: String, thread_id: String },
}

/// Deterministic reducer/effect harness. Tests inject deadlines explicitly,
/// inspect persisted-next effects, and restart from its cloned runtime state.
#[cfg(test)]
pub(crate) struct QuotaGuardHarness {
    pub(crate) runtime: QuotaGuardRuntimeState,
    pub(crate) settings: QuotaGuardSettings,
    pub(crate) now_ms: i64,
    effects: Vec<super::reducer::ReducerEffect>,
    configured_workspaces: BTreeSet<String>,
    bindings: BTreeMap<String, String>,
}

#[cfg(test)]
impl QuotaGuardHarness {
    pub(crate) fn new(settings: QuotaGuardSettings, now_ms: i64) -> Self {
        Self {
            runtime: QuotaGuardRuntimeState::default(),
            settings,
            now_ms,
            effects: Vec::new(),
            configured_workspaces: BTreeSet::new(),
            bindings: BTreeMap::new(),
        }
    }
    pub(crate) fn from_persisted(settings: QuotaGuardSettings, runtime: QuotaGuardRuntimeState, now_ms: i64) -> Self {
        Self { runtime, settings, now_ms, effects: Vec::new(), configured_workspaces: BTreeSet::new(), bindings: BTreeMap::new() }
    }
    pub(crate) fn dispatch(&mut self, event: super::reducer::ReducerEvent) {
        let (next, effects) = super::reducer::reduce(self.runtime.clone(), event, &self.settings);
        self.runtime = next;
        self.effects.extend(effects);
    }
    pub(crate) fn advance_to(&mut self, now_ms: i64) {
        assert!(now_ms >= self.now_ms, "test clock cannot move backward");
        self.now_ms = now_ms;
    }
    pub(crate) fn take_effects(&mut self) -> Vec<super::reducer::ReducerEffect> {
        std::mem::take(&mut self.effects)
    }
    pub(crate) fn restart(&self) -> Self {
        Self::from_persisted(self.settings.clone(), self.runtime.clone(), self.now_ms)
    }
    pub(crate) fn configure_workspaces(&mut self, workspace_ids: impl IntoIterator<Item = String>) {
        self.configured_workspaces = workspace_ids.into_iter().collect();
    }
    pub(crate) fn bind_workspace(&mut self, workspace_id: String, session_epoch: String) {
        self.configured_workspaces.insert(workspace_id.clone());
        self.bindings.insert(workspace_id, session_epoch);
    }
    pub(crate) fn disconnect_workspace(&mut self, workspace_id: &str) {
        self.bindings.remove(workspace_id);
    }
    pub(crate) fn public_admission(&self) -> BTreeMap<String, AdmissionProjection> {
        self.configured_workspaces.iter().map(|workspace_id| {
            let epoch = self.bindings.get(workspace_id).cloned();
            let phase = self.runtime.account.as_ref().map(|account| account.phase);
            let open = epoch.is_some() && matches!(phase, None | Some(super::model::QuotaGuardPhase::Disabled | super::model::QuotaGuardPhase::Monitoring | super::model::QuotaGuardPhase::Ready));
            let reason = if epoch.is_none() { "workspaceUnbound" } else if open { "open" } else { "processClosed" };
            (workspace_id.clone(), AdmissionProjection { session_epoch: epoch, open, reason: reason.to_string() })
        }).collect()
    }
    pub(crate) fn overflow(&mut self) {
        if let Some(account) = self.runtime.account.as_mut() {
            account.phase = super::model::QuotaGuardPhase::InterventionRequired;
            account.monitor_healthy = false;
            account.last_error = Some("event channel overflow".into());
            account.updated_at = self.now_ms;
        }
    }

    /// Runs only transport effects against a supplied fake. Non-transport
    /// effects remain observable in `take_effects`; each control outcome is
    /// reduced as a new persisted event before the next effect is considered.
    pub(crate) async fn execute_control_effects(&mut self, control: &dyn AppServerControl) {
        let effects = self.take_effects();
        for effect in effects {
            match effect {
                super::reducer::ReducerEffect::Interrupt { turn, generation, operation_id, attempt } => {
                    match control.interrupt_turn(turn.clone()).await {
                        Ok(_) => self.dispatch(super::reducer::ReducerEvent::InterruptAcknowledged { turn, generation, operation_id, attempt, now_ms: self.now_ms }),
                        Err(_) => self.dispatch(super::reducer::ReducerEvent::InterruptRequestFailed { turn, generation, operation_id, attempt, now_ms: self.now_ms }),
                    }
                }
                super::reducer::ReducerEffect::ReconcileThread { turn, generation, operation_id, attempt } => {
                    match control.read_thread(turn.workspace_id.clone(), turn.thread_id.clone()).await {
                        Ok(value) => self.dispatch(super::reducer::ReducerEvent::InterruptReconciled {
                            active_turn_id: fake_active_turn_id(&value), turn, generation, operation_id, attempt, now_ms: self.now_ms,
                        }),
                        Err(reason) => self.dispatch(super::reducer::ReducerEvent::InterruptReconcileFailed {
                            turn, generation, operation_id, attempt, reason, now_ms: self.now_ms,
                        }),
                    }
                }
                other => self.effects.push(other),
            }
        }
    }

}
#[cfg(test)]
fn fake_active_turn_id(value: &Value) -> Option<String> {
    ["activeTurnId", "active_turn_id", "turnId", "turn_id"].iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).map(ToOwned::to_owned))
        .or_else(|| ["result", "params", "turn"].iter().find_map(|key| value.get(*key).and_then(fake_active_turn_id)))
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FakeAppServerControl {
    calls: Arc<StdMutex<Vec<ControlCall>>>,
    replies: Arc<StdMutex<HashMap<String, std::collections::VecDeque<Result<Value, String>>>>>,
}

#[cfg(test)]
impl FakeAppServerControl {
    pub(crate) fn queue(&self, operation: impl Into<String>, reply: Result<Value, String>) {
        self.replies.lock().expect("fake control lock poisoned").entry(operation.into()).or_default().push_back(reply);
    }
    pub(crate) fn calls(&self) -> Vec<ControlCall> {
        self.calls.lock().expect("fake control lock poisoned").clone()
    }
    fn next(&self, operation: String, call: ControlCall) -> ControlFuture<'_> {
        self.calls.lock().expect("fake control lock poisoned").push(call);
        Box::pin(async move {
            self.replies.lock().expect("fake control lock poisoned")
                .get_mut(&operation).and_then(std::collections::VecDeque::pop_front)
                .unwrap_or_else(|| Err(format!("no fake reply queued for {operation}")))
        })
    }
}

#[cfg(test)]
impl AppServerControl for FakeAppServerControl {
    fn read_rate_limits(&self, workspace_id: String) -> ControlFuture<'_> {
        self.next(format!("rate:{workspace_id}"), ControlCall::ReadRateLimits(workspace_id))
    }
    fn read_identity(&self, workspace_id: String) -> ControlFuture<'_> {
        self.next(format!("identity:{workspace_id}"), ControlCall::ReadIdentity(workspace_id))
    }
    fn interrupt_turn(&self, turn: TurnKey) -> ControlFuture<'_> {
        self.next(format!("interrupt:{}", turn.stable_id()), ControlCall::Interrupt(turn))
    }
    fn read_thread(&self, workspace_id: String, thread_id: String) -> ControlFuture<'_> {
        self.next(format!("thread:{workspace_id}:{thread_id}"), ControlCall::ReadThread { workspace_id, thread_id })
    }
    fn resume_thread(&self, workspace_id: String, thread_id: String) -> ControlFuture<'_> {
        self.next(format!("resume:{workspace_id}:{thread_id}"), ControlCall::ResumeThread { workspace_id, thread_id })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaGuardPublicState {
    pub(crate) account_key: Option<String>,
    pub(crate) account_label: Option<String>,
    pub(crate) phase: super::model::QuotaGuardPhase,
    pub(crate) snapshot: Option<super::model::RateLimitSnapshot>,
    pub(crate) snapshot_fresh: bool,
    pub(crate) breached_windows: Vec<super::model::QuotaWindowKind>,
    pub(crate) affected_turns: Vec<QuotaGuardPublicTurn>,
    pub(crate) drain_deadline: Option<i64>,
    pub(crate) verify_at: Option<i64>,
    pub(crate) monitor_healthy: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) activity: Vec<QuotaGuardPublicActivityEntry>,
    pub(crate) admission_by_workspace: BTreeMap<String, AdmissionProjection>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaGuardPublicTurn {
    pub(crate) workspace_id: String,
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaGuardPublicActivityEntry {
    pub(crate) id: Option<String>,
    pub(crate) kind: super::model::QuotaGuardActivityKind,
    pub(crate) timestamp: i64,
    pub(crate) operation_id: Option<String>,
    pub(crate) workspace_id: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) turn_id: Option<String>,
    pub(crate) attempt: Option<u8>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdmissionProjection {
    pub(crate) session_epoch: Option<String>,
    pub(crate) open: bool,
    pub(crate) reason: String,
}

pub(crate) fn policy_name(policy: ProcessPolicy) -> &'static str { match policy { ProcessPolicy::DisabledOpen => "disabledOpen", ProcessPolicy::EnabledClosed => "enabledClosed", ProcessPolicy::EnabledOpen => "enabledOpen" } }
pub(crate) fn reason_name(reason: AdmissionReason) -> &'static str { match reason { AdmissionReason::Open => "open", AdmissionReason::GuardDisabled => "guardDisabled", AdmissionReason::ProcessClosed => "processClosed", AdmissionReason::EpochUnverified => "epochUnverified", AdmissionReason::WorkspaceUnbound => "workspaceUnbound" } }
