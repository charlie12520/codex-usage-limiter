use std::collections::BTreeSet;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub(crate) use crate::types::{DrainTimeoutAction, QuotaAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum QuotaGuardPhase {
    Disabled,
    Monitoring,
    RevalidatingIdentity,
    Closing,
    Draining,
    AwaitingDrainDecision,
    Interrupting,
    Parked,
    VerifyingReset,
    Ready,
    InterventionRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum QuotaWindowKind {
    Primary,
    Secondary,
    HardLimit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RateLimitWindow {
    pub(crate) used_percent: u8,
    pub(crate) window_duration_mins: Option<u64>,
    /// Codex protocol timestamps remain Unix seconds.
    pub(crate) resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RateLimitSnapshot {
    pub(crate) primary: Option<RateLimitWindow>,
    pub(crate) secondary: Option<RateLimitWindow>,
    pub(crate) credits: Option<serde_json::Value>,
    pub(crate) plan_type: Option<String>,
    pub(crate) rate_limit_reached_type: Option<String>,
    /// Local observation time is Unix milliseconds.
    pub(crate) observed_at: i64,
}

impl RateLimitSnapshot {
    pub(crate) fn is_fresh_at(&self, now_ms: i64) -> bool {
        now_ms >= self.observed_at && now_ms.saturating_sub(self.observed_at) <= 600_000
    }

    pub(crate) fn window(&self, kind: QuotaWindowKind) -> Option<&RateLimitWindow> {
        match kind {
            QuotaWindowKind::Primary => self.primary.as_ref(),
            QuotaWindowKind::Secondary => self.secondary.as_ref(),
            QuotaWindowKind::HardLimit => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub(crate) enum EpisodeKey {
    Threshold {
        account_key: String,
        window: QuotaWindowKind,
        threshold_percent: u8,
        resets_at: Option<i64>,
    },
    HardLimit { account_key: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EpisodePolicy {
    pub(crate) action: QuotaAction,
    pub(crate) drain_timeout_minutes: u16,
    pub(crate) drain_timeout_action: DrainTimeoutAction,
    pub(crate) reset_grace_minutes: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnKey {
    pub(crate) session_epoch: String,
    pub(crate) workspace_id: String,
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
}

impl TurnKey {
    pub(crate) fn stable_id(&self) -> String {
        format!("{}\u{1f}{}\u{1f}{}\u{1f}{}", self.session_epoch, self.workspace_id, self.thread_id, self.turn_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingInterrupt {
    pub(crate) turn: TurnKey,
    #[serde(default)]
    pub(crate) generation: u64,
    pub(crate) operation_id: u64,
    pub(crate) attempt: u8,
    pub(crate) acknowledged: bool,
    pub(crate) ack_deadline: i64,
    pub(crate) completion_deadline: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PendingStartDisposition {
    AllowOnBind,
    InterruptOnBind,
}

/// A start request is durable before its JSON is written.  Until exact
/// response/notification correlation proves a turn ID it is not ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingLocalStart {
    pub(crate) request_id: u64,
    pub(crate) session_epoch: String,
    pub(crate) workspace_id: String,
    pub(crate) request_thread_id: Option<String>,
    pub(crate) expected_thread_id: Option<String>,
    pub(crate) request_kind: String,
    #[serde(default)]
    pub(crate) response_thread_id: Option<String>,
    #[serde(default)]
    pub(crate) response_received_at: Option<i64>,
    pub(crate) generation: u64,
    pub(crate) disposition: Option<PendingStartDisposition>,
    pub(crate) registered_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnmatchedStartedTurn {
    pub(crate) turn: TurnKey,
    #[serde(default)]
    pub(crate) generation: u64,
    pub(crate) observed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalObservation {
    pub(crate) turn: TurnKey,
    #[serde(default)]
    pub(crate) generation: u64,
    pub(crate) status: String,
    pub(crate) error: Option<serde_json::Value>,
    pub(crate) observed_at: i64,
}

/// Durable state for the single account guarded by this process.  Collection
/// keys are exact session/workspace/thread/turn triples; their wire form is
/// intentionally never inferred from frontend visibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountRuntime {
    pub(crate) account_key: String,
    pub(crate) phase: QuotaGuardPhase,
    pub(crate) revalidation_return_phase: Option<QuotaGuardPhase>,
    pub(crate) snapshot: Option<RateLimitSnapshot>,
    pub(crate) breached_windows: BTreeSet<QuotaWindowKind>,
    pub(crate) fired_episodes: BTreeSet<EpisodeKey>,
    pub(crate) episode_policy: Option<EpisodePolicy>,
    pub(crate) associated_workspace_ids: Vec<String>,
    pub(crate) local_turn_registry: Vec<TurnKey>,
    #[serde(default)]
    pub(crate) pending_local_starts: BTreeMap<u64, PendingLocalStart>,
    /// Bounded provisional review-start observations; never active ownership.
    #[serde(default)]
    pub(crate) unmatched_started_turns: Vec<UnmatchedStartedTurn>,
    #[serde(default)]
    pub(crate) terminal_observations: Vec<TerminalObservation>,
    #[serde(default)]
    pub(crate) activity_entries: Vec<QuotaGuardActivityEntry>,
    pub(crate) allowed_drain_turns: Vec<TurnKey>,
    /// Canonical exact-turn index. Every interrupt lifecycle transition is
    /// keyed by the immutable session/workspace/thread/turn identity.
    #[serde(default)]
    pub(crate) pending_interrupt_index: BTreeMap<String, PendingInterrupt>,
    pub(crate) drain_deadline: Option<i64>,
    pub(crate) verify_at: Option<i64>,
    pub(crate) monitor_healthy: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaGuardRuntimeState {
    pub(crate) schema_version: u32,
    pub(crate) lifecycle_generation: u64,
    /// Monotonic operation IDs fence effects within a lifecycle generation.
    #[serde(default)]
    pub(crate) next_operation_id: u64,
    pub(crate) account: Option<AccountRuntime>,
}

impl Default for QuotaGuardRuntimeState {
    fn default() -> Self {
        Self { schema_version: 1, lifecycle_generation: 0, next_operation_id: 0, account: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum QuotaGuardActivityKind {
    StateChanged,
    NotificationSent,
    NotificationFailed,
    InterruptRequested,
    InterruptAcknowledged,
    InterruptCompleted,
    MonitorError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaGuardActivityEntry {
    pub(crate) id: Option<String>,
    pub(crate) kind: QuotaGuardActivityKind,
    pub(crate) timestamp: i64,
    pub(crate) operation_id: Option<u64>,
    pub(crate) workspace_id: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) turn_id: Option<String>,
    pub(crate) attempt: Option<u8>,
    pub(crate) message: Option<String>,
}

impl AccountRuntime {
    pub(crate) fn new(account_key: String, now_ms: i64) -> Self {
        Self {
            account_key,
            phase: QuotaGuardPhase::Monitoring,
            revalidation_return_phase: None,
            snapshot: None,
            breached_windows: BTreeSet::new(),
            fired_episodes: BTreeSet::new(),
            episode_policy: None,
            associated_workspace_ids: Vec::new(),
            local_turn_registry: Vec::new(),
            unmatched_started_turns: Vec::new(),
            pending_local_starts: BTreeMap::new(),
            terminal_observations: Vec::new(),
            activity_entries: Vec::new(),
            allowed_drain_turns: Vec::new(),
            pending_interrupt_index: BTreeMap::new(),
            drain_deadline: None,
            verify_at: None,
            monitor_healthy: true,
            last_error: None,
            updated_at: now_ms,
        }
    }

    pub(crate) fn push_activity(&mut self, activity: QuotaGuardActivityEntry) {
        self.activity_entries.push(activity);
        if self.activity_entries.len() > 100 {
            self.activity_entries.drain(..self.activity_entries.len() - 100);
        }
    }
    pub(crate) fn insert_pending_interrupt(&mut self, pending: PendingInterrupt) {
        self.pending_interrupt_index.insert(pending.turn.stable_id(), pending);
    }

    pub(crate) fn push_unmatched_started_turn(&mut self, observation: UnmatchedStartedTurn) -> Result<(), String> {
        if self.unmatched_started_turns.len() >= 32 {
            return Err("unmatched turn observation buffer overflow".into());
        }
        self.unmatched_started_turns.push(observation);
        Ok(())
    }

    pub(crate) fn push_terminal_observation(&mut self, observation: TerminalObservation) -> Result<(), String> {
        if self.terminal_observations.len() >= 32 {
            return Err("terminal observation buffer overflow".into());
        }
        self.terminal_observations.push(observation);
        Ok(())
    }

    pub(crate) fn remove_pending_interrupt(&mut self, turn: &TurnKey) {
        self.pending_interrupt_index.remove(&turn.stable_id());
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountRuntime, QuotaGuardActivityEntry, QuotaGuardActivityKind};

    #[test]
    fn activity_log_retains_only_newest_hundred_entries() {
        let mut account = AccountRuntime::new("account".into(), 0);
        for timestamp in 0..101 {
            account.push_activity(QuotaGuardActivityEntry {
                id: None,
                kind: QuotaGuardActivityKind::StateChanged,
                timestamp,
                operation_id: None,
                workspace_id: None,
                thread_id: None,
                turn_id: None,
                attempt: None,
                message: None,
            });
        }
        assert_eq!(account.activity_entries.len(), 100);
        assert_eq!(account.activity_entries.first().map(|entry| entry.timestamp), Some(1));
    }
}
