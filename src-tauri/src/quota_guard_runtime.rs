use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, oneshot};

use crate::shared::codex_core::{account_rate_limits_core, account_read_strict_core, read_thread_core, resume_thread_core, strict_account_identity, turn_interrupt_core};
use crate::shared::quota_guard::coordinator::{
    reason_name, ActorEvent, AdmissionProjection, AppServerControl, ControlFuture,
    QuotaGuardCommand, QuotaGuardEvent, QuotaGuardHandle,
    QuotaGuardPublicActivityEntry, QuotaGuardPublicState, QuotaGuardPublicSuspendedEngine, QuotaGuardPublicTurn,
    SettingsChanged, EVENT_CHANNEL_CAPACITY,
};
use crate::shared::quota_guard::gate::ProcessPolicy;
use crate::shared::quota_guard::external_suspend;
use crate::shared::quota_guard::model::{PendingLocalStart, PendingThresholdSettings, QuotaGuardActivityEntry, QuotaGuardActivityKind, QuotaGuardPhase, TurnKey};
use crate::shared::quota_guard::parser::parse_rate_limits;
use crate::shared::quota_guard::persistence::{load_runtime, persist_runtime};
use crate::shared::quota_guard::recovery::recover;
use crate::shared::quota_guard::reducer::{reduce, ReducerEffect, ReducerEvent};
use crate::state::AppState;
use crate::types::QuotaGuardSettings;

const BLOCKED_PREFIX: &str = "QUOTA_GUARD_BLOCKED";
const THRESHOLD_SETTLE_DELAY_MS: i64 = 10_000;
const MONITOR_POLL_INTERVAL_MS: i64 = 10_000;

#[derive(Clone)]
struct LocalAppServerControl {
    app: AppHandle,
}

impl AppServerControl for LocalAppServerControl {
    fn read_rate_limits(&self, workspace_id: String) -> ControlFuture<'_> {
        Box::pin(async move { account_rate_limits_core(&self.app.state::<AppState>().sessions, workspace_id).await })
    }
    fn read_identity(&self, workspace_id: String) -> ControlFuture<'_> {
        Box::pin(async move { account_read_strict_core(&self.app.state::<AppState>().sessions, workspace_id).await })
    }
    fn interrupt_turn(&self, turn: TurnKey) -> ControlFuture<'_> {
        Box::pin(async move { turn_interrupt_core(&self.app.state::<AppState>().sessions, turn.workspace_id, turn.thread_id, turn.turn_id).await })
    }
    fn read_thread(&self, workspace_id: String, thread_id: String) -> ControlFuture<'_> {
        Box::pin(async move { read_thread_core(&self.app.state::<AppState>().sessions, workspace_id, thread_id).await })
    }
    fn resume_thread(&self, workspace_id: String, thread_id: String) -> ControlFuture<'_> {
        Box::pin(async move { resume_thread_core(&self.app.state::<AppState>().sessions, workspace_id, thread_id).await })
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
impl QuotaGuardHandle {

    pub(crate) async fn public_state(&self) -> QuotaGuardPublicState {
        let bindings = self.inner.bindings.lock().await.clone();
        public_state_with_bindings(self, &bindings).await
    }

    /// Starts the sole local coordinator after AppState is managed. Repeated
    /// setup calls are idempotent.
    pub(crate) fn start(&self, app: AppHandle, state_path: PathBuf) {
        let (sender, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let mut slot = self.inner.sender.lock().expect("quota guard sender lock poisoned");
        if slot.is_some() { return; }
        *slot = Some(sender);
        drop(slot);
        let handle = self.clone();
        tauri::async_runtime::spawn(async move { actor_loop(handle, app, state_path, receiver).await; });
    }
    /// Close synchronously before an enable write so inference cannot race the
    /// transaction. The actor will establish identity before it opens permits.
    pub(crate) async fn begin_enable(&self) { self.inner.gate.close(); }

    pub(crate) fn rehydrate(&self) {
        if let Some(sender) = self.inner.sender.lock().expect("quota guard sender lock poisoned").clone() {
            // Startup is best-effort only because actor_loop has already
            // performed the closed recovery decision before receiving work.
            let _ = sender.try_send(ActorEvent::AppStartupRehydrate);
        } else {
            self.inner.gate.close();
        }
    }

    pub(crate) async fn settings_changed(&self, change: SettingsChanged) -> Result<(), String> {
        let sender = self.inner.sender.lock().expect("quota guard sender lock poisoned").clone()
            .ok_or_else(|| "quota guard actor is unavailable".to_string())?;
        let (reply, receive) = oneshot::channel();
        sender.send(ActorEvent::SettingsChanged(change, reply)).await.map_err(|_| "quota guard actor is unavailable".to_string())?;
        receive.await.map_err(|_| "quota guard actor is unavailable".to_string())?
    }

    async fn command(&self, command: QuotaGuardCommand) -> Result<QuotaGuardPublicState, String> {
        let sender = self.inner.sender.lock().expect("quota guard sender lock poisoned").clone()
            .ok_or_else(|| "quota guard actor is unavailable".to_string())?;
        let (reply, receive) = oneshot::channel();
        sender.send(ActorEvent::Command(command, reply)).await.map_err(|_| "quota guard actor is unavailable".to_string())?;
        receive.await.map_err(|_| "quota guard actor is unavailable".to_string())?
    }

    pub(crate) async fn fail_closed(&self, message: &str) {
        self.inner.gate.close();
        let mut runtime = self.inner.runtime.lock().await;
        if let Some(account) = runtime.account.as_mut() {
            account.phase = QuotaGuardPhase::InterventionRequired;
            account.last_error = Some(message.to_string());
            account.updated_at = now_ms();
        }
    }
}

async fn actor_loop(handle: QuotaGuardHandle, app: AppHandle, path: PathBuf, mut receiver: mpsc::Receiver<ActorEvent>) {
    let settings = app.state::<AppState>().app_settings.lock().await.clone();
    let decision = recover(settings.quota_guard.enabled, load_runtime(&path, now_ms()), now_ms());
    handle.inner.gate.set_policy(decision.policy);
    let runtime = restore_runtime_for_startup(decision.state, &settings.quota_guard);
    *handle.inner.runtime.lock().await = runtime;
    if let Err(error) = persist_current(&handle, &path).await {
        handle.fail_closed(&format!("quota guard startup persistence failed: {error}")).await;
    }
    emit_state(&app, &handle).await;
    // Recovery never opens a persisted guard.  Connecting the most recently
    // verified workspace is a prerequisite for the later WorkspaceBound
    // bootstrap, strict identity read, and full quota read.
    if settings.quota_guard.enabled {
        if let Some(workspace_id) = handle.runtime().await.account
            .and_then(|account| account.associated_workspace_ids.first().cloned())
        {
            let state = app.state::<AppState>();
            if let Err(error) = crate::workspaces::connect_workspace_local(&app, &state, workspace_id).await {
                mark_intervention(&handle, &path, &format!("quota guard startup workspace recovery failed: {error}")).await;
            }
        }
    }

    let overflow_handle = handle.clone();
    let overflow_path = path.clone();
    let overflow_app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            overflow_handle.inner.overflow_notify.notified().await;
            if overflow_handle.inner.overflowed.swap(false, Ordering::SeqCst) {
                mark_intervention(&overflow_handle, &overflow_path, "event channel overflow").await;
                emit_state(&overflow_app, &overflow_handle).await;
            }
        }
    });

    let mut bindings = HashMap::<String, (String, String)>::new();
    while let Some(event) = receiver.recv().await {
        let result = match event {
            ActorEvent::Observed(event) => handle_observed(&handle, &app, &path, &mut bindings, event).await,
            ActorEvent::ReliableObserved(event, reply) => {
                let result = match handle_observed(&handle, &app, &path, &mut bindings, event).await {
                    Ok(()) => persist_current(&handle, &path).await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
                Ok(())
            }
            ActorEvent::SettingsChanged(change, reply) => {
                let result = handle_settings_changed(&handle, &app, &path, &mut bindings, change).await;
                let _ = reply.send(result);
                Ok(())
            }
            ActorEvent::Command(command, reply) => {
                let result = handle_command(&handle, &app, &path, &mut bindings, command).await;
                let _ = reply.send(result);
                Ok(())
            }
            ActorEvent::AppStartupRehydrate => {
                if let Err(error) = reconcile_startup_external_engines(&handle, &app, &path).await {
                    Err(error)
                } else {
                    let runtime = handle.runtime().await;
                    if let Some(account) = runtime.account {
                        let generation = runtime.lifecycle_generation;
                        if let Some(deadline) = account.drain_deadline { schedule_drain(&handle, generation, deadline); }
                        if matches!(account.phase, QuotaGuardPhase::Parked | QuotaGuardPhase::VerifyingReset) {
                            if let Some(verify_at) = account.verify_at { schedule_verification(&handle, generation, verify_at); }
                        }
                        // First read fires immediately so a fresh launch shows current
                        // usage instead of the persisted snapshot from the last run. A
                        // parked episode still needs these reads for monitor health and
                        // external-engine re-sweeps; reducer semantics keep its threshold
                        // evaluation blocked until verification is due.
                        schedule_monitor_poll_if_enabled(&handle, generation, now_ms()).await;
                        apply_event(&handle, &app, &path, &mut bindings, ReducerEvent::RehydratePendingInterrupts { now_ms: now_ms() }).await
                    } else {
                        Ok(())
                    }
                }
            }
            ActorEvent::ThresholdSettled { settles_at } =>
                settle_pending_thresholds(&handle, &app, &path, &mut bindings, settles_at).await,
            ActorEvent::FinalizeClosedEpisode { generation } =>
                apply_event(&handle, &app, &path, &mut bindings, ReducerEvent::FinalizeClosedEpisode { transition_id: generation, now_ms: now_ms() }).await,
            ActorEvent::DrainDeadline { generation, deadline } => {
                if handle.runtime().await.account.as_ref().is_some_and(|account| account.drain_deadline == Some(deadline))
                    && handle.runtime().await.lifecycle_generation == generation {
                    apply_event(&handle, &app, &path, &mut bindings, ReducerEvent::DrainDeadline { generation, now_ms: now_ms() }).await
                } else { Ok(()) }
            }
            ActorEvent::Verify { generation, verify_at } => {
                if handle.runtime().await.account.as_ref().is_some_and(|account| account.verify_at == Some(verify_at))
                    && handle.runtime().await.lifecycle_generation == generation {
                    verify_once(&handle, &app, &path, &mut bindings, false).await
                } else { Ok(()) }
            }
            ActorEvent::InterruptDeadline { turn, generation, operation_id, attempt, acknowledgement } =>
                apply_event(&handle, &app, &path, &mut bindings, ReducerEvent::InterruptDeadline { turn, generation, operation_id, attempt, acknowledgement, now_ms: now_ms() }).await,
            ActorEvent::StartExpiry { request_id, generation } =>
                apply_event(&handle, &app, &path, &mut bindings, ReducerEvent::PendingStartExpired { request_id, generation, now_ms: now_ms() }).await,
            ActorEvent::ProvisionalExpiry { turn, generation, terminal } =>
                apply_event(&handle, &app, &path, &mut bindings, ReducerEvent::ProvisionalExpired { turn, generation, terminal, now_ms: now_ms() }).await,
            ActorEvent::HealthyRevalidate { generation, due_at } => {
                let runtime = handle.runtime().await;
                let pollable = monitor_pollable(&runtime, generation);
                let due = now_ms() >= due_at;
                if due {
                    *handle.inner.scheduled_healthy_due.lock().expect("healthy timer lock poisoned") = None;
                }
                if pollable && due {
                    match healthy_revalidate(&handle, &app, &path, &mut bindings).await {
                        Ok(()) => {
                            schedule_monitor_poll_if_enabled(&handle, generation, now_ms().saturating_add(MONITOR_POLL_INTERVAL_MS)).await;
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    Ok(())
                }
            }
        };
        if let Err(error) = result {
            mark_intervention(&handle, &path, &error).await;
            let runtime = handle.runtime().await;
            schedule_monitor_poll_if_enabled(&handle, runtime.lifecycle_generation, now_ms().saturating_add(MONITOR_POLL_INTERVAL_MS)).await;
        }
        *handle.inner.bindings.lock().await = bindings.clone();
        emit_state(&app, &handle).await;
    }
    handle.inner.gate.close();
}

/// The app settings file is authoritative after a completed threshold update.
/// Keep an in-flight debounce's durable effective settings, but do not let an
/// older completed value control the first post-restart snapshot evaluation.
fn restore_runtime_for_startup(
    mut runtime: crate::shared::quota_guard::model::QuotaGuardRuntimeState,
    settings: &QuotaGuardSettings,
) -> crate::shared::quota_guard::model::QuotaGuardRuntimeState {
    if runtime.pending_thresholds.is_none() || runtime.effective_settings.is_none() {
        runtime.effective_settings = Some(settings.clone());
    }
    runtime
}

async fn handle_settings_changed(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, change: SettingsChanged) -> Result<(), String> {
    let (change, schedule_at) = prepare_settings_change(handle, change, now_ms()).await;
    persist_current(handle, path).await?;
    apply_effective_settings_changed(handle, app, path, bindings, change).await?;
    if let Some(settles_at) = schedule_at {
        schedule_threshold_settle(handle, settles_at)?;
    }
    Ok(())
}

async fn apply_effective_settings_changed(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, change: SettingsChanged) -> Result<(), String> {
    if (change.previous.external_suspend && !change.updated.external_suspend)
        || (change.previous.armed && !change.updated.armed) {
        resume_external_engines(handle, path).await?;
        // The episode policy is durable so later blocked snapshots cannot
        // refreeze a process that was just intentionally resumed.
        disable_external_suspension_for_active_episode(handle, path).await?;
    }
    if !change.updated.enabled {
        apply_event(handle, app, path, bindings, ReducerEvent::Disable { now_ms: now_ms() }).await?;
        handle.inner.gate.set_policy(ProcessPolicy::DisabledOpen);
        return Ok(());
    }
    if !change.previous.enabled && change.updated.enabled {
        // A workspace can already be connected when the setting flips.  The
        // enable acknowledgement is not complete until strict identity and a
        // fresh full quota snapshot have passed through this closed bootstrap.
        handle.inner.gate.close();
        persist_current(handle, path).await?;
        let workspace_id = bindings.keys().next().cloned()
            .ok_or_else(|| "quota guard requires a connected local workspace before it can be enabled".to_string())?;
        bootstrap_workspace(handle, app, path, bindings, &workspace_id).await?;
    } else if change.previous != change.updated {
        let thresholds_changed = change.previous.primary_threshold_percent != change.updated.primary_threshold_percent
            || change.previous.secondary_threshold_percent != change.updated.secondary_threshold_percent;
        // A raised threshold can make the snapshot that caused the parked
        // episode healthy. Run the same authoritative read as Verify now.
        apply_event_with_settings(
            handle,
            app,
            path,
            bindings,
            ReducerEvent::SettingsChanged { thresholds_changed },
            &change.updated,
        )
        .await?;
        synchronize_gate_with_runtime(handle, bindings).await;
    }
    Ok(())
}

/// Stores UI threshold drafts durably, but holds the coordinator at its last
/// effective thresholds until the draft has remained unchanged for ten seconds.
/// Non-threshold settings are copied into the effective value immediately.
async fn prepare_settings_change(handle: &QuotaGuardHandle, requested: SettingsChanged, now: i64) -> (SettingsChanged, Option<i64>) {
    let mut runtime = handle.inner.runtime.lock().await;
    let previous = runtime.effective_settings.clone().unwrap_or_else(|| requested.previous.clone());
    let requested_thresholds_changed = requested.previous.primary_threshold_percent != requested.updated.primary_threshold_percent
        || requested.previous.secondary_threshold_percent != requested.updated.secondary_threshold_percent;
    let mut updated = requested.updated;
    let mut schedule_at = None;

    if requested_thresholds_changed {
        if updated.primary_threshold_percent == previous.primary_threshold_percent
            && updated.secondary_threshold_percent == previous.secondary_threshold_percent {
            runtime.pending_thresholds = None;
        } else {
            let settles_at = now.saturating_add(THRESHOLD_SETTLE_DELAY_MS);
            runtime.pending_thresholds = Some(PendingThresholdSettings {
                primary_threshold_percent: updated.primary_threshold_percent,
                secondary_threshold_percent: updated.secondary_threshold_percent,
                settles_at,
            });
            updated.primary_threshold_percent = previous.primary_threshold_percent;
            updated.secondary_threshold_percent = previous.secondary_threshold_percent;
            schedule_at = Some(settles_at);
        }
    } else if let Some(pending) = runtime.pending_thresholds.as_ref() {
        updated.primary_threshold_percent = previous.primary_threshold_percent;
        updated.secondary_threshold_percent = previous.secondary_threshold_percent;
        schedule_at = Some(pending.settles_at);
    }

    runtime.effective_settings = Some(updated.clone());
    (SettingsChanged { previous, updated }, schedule_at)
}

async fn settle_pending_thresholds(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, settles_at: i64) -> Result<(), String> {
    if let Some(change) = take_settled_threshold_change(handle, settles_at).await? {
        apply_effective_settings_changed(handle, app, path, bindings, change).await?;
    }
    Ok(())
}

async fn take_settled_threshold_change(handle: &QuotaGuardHandle, settles_at: i64) -> Result<Option<SettingsChanged>, String> {
    let mut runtime = handle.inner.runtime.lock().await;
    let Some(pending) = runtime.pending_thresholds.clone().filter(|pending| pending.settles_at == settles_at) else {
        return Ok(None);
    };
    let previous = runtime.effective_settings.clone()
        .ok_or_else(|| "quota guard effective settings are unavailable".to_string())?;
    let mut updated = previous.clone();
    updated.primary_threshold_percent = pending.primary_threshold_percent;
    updated.secondary_threshold_percent = pending.secondary_threshold_percent;
    runtime.effective_settings = Some(updated.clone());
    runtime.pending_thresholds = None;
    Ok(Some(SettingsChanged { previous, updated }))
}

fn schedule_threshold_settle(handle: &QuotaGuardHandle, settles_at: i64) -> Result<(), String> {
    let sender = handle.inner.sender.lock().expect("quota guard sender lock poisoned").clone()
        .ok_or_else(|| "quota guard actor is unavailable".to_string())?;
    tauri::async_runtime::spawn(async move {
        let delay = settles_at.saturating_sub(now_ms());
        tokio::time::sleep(Duration::from_millis(u64::try_from(delay).unwrap_or_default())).await;
        let _ = sender.send(ActorEvent::ThresholdSettled { settles_at }).await;
    });
    Ok(())
}

async fn synchronize_gate_with_runtime(handle: &QuotaGuardHandle, bindings: &HashMap<String, (String, String)>) {
    let phase = handle.runtime().await.account.as_ref().map(|account| account.phase);
    if matches!(phase, Some(QuotaGuardPhase::Monitoring | QuotaGuardPhase::Ready)) {
        handle.inner.gate.set_policy(ProcessPolicy::EnabledOpen);
        for (workspace_id, (epoch, _)) in bindings {
            handle.inner.gate.set_epoch_open(epoch, workspace_id, true);
        }
    } else {
        handle.inner.gate.close();
    }
}

async fn handle_observed(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, event: QuotaGuardEvent) -> Result<(), String> {
    match event {
        QuotaGuardEvent::WorkspaceBound { session_epoch, workspace_id, canonical_codex_home } => {
            handle.inner.gate.register_closed_epoch(session_epoch.clone(), workspace_id.clone());
            bindings.insert(workspace_id.clone(), (session_epoch, canonical_codex_home));
            if app.state::<AppState>().app_settings.lock().await.quota_guard.enabled {
                bootstrap_workspace(handle, app, path, bindings, &workspace_id).await?;
            }
            if let Some(pending) = handle.runtime().await.pending_thresholds {
                schedule_threshold_settle(handle, pending.settles_at)?;
            }
        }
        QuotaGuardEvent::WorkspaceDisconnected { session_epoch, workspace_id } => {
            handle.inner.gate.revoke_epoch(&session_epoch, &workspace_id);
            if bindings.get(&workspace_id).is_some_and(|(epoch, _)| epoch == &session_epoch) {
                bindings.remove(&workspace_id);
                if handle.inner.gate.policy() != ProcessPolicy::DisabledOpen {
                    mark_intervention(handle, path, "guarded workspace disconnected").await;
                }
            }
        }
        QuotaGuardEvent::RateLimits { session_epoch, workspace_id, value } => {
            if bindings.get(&workspace_id).is_some_and(|(epoch, _)| epoch == &session_epoch) {
                apply_rate_limits(handle, app, path, bindings, &workspace_id, value, false, false).await?;
            }
        }
        QuotaGuardEvent::TurnStarted { session_epoch, workspace_id, thread_id, turn_id } => {
            if bindings.get(&workspace_id).is_some_and(|(epoch, _)| epoch == &session_epoch) {
                apply_event(handle, app, path, bindings, ReducerEvent::TurnStarted {
                    turn: TurnKey { session_epoch, workspace_id, thread_id, turn_id }, now_ms: now_ms(),
                }).await?;
            }
        }
        QuotaGuardEvent::TurnCompleted { session_epoch, workspace_id, thread_id, turn_id, status, error } => {
            if bindings.get(&workspace_id).is_some_and(|(epoch, _)| epoch == &session_epoch) {
                apply_event(handle, app, path, bindings, ReducerEvent::TurnTerminal {
                    turn: TurnKey { session_epoch, workspace_id, thread_id, turn_id }, status, error, now_ms: now_ms(),
                }).await?;
            }
        }
        QuotaGuardEvent::AccountIdentityChanged { session_epoch, workspace_id, reason } => {
            if bindings.get(&workspace_id).is_some_and(|(epoch, _)| epoch == &session_epoch) {
                handle.inner.gate.set_policy(ProcessPolicy::EnabledClosed);
                bootstrap_workspace(handle, app, path, bindings, &workspace_id)
                    .await
                    .map_err(|error| format!("identity revalidation ({reason}) failed: {error}"))?;
            }
        }
        QuotaGuardEvent::PendingLocalStart {
            request_id, session_epoch, workspace_id, request_thread_id, expected_thread_id, request_kind,
        } => {
            if !bindings.get(&workspace_id).is_some_and(|(epoch, _)| epoch == &session_epoch) {
                return Ok(());
            }
            let start = PendingLocalStart {
                request_id, session_epoch, workspace_id, request_thread_id, expected_thread_id, request_kind,
                response_thread_id: None, response_received_at: None,
                generation: handle.runtime().await.lifecycle_generation,
                disposition: None, registered_at: now_ms(),
            };
            apply_event(handle, app, path, bindings, ReducerEvent::PendingStartRecorded { start, now_ms: now_ms() }).await?;
        }
        QuotaGuardEvent::StartFailed { request_id, session_epoch, workspace_id, .. } => {
            if bindings.get(&workspace_id).is_some_and(|(epoch, _)| epoch == &session_epoch) {
                let generation = handle.runtime().await.lifecycle_generation;
                apply_event(handle, app, path, bindings, ReducerEvent::PendingStartFailed { request_id, generation, now_ms: now_ms() }).await?;
            }
        }
        QuotaGuardEvent::StartResponse { request_id, session_epoch, workspace_id, value, .. } => {
            if bindings.get(&workspace_id).is_some_and(|(epoch, _)| epoch == &session_epoch) {
                apply_event(handle, app, path, bindings, ReducerEvent::StartResponse {
                    request_id, session_epoch, workspace_id, thread_id: response_thread_id(&value), now_ms: now_ms(),
                }).await?;
            }
        }
    }
    Ok(())
}

async fn bootstrap_workspace(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, workspace_id: &str) -> Result<(), String> {
    let Some((epoch, home)) = bindings.get(workspace_id).cloned() else { return Err("workspace binding missing for quota bootstrap".into()); };
    handle.inner.gate.set_policy(ProcessPolicy::EnabledClosed);
    let control = LocalAppServerControl { app: app.clone() };
    let identity = control.read_identity(workspace_id.to_string()).await?;
    let identity = strict_account_identity(&identity).ok_or_else(|| "account/read returned no strict identity".to_string())?;
    let account_key = account_key(&home, &identity);
    let current = handle.runtime().await;
    match current.account.as_ref().map(|account| (account.account_key.as_str(), account.phase)) {
        Some((existing, _)) if existing != account_key => return Err("strict account identity changed; disable and re-enable quota guard".into()),
        None | Some((_, QuotaGuardPhase::Disabled)) => {
            let settings = effective_settings(handle, app).await;
            apply_event_with_settings(handle, app, path, bindings, ReducerEvent::Enable { account_key, now_ms: now_ms() }, &settings).await?;
        }
        _ => {}
    }
    let rate_limits = control.read_rate_limits(workspace_id.to_string()).await?;
    apply_rate_limits(handle, app, path, bindings, workspace_id, rate_limits, true, false).await?;
    {
        let mut runtime = handle.inner.runtime.lock().await;
        if let Some(account) = runtime.account.as_mut() {
            account.associated_workspace_ids.retain(|id| id != workspace_id);
            account.associated_workspace_ids.insert(0, workspace_id.to_string());
            account.updated_at = now_ms();
        }
    }
    let persisted_turns = handle.runtime().await.account.map(|account| {
        account.local_turn_registry.into_iter()
            .chain(account.allowed_drain_turns)
            .chain(account.pending_interrupt_index.into_values().map(|pending| pending.turn))
            .collect::<Vec<_>>()
    }).unwrap_or_default();
    for turn in persisted_turns {
        let active = control.read_thread(turn.workspace_id.clone(), turn.thread_id.clone()).await?;
        if active_turn_id(&active).as_deref() != Some(turn.turn_id.as_str()) {
            apply_event(handle, app, path, bindings, ReducerEvent::TurnTerminal {
                turn, status: "reconciled".into(), error: None, now_ms: now_ms(),
            }).await?;
        }
    }
    persist_current(handle, path).await?;
    let runtime = handle.runtime().await;
    schedule_monitor_poll_if_enabled(handle, runtime.lifecycle_generation, now_ms().saturating_add(MONITOR_POLL_INTERVAL_MS)).await;
    if handle.inner.gate.policy() == ProcessPolicy::EnabledOpen { handle.inner.gate.set_epoch_open(&epoch, workspace_id, true); }
    Ok(())
}

async fn apply_rate_limits(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, _workspace_id: &str, value: Value, full_read: bool, verification: bool) -> Result<(), String> {
    let prior = handle.runtime().await.account.and_then(|account| account.snapshot);
    let snapshot = parse_rate_limits(&value, prior.as_ref(), now_ms())?;
    let settings = effective_settings(handle, app).await;
    apply_event_with_settings(handle, app, path, bindings, ReducerEvent::Snapshot {
        snapshot,
        full_read,
        verification,
        now_ms: now_ms(),
    }, &settings).await
}


async fn apply_event(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, event: ReducerEvent) -> Result<(), String> {
    let settings = effective_settings(handle, app).await;
    apply_event_with_settings(handle, app, path, bindings, event, &settings).await
}

async fn effective_settings(handle: &QuotaGuardHandle, app: &AppHandle) -> QuotaGuardSettings {
    if let Some(settings) = handle.runtime().await.effective_settings {
        return settings;
    }
    // This fallback is only for a just-created or legacy runtime before actor
    // startup has persisted its initial effective settings.
    app.state::<AppState>().app_settings.lock().await.quota_guard.clone()
}

async fn apply_event_with_settings(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, event: ReducerEvent, settings: &QuotaGuardSettings) -> Result<(), String> {
    let current = handle.runtime().await;
    let (next, effects) = reduce(current, event, settings);
    close_enforcement_before_persistence(handle, &effects);
    *handle.inner.runtime.lock().await = next;
    persist_current(handle, path).await?;
    for effect in effects { run_effect(handle, app, path, bindings, effect).await?; }
    Ok(())
}

fn close_enforcement_before_persistence(handle: &QuotaGuardHandle, effects: &[ReducerEffect]) {
    if effects.iter().any(|effect| matches!(effect, ReducerEffect::SetProcessClosed)) {
        // Closing admission is the sole intentional pre-persistence effect:
        // once an enforcement transition is selected, no later request may
        // acquire an admission while the durable record is being written.
        handle.inner.gate.close();
    }
}

fn enqueue_finalization_after_admissions(handle: &QuotaGuardHandle, generation: u64) -> Result<(), String> {
    let sender = handle.inner.sender.lock().expect("quota guard sender lock poisoned").clone()
        .ok_or_else(|| "quota guard actor is unavailable".to_string())?;
    let waiter = handle.clone();
    tauri::async_runtime::spawn(async move {
        waiter.inner.gate.wait_for_admissions().await;
        let _ = sender.send(ActorEvent::FinalizeClosedEpisode { generation }).await;
    });
    Ok(())
}

async fn run_effect(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, effect: ReducerEffect) -> Result<(), String> {
    match effect {
        ReducerEffect::SetProcessClosed => handle.inner.gate.close(),
        ReducerEffect::SuspendExternalEngines => suspend_external_engines(handle, app, path, true).await?,
        ReducerEffect::MaintainExternalEngineSuspension { prevent_new_sessions } => suspend_external_engines(handle, app, path, prevent_new_sessions).await?,
        ReducerEffect::ResumeExternalEngines => resume_external_engines(handle, path).await?,
        ReducerEffect::SetProcessOpen => {
            handle.inner.gate.set_policy(ProcessPolicy::EnabledOpen);
            for (workspace_id, (epoch, _)) in bindings.iter() { handle.inner.gate.set_epoch_open(epoch, workspace_id, true); }
            let ready = handle.runtime().await.account.as_ref().is_some_and(|account| account.phase == QuotaGuardPhase::Ready);
            let notify_when_available = effective_settings(handle, app).await.notify_when_available;
            if ready && notify_when_available {
                let _ = crate::notifications::notify_quota_available(app, "Quota guard ready", "Quota limits have been verified healthy.", "quota-guard");
            }
        }
        ReducerEffect::FinalizeClosedEpisode { transition_id } => {
            enqueue_finalization_after_admissions(handle, transition_id)?;
        }
        ReducerEffect::Interrupt { turn, generation, operation_id, attempt } => {
            append_activity(handle, path, QuotaGuardActivityKind::InterruptRequested, Some(operation_id), Some(&turn), None).await?;
            let control = LocalAppServerControl { app: app.clone() };
            match control.interrupt_turn(turn.clone()).await {
                Ok(_) => {
                    Box::pin(apply_event(handle, app, path, bindings, ReducerEvent::InterruptAcknowledged {
                        turn: turn.clone(), generation, operation_id, attempt, now_ms: now_ms(),
                    })).await?;
                    append_activity(handle, path, QuotaGuardActivityKind::InterruptAcknowledged, Some(operation_id), Some(&turn), None).await?;
                }
                Err(_) => {
                    Box::pin(apply_event(handle, app, path, bindings, ReducerEvent::InterruptRequestFailed {
                        turn, generation, operation_id, attempt, now_ms: now_ms(),
                    })).await?;
                }
            }
        }
        ReducerEffect::ReconcileThread { turn, generation, operation_id, attempt } => {
            let control = LocalAppServerControl { app: app.clone() };
            match control.read_thread(turn.workspace_id.clone(), turn.thread_id.clone()).await {
                Ok(value) => Box::pin(apply_event(handle, app, path, bindings, ReducerEvent::InterruptReconciled {
                    active_turn_id: active_turn_id(&value), turn, generation, operation_id, attempt, now_ms: now_ms(),
                })).await?,
                Err(reason) => Box::pin(apply_event(handle, app, path, bindings, ReducerEvent::InterruptReconcileFailed {
                    turn, generation, operation_id, attempt, reason, now_ms: now_ms(),
                })).await?,
            }
        }
        ReducerEffect::Notify { episode } => {
            let (window_name, observed_percent, threshold_percent, reset_time) = {
                let runtime = handle.runtime().await;
                let account = runtime.account.as_ref();
                match &episode {
                    crate::shared::quota_guard::model::EpisodeKey::HardLimit { .. } => {
                        ("hard limit", 100, 100, "after the next verified reset".to_string())
                    }
                    crate::shared::quota_guard::model::EpisodeKey::Threshold { window, threshold_percent, resets_at, .. } => {
                        let window_name = match window {
                            crate::shared::quota_guard::model::QuotaWindowKind::Primary => "primary window",
                            crate::shared::quota_guard::model::QuotaWindowKind::Secondary => "secondary window",
                            crate::shared::quota_guard::model::QuotaWindowKind::HardLimit => "hard limit",
                        };
                        let observed_percent = account
                            .and_then(|account| account.snapshot.as_ref())
                            .and_then(|snapshot| snapshot.window(*window))
                            .map(|window| window.used_percent)
                            .unwrap_or(*threshold_percent);
                        let reset_time = resets_at.map(|seconds| {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|elapsed| elapsed.as_secs() as i64)
                                .unwrap_or(0);
                            let minutes = ((seconds - now).max(0) + 59) / 60;
                            let (days, hours, mins) = (minutes / 1440, (minutes % 1440) / 60, minutes % 60);
                            if days > 0 { format!("in {days}d {hours}h {mins}m") }
                            else if hours > 0 { format!("in {hours}h {mins}m") }
                            else { format!("in {mins}m") }
                        }).unwrap_or_else(|| "when the quota service reports a reset".into());
                        (window_name, observed_percent, *threshold_percent, reset_time)
                    }
                }
            };
            let body = crate::notifications::quota_guard_breach_body(window_name, observed_percent, threshold_percent, &reset_time);
            match crate::notifications::notify_quota_breach(app, "Quota guard action required", &body, "quota-guard") {
                Ok(()) => append_activity(handle, path, QuotaGuardActivityKind::NotificationSent, None, None, None).await?,
                Err(error) => append_activity(handle, path, QuotaGuardActivityKind::NotificationFailed, None, None, Some(error)).await?,
            }
        }
        ReducerEffect::ScheduleDrain { generation, deadline } => schedule_drain(handle, generation, deadline),
        ReducerEffect::ScheduleVerification { generation, verify_at } => schedule_verification(handle, generation, verify_at),
        ReducerEffect::ScheduleInterruptAck { turn, generation, operation_id, attempt, deadline } =>
            schedule_event(handle, deadline, ActorEvent::InterruptDeadline { turn, generation, operation_id, attempt, acknowledgement: true }),
        ReducerEffect::ScheduleInterruptCompletion { turn, generation, operation_id, attempt, deadline } =>
            schedule_event(handle, deadline, ActorEvent::InterruptDeadline { turn, generation, operation_id, attempt, acknowledgement: false }),
        ReducerEffect::ScheduleStartExpiry { request_id, generation, deadline } =>
            schedule_event(handle, deadline, ActorEvent::StartExpiry { request_id, generation }),
        ReducerEffect::ScheduleProvisionalExpiry { turn, generation, terminal, deadline } =>
            schedule_event(handle, deadline, ActorEvent::ProvisionalExpiry { turn, generation, terminal }),
        ReducerEffect::ReadFullRateLimits => Box::pin(verify_once(handle, app, path, bindings, false)).await?,
        ReducerEffect::VerifyNow => Box::pin(verify_once(handle, app, path, bindings, true)).await?,
    }
    Ok(())
}

fn schedule_event(handle: &QuotaGuardHandle, deadline: i64, event: ActorEvent) {
    if let Some(sender) = handle.inner.sender.lock().expect("quota guard sender lock poisoned").clone() {
        let wait = u64::try_from(deadline.saturating_sub(now_ms())).unwrap_or_default();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            let _ = sender.send(event).await;
        });
    }
}

fn schedule_drain(handle: &QuotaGuardHandle, generation: u64, deadline: i64) {
    schedule_event(handle, deadline, ActorEvent::DrainDeadline { generation, deadline });
}

fn schedule_verification(handle: &QuotaGuardHandle, generation: u64, verify_at: i64) {
    schedule_event(handle, verify_at, ActorEvent::Verify { generation, verify_at });
}

fn schedule_healthy_revalidation(handle: &QuotaGuardHandle, generation: u64, due_at: i64) {
    let mut scheduled = handle.inner.scheduled_healthy_due.lock().expect("healthy timer lock poisoned");
    if scheduled.is_some() {
        return;
    }
    *scheduled = Some(due_at);
    drop(scheduled);
    schedule_event(handle, due_at, ActorEvent::HealthyRevalidate { generation, due_at });
}

fn monitor_polling_enabled(phase: QuotaGuardPhase) -> bool {
    phase != QuotaGuardPhase::Disabled
}

fn monitor_pollable(runtime: &crate::shared::quota_guard::model::QuotaGuardRuntimeState, generation: u64) -> bool {
    runtime.lifecycle_generation == generation
        && runtime.account.as_ref().is_some_and(|account| monitor_polling_enabled(account.phase))
}

async fn schedule_monitor_poll_if_enabled(handle: &QuotaGuardHandle, generation: u64, due_at: i64) -> bool {
    if monitor_pollable(&handle.runtime().await, generation) {
        schedule_healthy_revalidation(handle, generation, due_at);
        true
    } else {
        false
    }
}

async fn verify_once(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, manual: bool) -> Result<(), String> {
    let workspace_id = handle.runtime().await.account.and_then(|account| account.associated_workspace_ids.first().cloned())
        .ok_or_else(|| "quota guard has no associated workspace for verification".to_string())?;
    let control = LocalAppServerControl { app: app.clone() };
    let value = control.read_rate_limits(workspace_id.clone()).await?;
    apply_rate_limits(handle, app, path, bindings, &workspace_id, value, true, manual).await
}

async fn healthy_revalidate(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>) -> Result<(), String> {
    let (workspace_id, expected_account_key, canonical_home) = {
        let runtime = handle.runtime().await;
        let account = runtime.account.ok_or_else(|| "quota guard has no account for identity revalidation".to_string())?;
        let workspace_id = account.associated_workspace_ids.first().cloned()
            .ok_or_else(|| "quota guard has no associated workspace for identity revalidation".to_string())?;
        let canonical_home = bindings.get(&workspace_id).map(|(_, home)| home.clone())
            .ok_or_else(|| "quota guard workspace binding missing for identity revalidation".to_string())?;
        (workspace_id, account.account_key, canonical_home)
    };
    let control = LocalAppServerControl { app: app.clone() };
    let identity = strict_account_identity(&control.read_identity(workspace_id.clone()).await?)
        .ok_or_else(|| "account/read returned no strict identity".to_string())?;
    if account_key(&canonical_home, &identity) != expected_account_key {
        return Err("strict account identity changed during healthy revalidation".into());
    }
    let value = control.read_rate_limits(workspace_id.clone()).await?;
    apply_rate_limits(handle, app, path, bindings, &workspace_id, value, true, false).await
}

async fn handle_command(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, bindings: &mut HashMap<String, (String, String)>, command: QuotaGuardCommand) -> Result<QuotaGuardPublicState, String> {
    match command {
        QuotaGuardCommand::ApplyActionNow => apply_event(handle, app, path, bindings, ReducerEvent::ApplyActionNow { now_ms: now_ms() }).await?,
        QuotaGuardCommand::KeepWaiting => apply_event(handle, app, path, bindings, ReducerEvent::KeepWaiting { now_ms: now_ms() }).await?,
        QuotaGuardCommand::InterruptNow => {
            apply_event(handle, app, path, bindings, ReducerEvent::ForceInterrupt { now_ms: now_ms() }).await?;
        }
        QuotaGuardCommand::VerifyNow => verify_once(handle, app, path, bindings, true).await?,
        QuotaGuardCommand::Rearm => {
            apply_event(handle, app, path, bindings, ReducerEvent::Rearm { now_ms: now_ms() }).await?;
        }
        QuotaGuardCommand::RetryClosed => {
            handle.inner.gate.set_policy(ProcessPolicy::EnabledClosed);
            resume_external_engines(handle, path).await?;
            verify_once(handle, app, path, bindings, true).await?;
        }
    }
    Ok(public_state_with_bindings(handle, bindings).await)
}

fn external_suspension_phase(phase: QuotaGuardPhase) -> bool {
    matches!(phase, QuotaGuardPhase::Closing | QuotaGuardPhase::Interrupting | QuotaGuardPhase::AwaitingDrainDecision | QuotaGuardPhase::Draining | QuotaGuardPhase::Parked | QuotaGuardPhase::VerifyingReset)
}

async fn owned_session_pids(app: &AppHandle) -> HashSet<u32> {
    let sessions = app.state::<AppState>().sessions.lock().await.values().cloned().collect::<Vec<_>>();
    let mut pids = HashSet::new();
    for session in sessions {
        if let Some(pid) = session.child.lock().await.id() { pids.insert(pid); }
    }
    pids
}

async fn push_external_activity(handle: &QuotaGuardHandle, path: &PathBuf, kind: QuotaGuardActivityKind, messages: Vec<String>) -> Result<(), String> {
    if messages.is_empty() { return Ok(()); }
    let mut runtime = handle.inner.runtime.lock().await;
    if let Some(account) = runtime.account.as_mut() {
        for message in messages {
            account.push_activity(QuotaGuardActivityEntry {
                id: None, kind, timestamp: now_ms(), operation_id: None,
                workspace_id: None, thread_id: None, turn_id: None, attempt: None, message: Some(message),
            });
        }
    }
    drop(runtime);
    persist_current(handle, path).await
}

async fn suspend_external_engines(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf, prevent_new_sessions: bool) -> Result<(), String> {
    let prior = handle.runtime().await.account.map(|account| account.suspended_external_engines).unwrap_or_default();
    let (live, stale) = external_suspend::live_entries(&prior);
    let known = live.iter().map(|entry| (entry.pid, entry.process_start_time)).collect::<HashSet<_>>();
    let result = if prevent_new_sessions {
        external_suspend::sweep(owned_session_pids(app).await, &known, now_ms())
    } else {
        external_suspend::SweepResult::default()
    };
    if live != prior || !result.suspended.is_empty() {
        let mut runtime = handle.inner.runtime.lock().await;
        if let Some(account) = runtime.account.as_mut() {
            account.suspended_external_engines = live;
            for engine in &result.suspended {
                if !account.suspended_external_engines.iter().any(|entry| entry.pid == engine.pid && entry.process_start_time == engine.process_start_time) {
                    account.suspended_external_engines.push(engine.clone());
                }
            }
        }
        drop(runtime);
        persist_current(handle, path).await?;
    }
    push_external_activity(handle, path, QuotaGuardActivityKind::ExternalEngineSkipped, stale).await?;
    push_external_activity(handle, path, QuotaGuardActivityKind::ExternalEngineSuspended,
        result.suspended.iter().map(|engine| format!("suspended {} ({})", engine.image_path, engine.pid)).collect()).await?;
    push_external_activity(handle, path, QuotaGuardActivityKind::ExternalEngineSkipped, result.skipped).await
}

async fn resume_external_engines(handle: &QuotaGuardHandle, path: &PathBuf) -> Result<(), String> {
    let entries = handle.runtime().await.account.map(|account| account.suspended_external_engines).unwrap_or_default();
    let (live, stale) = external_suspend::live_entries(&entries);
    let result = external_suspend::resume(&live);
    let resumed = result.resumed.iter().map(|entry| (entry.pid, entry.process_start_time)).collect::<HashSet<_>>();
    let live_ids = live.iter().map(|entry| (entry.pid, entry.process_start_time)).collect::<HashSet<_>>();
    let mut runtime = handle.inner.runtime.lock().await;
    if let Some(account) = runtime.account.as_mut() {
        account.suspended_external_engines.retain(|entry| {
            let identity = (entry.pid, entry.process_start_time);
            // Stale identities and successful resumes are removed. Permission
            // failures remain durable so a later explicit resolve/shutdown can retry.
            live_ids.contains(&identity) && !resumed.contains(&identity)
        });
    }
    drop(runtime);
    persist_current(handle, path).await?;
    push_external_activity(handle, path, QuotaGuardActivityKind::ExternalEngineResumed,
        result.resumed.iter().map(|engine| format!("resumed {} ({})", engine.image_path, engine.pid)).collect()).await?;
    let mut skipped = stale;
    skipped.extend(result.skipped);
    push_external_activity(handle, path, QuotaGuardActivityKind::ExternalEngineSkipped, skipped).await
}

async fn disable_external_suspension_for_active_episode(handle: &QuotaGuardHandle, path: &PathBuf) -> Result<(), String> {
    let mut runtime = handle.inner.runtime.lock().await;
    let Some(policy) = runtime.account.as_mut().and_then(|account| account.episode_policy.as_mut()) else { return Ok(()); };
    if !policy.external_suspend && !policy.prevent_new_sessions { return Ok(()); }
    policy.external_suspend = false;
    policy.prevent_new_sessions = false;
    drop(runtime);
    persist_current(handle, path).await
}

async fn reconcile_startup_external_engines(handle: &QuotaGuardHandle, app: &AppHandle, path: &PathBuf) -> Result<(), String> {
    let runtime = handle.runtime().await;
    let Some(account) = runtime.account else { return Ok(()) };
    let plan = external_suspend::startup_reconcile_plan(
        external_suspension_phase(account.phase),
        account.episode_policy.as_ref().is_some_and(|policy| policy.external_suspend),
        account.episode_policy.as_ref().is_some_and(|policy| policy.prevent_new_sessions),
    );
    if plan.resuspend_persisted {
        let (live, skipped) = external_suspend::live_entries(&account.suspended_external_engines);
        // Shutdown intentionally resumes these processes so the app can exit.
        // A still-active episode must freeze those exact trip-time identities
        // again on rehydrate even when newcomer prevention is disabled.
        let resuspended = external_suspend::resuspend(&live);
        let known = live.iter().map(|entry| (entry.pid, entry.process_start_time)).collect::<HashSet<_>>();
        let newcomers = if plan.sweep_newcomers {
            external_suspend::sweep(owned_session_pids(app).await, &known, now_ms())
        } else {
            external_suspend::SweepResult::default()
        };
        let mut next = handle.inner.runtime.lock().await;
        if let Some(account) = next.account.as_mut() {
            account.suspended_external_engines = live;
            for engine in &newcomers.suspended {
                if !account.suspended_external_engines.iter().any(|entry| entry.pid == engine.pid && entry.process_start_time == engine.process_start_time) {
                    account.suspended_external_engines.push(engine.clone());
                }
            }
        }
        drop(next);
        persist_current(handle, path).await?;
        push_external_activity(handle, path, QuotaGuardActivityKind::ExternalEngineSkipped, skipped).await?;
        push_external_activity(handle, path, QuotaGuardActivityKind::ExternalEngineSuspended,
            resuspended.suspended.iter().chain(newcomers.suspended.iter()).map(|engine| format!("suspended {} ({})", engine.image_path, engine.pid)).collect()).await?;
        let mut skipped = resuspended.skipped;
        skipped.extend(newcomers.skipped);
        push_external_activity(handle, path, QuotaGuardActivityKind::ExternalEngineSkipped, skipped).await
    } else {
        resume_external_engines(handle, path).await
    }
}

pub(crate) async fn resume_external_engines_for_shutdown(handle: &QuotaGuardHandle, path: &PathBuf) {
    let _ = resume_external_engines(handle, path).await;
}

async fn append_activity(handle: &QuotaGuardHandle, path: &PathBuf, kind: QuotaGuardActivityKind, operation_id: Option<u64>, turn: Option<&TurnKey>, message: Option<String>) -> Result<(), String> {
    let mut runtime = handle.inner.runtime.lock().await;
    if let Some(account) = runtime.account.as_mut() {
        account.push_activity(QuotaGuardActivityEntry {
            id: None, kind, timestamp: now_ms(), operation_id,
            workspace_id: turn.map(|value| value.workspace_id.clone()), thread_id: turn.map(|value| value.thread_id.clone()), turn_id: turn.map(|value| value.turn_id.clone()), attempt: None, message,
        });
    }
    drop(runtime);
    persist_current(handle, path).await
}

async fn persist_current(handle: &QuotaGuardHandle, path: &PathBuf) -> Result<(), String> {
    let runtime = handle.runtime().await;
    persist_runtime(path, &runtime)
}

async fn mark_intervention(handle: &QuotaGuardHandle, path: &PathBuf, message: &str) {
    handle.inner.gate.set_policy(ProcessPolicy::EnabledClosed);
    let mut runtime = handle.inner.runtime.lock().await;
    if let Some(account) = runtime.account.as_mut() {
        account.phase = QuotaGuardPhase::InterventionRequired;
        account.last_error = Some(message.to_string());
        account.updated_at = now_ms();
    }
    drop(runtime);
    let _ = persist_current(handle, path).await;
}
async fn public_state_with_bindings(handle: &QuotaGuardHandle, bindings: &HashMap<String, (String, String)>) -> QuotaGuardPublicState {
    let mut workspace_ids = handle.inner.configured_workspaces.lock().await.clone();
    workspace_ids.extend(bindings.keys().cloned());
    let mut admission_by_workspace = BTreeMap::new();
    for workspace_id in workspace_ids {
        let (session_epoch, status) = match bindings.get(&workspace_id) {
            Some((epoch, _)) => (Some(epoch.clone()), handle.inner.gate.status(Some(epoch), &workspace_id)),
            None => (None, handle.inner.gate.status(None, &workspace_id)),
        };
        admission_by_workspace.insert(workspace_id, AdmissionProjection {
            session_epoch,
            open: status.open,
            reason: reason_name(status.reason).to_string(),
        });
    }
    let runtime = handle.runtime().await;
    let Some(account) = runtime.account.as_ref() else {
        return QuotaGuardPublicState {
            account_key: None,
            account_label: None,
            phase: QuotaGuardPhase::Disabled,
            snapshot: None,
            snapshot_fresh: false,
            breached_windows: Vec::new(),
            affected_turns: Vec::new(),
            drain_deadline: None,
            verify_at: None,
            monitor_healthy: true,
            last_error: None,
            suspended_external_engines: Vec::new(),
            activity: Vec::new(),
            admission_by_workspace,
        };
    };
    let mut seen_turns = HashSet::new();
    let mut affected_turns = Vec::new();
    for turn in account.local_turn_registry.iter()
        .chain(account.allowed_drain_turns.iter())
        .chain(account.pending_interrupt_index.values().map(|pending| &pending.turn))
    {
        if seen_turns.insert(turn.stable_id()) {
            affected_turns.push(QuotaGuardPublicTurn {
                workspace_id: turn.workspace_id.clone(),
                thread_id: turn.thread_id.clone(),
                turn_id: turn.turn_id.clone(),
            });
        }
    }
    QuotaGuardPublicState {
        account_key: Some(account.account_key.clone()),
        // Raw account identity is intentionally never persisted or exposed.
        account_label: None,
        phase: account.phase,
        snapshot: account.snapshot.clone(),
        snapshot_fresh: account.snapshot.as_ref().is_some_and(|snapshot| snapshot.is_fresh_at(now_ms())),
        breached_windows: account.breached_windows.iter().copied().collect(),
        affected_turns,
        drain_deadline: account.drain_deadline,
        verify_at: account.verify_at,
        monitor_healthy: account.monitor_healthy,
        last_error: account.last_error.clone(),
        suspended_external_engines: account.suspended_external_engines.iter().map(|engine| QuotaGuardPublicSuspendedEngine {
            pid: engine.pid,
            image_path: engine.image_path.clone(),
            suspended_at: engine.suspended_at,
        }).collect(),
        activity: account.activity_entries.iter().map(|entry| QuotaGuardPublicActivityEntry {
            id: entry.id.clone(),
            kind: entry.kind,
            timestamp: entry.timestamp,
            operation_id: entry.operation_id.map(|value| value.to_string()),
            workspace_id: entry.workspace_id.clone(),
            thread_id: entry.thread_id.clone(),
            turn_id: entry.turn_id.clone(),
            attempt: entry.attempt,
            message: entry.message.clone(),
        }).collect(),
        admission_by_workspace,
    }
}

async fn emit_state(app: &AppHandle, handle: &QuotaGuardHandle) {
    let workspace_ids = app.state::<AppState>().workspaces.lock().await.keys().cloned().collect();
    handle.set_configured_workspaces(workspace_ids).await;
    let _ = app.emit("quota-guard-state-changed", handle.public_state().await);
}


fn nested_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(*key).and_then(Value::as_str).map(ToOwned::to_owned))
        .or_else(|| ["result", "params", "thread", "turn"].iter().find_map(|key| value.get(*key).and_then(|nested| nested_string(nested, keys))))
}

fn response_thread_id(value: &Value) -> Option<String> {
    nested_string(value, &["threadId", "thread_id"])
}

fn active_turn_id(value: &Value) -> Option<String> {
    nested_string(value, &["activeTurnId", "active_turn_id", "turnId", "turn_id"])
}

fn account_key(canonical_home: &str, identity: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(canonical_home.as_bytes());
    digest.update([0]);
    digest.update(identity.as_bytes());
    format!("{:x}", digest.finalize())
}

#[tauri::command]
pub(crate) async fn quota_guard_get_state(state: State<'_, AppState>) -> Result<QuotaGuardPublicState, String> {
    let workspace_ids = state.workspaces.lock().await.keys().cloned().collect();
    state.quota_guard.set_configured_workspaces(workspace_ids).await;
    Ok(state.quota_guard.public_state().await)
}
#[tauri::command]
pub(crate) async fn quota_guard_apply_action_now(state: State<'_, AppState>) -> Result<QuotaGuardPublicState, String> { state.quota_guard.command(QuotaGuardCommand::ApplyActionNow).await }
#[tauri::command]
pub(crate) async fn quota_guard_keep_waiting(state: State<'_, AppState>) -> Result<QuotaGuardPublicState, String> { state.quota_guard.command(QuotaGuardCommand::KeepWaiting).await }
#[tauri::command]
pub(crate) async fn quota_guard_interrupt_now(state: State<'_, AppState>) -> Result<QuotaGuardPublicState, String> { state.quota_guard.command(QuotaGuardCommand::InterruptNow).await }
#[tauri::command]
pub(crate) async fn quota_guard_verify_now(state: State<'_, AppState>) -> Result<QuotaGuardPublicState, String> { state.quota_guard.command(QuotaGuardCommand::VerifyNow).await }
#[tauri::command]
pub(crate) async fn quota_guard_rearm(state: State<'_, AppState>) -> Result<QuotaGuardPublicState, String> { state.quota_guard.command(QuotaGuardCommand::Rearm).await }
#[tauri::command]
pub(crate) async fn quota_guard_resolve_intervention(resolution: String, state: State<'_, AppState>) -> Result<QuotaGuardPublicState, String> {
    match resolution.as_str() {
        "disableGuard" => {
            crate::settings::disable_quota_guard_and_open(state.inner()).await?;
            Ok(state.quota_guard.public_state().await)
        }
        "retryClosed" => state.quota_guard.command(QuotaGuardCommand::RetryClosed).await,
        _ => Err("unsupported quota guard resolution".into()),
    }
}

pub(crate) fn quota_guard_blocked_error(phase: QuotaGuardPhase, verify_at: Option<i64>) -> String {
    format!("{BLOCKED_PREFIX}|state={}|verifyAt={}", phase_name(phase), verify_at.map(|value| value.to_string()).unwrap_or_default())
}
fn phase_name(phase: QuotaGuardPhase) -> &'static str { match phase { QuotaGuardPhase::Disabled => "disabled", QuotaGuardPhase::Monitoring => "monitoring", QuotaGuardPhase::RevalidatingIdentity => "revalidatingIdentity", QuotaGuardPhase::Closing => "closing", QuotaGuardPhase::Draining => "draining", QuotaGuardPhase::AwaitingDrainDecision => "awaitingDrainDecision", QuotaGuardPhase::Interrupting => "interrupting", QuotaGuardPhase::Parked => "parked", QuotaGuardPhase::VerifyingReset => "verifyingReset", QuotaGuardPhase::Ready => "ready", QuotaGuardPhase::InterventionRequired => "interventionRequired" } }
#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::quota_guard::gate::ProcessGate;
    use crate::shared::quota_guard::model::{AccountRuntime, QuotaGuardRuntimeState, QuotaWindowKind, RateLimitSnapshot, RateLimitWindow};
    use crate::shared::quota_guard::persistence::{load_runtime, persist_runtime, LoadRuntime};
    use crate::types::{QuotaAction, QuotaGuardSettings};

    #[test]
    fn strict_identity_accepts_only_account_fields_through_known_envelopes() {
        assert_eq!(
            strict_account_identity(&serde_json::json!({
                "id": 42,
                "result": { "account": { "email": "Member@Example.test", "planType": "pro" } }
            })),
            Some("member@example.test".into()),
        );
        assert_eq!(
            strict_account_identity(&serde_json::json!({
                "id": "member@example.test",
                "result": { "requestId": "unrelated" }
            })),
            None,
        );
        assert_eq!(
            strict_account_identity(&serde_json::json!({
                "metadata": { "email": "member@example.test" }
            })),
            None,
        );
    }

    #[test]
    fn rapid_threshold_changes_apply_only_the_final_settled_value() {
        tauri::async_runtime::block_on(async {
            let handle = QuotaGuardHandle::default();
            let base = QuotaGuardSettings::default();
            handle.inner.runtime.lock().await.effective_settings = Some(base.clone());

            let mut first = base.clone();
            first.primary_threshold_percent = 27;
            first.secondary_threshold_percent = 27;
            let (_, first_due) = prepare_settings_change(&handle, SettingsChanged { previous: base.clone(), updated: first.clone() }, 1_000).await;
            assert_eq!(first_due, Some(11_000));

            let mut second = first.clone();
            second.primary_threshold_percent = 28;
            second.secondary_threshold_percent = 28;
            let (_, second_due) = prepare_settings_change(&handle, SettingsChanged { previous: first, updated: second.clone() }, 1_100).await;
            assert_eq!(second_due, Some(11_100));

            let mut final_draft = second.clone();
            final_draft.primary_threshold_percent = 34;
            final_draft.secondary_threshold_percent = 34;
            let (_, final_due) = prepare_settings_change(&handle, SettingsChanged { previous: second, updated: final_draft }, 1_200).await;
            assert_eq!(final_due, Some(11_200));
            assert!(take_settled_threshold_change(&handle, 11_000).await.unwrap().is_none());
            assert!(take_settled_threshold_change(&handle, 11_100).await.unwrap().is_none());

            let applied = take_settled_threshold_change(&handle, 11_200).await.unwrap().expect("final threshold change applies once");
            assert_eq!(applied.previous.primary_threshold_percent, 90);
            assert_eq!(applied.updated.primary_threshold_percent, 34);
            assert!(handle.runtime().await.pending_thresholds.is_none());
        });
    }

    #[test]
    fn threshold_change_during_settle_window_resets_the_timer() {
        tauri::async_runtime::block_on(async {
            let handle = QuotaGuardHandle::default();
            let base = QuotaGuardSettings::default();
            handle.inner.runtime.lock().await.effective_settings = Some(base.clone());
            let mut first = base.clone();
            first.primary_threshold_percent = 70;
            let (_, first_due) = prepare_settings_change(&handle, SettingsChanged { previous: base, updated: first.clone() }, 500).await;
            let mut second = first.clone();
            second.primary_threshold_percent = 71;
            let (_, second_due) = prepare_settings_change(&handle, SettingsChanged { previous: first, updated: second }, 9_999).await;

            assert_eq!(first_due, Some(10_500));
            assert_eq!(second_due, Some(19_999));
            assert!(take_settled_threshold_change(&handle, 10_500).await.unwrap().is_none());
            assert_eq!(handle.runtime().await.effective_settings.unwrap().primary_threshold_percent, 90);
        });
    }

    #[test]
    fn armed_toggle_bypasses_threshold_settle_debounce() {
        tauri::async_runtime::block_on(async {
            let handle = QuotaGuardHandle::default();
            let base = QuotaGuardSettings::default();
            handle.inner.runtime.lock().await.effective_settings = Some(base.clone());
            let mut disarmed = base.clone();
            disarmed.armed = false;

            let (change, due) = prepare_settings_change(&handle, SettingsChanged { previous: base, updated: disarmed }, 1_000).await;
            assert_eq!(due, None);
            assert!(!change.updated.armed);
            assert!(handle.runtime().await.pending_thresholds.is_none());
        });
    }

    #[test]
    fn startup_restores_snapshot_for_first_live_threshold_crossing() {
        let mut settings = QuotaGuardSettings::default();
        settings.enabled = true;
        settings.primary_threshold_percent = 47;
        settings.action = QuotaAction::InterruptImmediately;
        settings.external_suspend = true;

        let mut persisted = QuotaGuardRuntimeState::default();
        let mut account = AccountRuntime::new("account".into(), 10_000);
        account.phase = QuotaGuardPhase::Ready;
        account.snapshot = Some(RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 46,
                window_duration_mins: Some(60),
                resets_at: Some(2_000),
            }),
            secondary: None,
            credits: None,
            plan_type: None,
            rate_limit_reached_type: None,
            observed_at: 10_000,
        });
        persisted.account = Some(account);

        // This stale value is possible after a completed debounce. It must not
        // replace the saved app settings on the next launch.
        let mut stale_effective_settings = settings.clone();
        stale_effective_settings.primary_threshold_percent = 90;
        persisted.effective_settings = Some(stale_effective_settings);

        let recovered = recover(true, LoadRuntime::Valid(persisted), 10_001);
        let runtime = restore_runtime_for_startup(recovered.state, &settings);
        let effective_settings = runtime.effective_settings.clone().expect("startup settings");
        assert_eq!(effective_settings.primary_threshold_percent, 47);
        assert_eq!(
            runtime.account.as_ref().and_then(|account| account.snapshot.as_ref())
                .and_then(|snapshot| snapshot.primary.as_ref())
                .map(|window| window.used_percent),
            Some(46),
        );

        let live = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 47,
                window_duration_mins: Some(60),
                resets_at: Some(2_000),
            }),
            secondary: None,
            credits: None,
            plan_type: None,
            rate_limit_reached_type: None,
            observed_at: 10_001,
        };
        let (runtime, effects) = reduce(
            runtime,
            ReducerEvent::Snapshot {
                snapshot: live,
                full_read: true,
                verification: false,
                now_ms: 10_001,
            },
            &effective_settings,
        );

        assert_eq!(runtime.account.map(|account| account.phase), Some(QuotaGuardPhase::Closing));
        assert!(effects.contains(&ReducerEffect::SuspendExternalEngines));
    }

    #[test]
    fn parked_account_schedules_periodic_rate_limit_read() {
        tauri::async_runtime::block_on(async {
            let handle = QuotaGuardHandle::default();
            let (sender, mut receiver) = mpsc::channel(1);
            *handle.inner.sender.lock().expect("sender lock") = Some(sender);
            let generation = 7;
            let mut account = AccountRuntime::new("account".into(), 1);
            account.phase = QuotaGuardPhase::Parked;
            {
                let mut runtime = handle.inner.runtime.lock().await;
                runtime.lifecycle_generation = generation;
                runtime.account = Some(account);
            }

            let due_at = now_ms();
            assert!(schedule_monitor_poll_if_enabled(&handle, generation, due_at).await);
            assert!(matches!(
                tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await,
                Ok(Some(ActorEvent::HealthyRevalidate { generation: observed_generation, due_at: observed_due_at }))
                    if observed_generation == generation && observed_due_at == due_at
            ));
        });
    }

    #[test]
    fn monitor_polling_remains_enabled_for_every_active_phase() {
        for phase in [
            QuotaGuardPhase::Monitoring,
            QuotaGuardPhase::RevalidatingIdentity,
            QuotaGuardPhase::Closing,
            QuotaGuardPhase::Draining,
            QuotaGuardPhase::AwaitingDrainDecision,
            QuotaGuardPhase::Interrupting,
            QuotaGuardPhase::Parked,
            QuotaGuardPhase::VerifyingReset,
            QuotaGuardPhase::Ready,
            QuotaGuardPhase::InterventionRequired,
        ] {
            assert!(monitor_polling_enabled(phase), "{phase:?} must keep polling rate limits");
        }
        assert!(!monitor_polling_enabled(QuotaGuardPhase::Disabled));
    }
    #[test]
    fn non_enable_action_change_keeps_breached_notify_only_monitoring_open_until_apply() {
        tauri::async_runtime::block_on(async {
            let handle = QuotaGuardHandle::default();
            let mut account = AccountRuntime::new("account".into(), 1);
            account.phase = QuotaGuardPhase::Monitoring;
            account.breached_windows.insert(QuotaWindowKind::Primary);
            account.snapshot = Some(RateLimitSnapshot {
                primary: Some(RateLimitWindow { used_percent: 90, window_duration_mins: None, resets_at: Some(60) }),
                secondary: None, credits: None, plan_type: None, rate_limit_reached_type: None, observed_at: 1,
            });
            handle.inner.runtime.lock().await.account = Some(account);
            let mut bindings = HashMap::new();
            bindings.insert("workspace".into(), ("epoch".into(), "home".into()));
            handle.inner.gate.register_closed_epoch("epoch".into(), "workspace".into());

            synchronize_gate_with_runtime(&handle, &bindings).await;
            assert_eq!(handle.inner.gate.policy(), ProcessPolicy::EnabledOpen);
            assert!(handle.inner.gate.status(Some("epoch"), "workspace").open);

            let mut settings = QuotaGuardSettings::default();
            settings.action = QuotaAction::InterruptImmediately;
            let runtime = handle.runtime().await;
            let (_, effects) = reduce(runtime, ReducerEvent::ApplyActionNow { now_ms: 2 }, &settings);
            assert!(effects.iter().any(|effect| matches!(effect, ReducerEffect::SetProcessClosed)));
            close_enforcement_before_persistence(&handle, &effects);
            assert_eq!(handle.inner.gate.policy(), ProcessPolicy::EnabledClosed);
        });
    }


    #[test]
    fn actor_smoke_persists_closed_gate_before_exact_interrupt() {
        let mut settings = QuotaGuardSettings::default();
        settings.action = QuotaAction::InterruptImmediately;
        let (mut runtime, _) = reduce(QuotaGuardRuntimeState::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 1 }, &settings);
        runtime.account.as_mut().unwrap().local_turn_registry.push(TurnKey { session_epoch: "epoch".into(), workspace_id: "workspace".into(), thread_id: "thread".into(), turn_id: "turn".into() });
        let baseline = RateLimitSnapshot { primary: Some(RateLimitWindow { used_percent: 89, window_duration_mins: None, resets_at: Some(60) }), secondary: None, credits: None, plan_type: None, rate_limit_reached_type: None, observed_at: 1 };
        let (runtime, _) = reduce(runtime, ReducerEvent::Snapshot { snapshot: baseline, full_read: true, verification: false, now_ms: 1 }, &settings);
        let snapshot = RateLimitSnapshot { primary: Some(RateLimitWindow { used_percent: 100, window_duration_mins: None, resets_at: Some(60) }), secondary: None, credits: None, plan_type: None, rate_limit_reached_type: Some("hard".into()), observed_at: 2 };
        let (runtime, effects) = reduce(runtime, ReducerEvent::Snapshot { snapshot, full_read: true, verification: false, now_ms: 2 }, &settings);
        assert!(effects.contains(&ReducerEffect::SetProcessClosed));
        let gate = ProcessGate::default();
        gate.set_policy(ProcessPolicy::EnabledClosed);
        let path = std::env::temp_dir().join(format!("quota-guard-actor-smoke-{}.json", std::process::id()));
        persist_runtime(&path, &runtime).unwrap();
        assert_eq!(gate.policy(), ProcessPolicy::EnabledClosed);
        assert!(matches!(load_runtime(&path, 2), LoadRuntime::Valid(saved) if matches!(saved.account.as_ref().map(|account| account.phase), Some(QuotaGuardPhase::Closing))));
        let generation = runtime.lifecycle_generation;
        let (_, final_effects) = reduce(runtime, ReducerEvent::FinalizeClosedEpisode { transition_id: generation, now_ms: 1 }, &settings);
        assert!(final_effects.iter().any(|effect| matches!(effect, ReducerEffect::Interrupt { turn, .. } if turn.workspace_id == "workspace" && turn.thread_id == "thread" && turn.turn_id == "turn")));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enforcing_close_blocks_post_snapshot_admission_and_defers_finalization() {
        tauri::async_runtime::block_on(async {
            let handle = QuotaGuardHandle::default();
            let (sender, mut receiver) = mpsc::channel(1);
            *handle.inner.sender.lock().expect("sender lock") = Some(sender);
            handle.inner.gate.set_policy(ProcessPolicy::EnabledOpen);
            handle.inner.gate.register_closed_epoch("epoch".into(), "workspace".into());
            handle.inner.gate.set_epoch_open("epoch", "workspace", true);
            let admitted_before_close = handle.inner.gate.admit(Some("epoch"), "workspace").expect("pre-close admission");

            close_enforcement_before_persistence(
                &handle,
                &[
                    ReducerEffect::SetProcessClosed,
                    ReducerEffect::FinalizeClosedEpisode { transition_id: 7 },
                ],
            );
            assert_eq!(handle.inner.gate.policy(), ProcessPolicy::EnabledClosed);
            assert!(handle.inner.gate.admit(Some("epoch"), "workspace").is_err(), "a post-snapshot request cannot become AllowOnBind");

            enqueue_finalization_after_admissions(&handle, 7).expect("waiter is queued without blocking the actor");
            assert!(receiver.try_recv().is_err(), "finalization waits outside the actor while a pre-close request persists its fact");
            drop(admitted_before_close);
            let event = tokio::time::timeout(std::time::Duration::from_millis(100), receiver.recv())
                .await
                .expect("finalization is queued after admission drains")
                .expect("actor sender remains open");
            assert!(matches!(event, ActorEvent::FinalizeClosedEpisode { generation: 7 }));
        });
    }

    #[test]
    fn usage_limit_terminal_orders_full_read_before_closing_finalization() {
        let mut settings = QuotaGuardSettings::default();
        settings.action = QuotaAction::InterruptImmediately;
        let (mut runtime, _) = reduce(QuotaGuardRuntimeState::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 1 }, &settings);
        let turn = TurnKey { session_epoch: "epoch".into(), workspace_id: "workspace".into(), thread_id: "thread".into(), turn_id: "turn".into() };
        runtime.account.as_mut().expect("account").local_turn_registry.push(turn.clone());

        let (_, effects) = reduce(runtime, ReducerEvent::TurnTerminal {
            turn,
            status: "failed".into(),
            error: Some(serde_json::json!({"error":{"codexErrorInfo":"usageLimitExceeded"}})),
            now_ms: 2,
        }, &settings);

        assert!(matches!(
            effects.as_slice(),
            [
                ReducerEffect::SetProcessClosed,
                ReducerEffect::ReadFullRateLimits,
                ReducerEffect::FinalizeClosedEpisode { .. },
            ]
        ));
    }

    #[test]
    fn public_state_serializes_every_frontend_contract_field() {
        tauri::async_runtime::block_on(async {
            let handle = QuotaGuardHandle::default();
            let mut account = AccountRuntime::new("hashed-account".into(), 1);
            account.snapshot = Some(RateLimitSnapshot {
                primary: Some(RateLimitWindow { used_percent: 20, window_duration_mins: Some(60), resets_at: Some(100) }),
                secondary: None, credits: None, plan_type: None,
                rate_limit_reached_type: None, observed_at: now_ms(),
            });
            handle.inner.runtime.lock().await.account = Some(account);
            let value = serde_json::to_value(public_state_with_bindings(&handle, &HashMap::new()).await).unwrap();
            for key in ["accountKey", "accountLabel", "phase", "snapshot", "snapshotFresh", "breachedWindows", "affectedTurns", "drainDeadline", "verifyAt", "monitorHealthy", "lastError", "activity", "admissionByWorkspace"] {
                assert!(value.get(key).is_some(), "missing public state key {key}");
            }
            assert_eq!(value["accountKey"], "hashed-account");
            assert!(value["activity"].as_array().is_some());
            assert!(value["admissionByWorkspace"].as_object().is_some());
        });
    }
}
