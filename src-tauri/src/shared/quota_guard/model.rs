pub(crate) use crate::types::QuotaAction;
use crate::types::QuotaGuardSettings;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum QuotaGuardPhase {
    Disabled,
    Monitoring,
    Tripped,
    InterventionRequired,
}

/// Persisted states from the reset-verification era are intentionally mapped
/// once at load time; runtime code only works with the current phases.
impl<'de> Deserialize<'de> for QuotaGuardPhase {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "disabled" => Ok(Self::Disabled),
            "monitoring" | "ready" | "revalidatingIdentity" => Ok(Self::Monitoring),
            "tripped" | "interrupting" | "parked" | "verifyingReset" => Ok(Self::Tripped),
            "interventionRequired" => Ok(Self::InterventionRequired),
            _ => Err(serde::de::Error::unknown_variant(
                &value,
                &[
                    "disabled", "monitoring", "tripped", "interventionRequired",
                    "ready", "revalidatingIdentity", "interrupting", "parked", "verifyingReset",
                ],
            )),
        }
    }
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
    #[serde(rename = "resetsAt")]
    pub(crate) reset_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RateLimitSnapshot {
    pub(crate) primary: Option<RateLimitWindow>,
    pub(crate) secondary: Option<RateLimitWindow>,
    pub(crate) credits: Option<serde_json::Value>,
    pub(crate) plan_type: Option<String>,
    pub(crate) rate_limit_reached_type: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SuspendedExternalEngine {
    pub(crate) pid: u32,
    pub(crate) process_start_time: u64,
    pub(crate) image_path: String,
    pub(crate) suspended_at: i64,
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
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.session_epoch, self.workspace_id, self.thread_id, self.turn_id
        )
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
    HardLimit {
        account_key: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EpisodePolicy {
    pub(crate) action: QuotaAction,
    pub(crate) external_suspend: bool,
    pub(crate) prevent_new_sessions: bool,
    pub(crate) reset_grace_minutes: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingInterrupt {
    pub(crate) turn: TurnKey,
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
    InterruptOnBind,
}
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
    pub(crate) generation: u64,
    pub(crate) observed_at: i64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalObservation {
    pub(crate) turn: TurnKey,
    pub(crate) generation: u64,
    pub(crate) status: String,
    pub(crate) error: Option<serde_json::Value>,
    pub(crate) observed_at: i64,
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
    ExternalEngineSuspended,
    ExternalEngineResumed,
    ExternalEngineSkipped,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountRuntime {
    pub(crate) account_key: String,
    pub(crate) phase: QuotaGuardPhase,
    pub(crate) snapshot: Option<RateLimitSnapshot>,
    pub(crate) associated_workspace_ids: Vec<String>,
    pub(crate) local_turn_registry: Vec<TurnKey>,
    #[serde(default)]
    pub(crate) activity_entries: Vec<QuotaGuardActivityEntry>,
    #[serde(default)]
    pub(crate) suspended_external_engines: Vec<SuspendedExternalEngine>,
    pub(crate) monitor_healthy: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) updated_at: i64,
    #[serde(skip)]
    pub(crate) breached_windows: std::collections::BTreeSet<QuotaWindowKind>,
    #[serde(skip)]
    pub(crate) fired_episodes: std::collections::BTreeSet<EpisodeKey>,
    #[serde(skip)]
    pub(crate) episode_policy: Option<EpisodePolicy>,
    #[serde(skip)]
    pub(crate) pending_local_starts: BTreeMap<u64, PendingLocalStart>,
    #[serde(skip)]
    pub(crate) unmatched_started_turns: Vec<UnmatchedStartedTurn>,
    #[serde(skip)]
    pub(crate) terminal_observations: Vec<TerminalObservation>,
    #[serde(skip)]
    pub(crate) pending_interrupt_index: BTreeMap<String, PendingInterrupt>,
    #[serde(default)]
    pub(crate) fire_at_or_above_on_next_snapshot: bool,
}
impl AccountRuntime {
    pub(crate) fn new(account_key: String, now_ms: i64) -> Self {
        Self {
            account_key,
            phase: QuotaGuardPhase::Monitoring,
            snapshot: None,
            associated_workspace_ids: vec![],
            local_turn_registry: vec![],
            activity_entries: vec![],
            suspended_external_engines: vec![],
            monitor_healthy: true,
            last_error: None,
            updated_at: now_ms,
            breached_windows: Default::default(),
            fired_episodes: Default::default(),
            episode_policy: None,
            pending_local_starts: Default::default(),
            unmatched_started_turns: vec![],
            terminal_observations: vec![],
            pending_interrupt_index: Default::default(),
            fire_at_or_above_on_next_snapshot: false,
        }
    }
    pub(crate) fn push_activity(&mut self, activity: QuotaGuardActivityEntry) {
        self.activity_entries.push(activity);
        if self.activity_entries.len() > 100 {
            self.activity_entries
                .drain(..self.activity_entries.len() - 100);
        }
    }
    pub(crate) fn insert_pending_interrupt(&mut self, pending: PendingInterrupt) {
        self.pending_interrupt_index
            .insert(pending.turn.stable_id(), pending);
    }
    pub(crate) fn remove_pending_interrupt(&mut self, turn: &TurnKey) {
        self.pending_interrupt_index.remove(&turn.stable_id());
    }
    pub(crate) fn push_unmatched_started_turn(
        &mut self,
        value: UnmatchedStartedTurn,
    ) -> Result<(), String> {
        self.unmatched_started_turns.push(value);
        Ok(())
    }
    pub(crate) fn push_terminal_observation(
        &mut self,
        value: TerminalObservation,
    ) -> Result<(), String> {
        self.terminal_observations.push(value);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingThresholdSettings {
    pub(crate) primary_threshold_percent: u8,
    pub(crate) secondary_threshold_percent: u8,
    pub(crate) settles_at: i64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaGuardRuntimeState {
    pub(crate) schema_version: u32,
    pub(crate) lifecycle_generation: u64,
    #[serde(default)]
    pub(crate) next_operation_id: u64,
    pub(crate) account: Option<AccountRuntime>,
    #[serde(default)]
    pub(crate) effective_settings: Option<QuotaGuardSettings>,
    #[serde(skip)]
    pub(crate) pending_thresholds: Option<PendingThresholdSettings>,
}
impl Default for QuotaGuardRuntimeState {
    fn default() -> Self {
        Self {
            schema_version: 2,
            lifecycle_generation: 0,
            next_operation_id: 0,
            account: None,
            effective_settings: None,
            pending_thresholds: None,
        }
    }
}

#[cfg(test)]
mod phase_deserialization_tests {
    use super::QuotaGuardPhase;

    #[test]
    fn legacy_persisted_phases_map_to_current_phases() {
        let healthy: QuotaGuardPhase = serde_json::from_str("\"ready\"").unwrap();
        let frozen: QuotaGuardPhase = serde_json::from_str("\"parked\"").unwrap();
        assert_eq!(healthy, QuotaGuardPhase::Monitoring);
        assert_eq!(frozen, QuotaGuardPhase::Tripped);
    }
}
