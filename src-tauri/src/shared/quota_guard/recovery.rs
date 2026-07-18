use super::gate::ProcessPolicy;
use super::model::{AccountRuntime, QuotaGuardPhase, QuotaGuardRuntimeState};
use super::persistence::LoadRuntime;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecoveryDecision {
    pub(crate) state: QuotaGuardRuntimeState,
    pub(crate) policy: ProcessPolicy,
    pub(crate) requires_bootstrap: bool,
}

/// Applies settings authority before interpreting persisted enforcement state.
/// A disabled setting always wins and never rehydrates stale enforcement.
pub(crate) fn recover(enabled: bool, loaded: LoadRuntime, now_ms: i64) -> RecoveryDecision {
    if !enabled {
        return RecoveryDecision { state: QuotaGuardRuntimeState::default(), policy: ProcessPolicy::DisabledOpen, requires_bootstrap: false };
    }
    match loaded {
        LoadRuntime::Valid(state) if !matches!(state.account.as_ref().map(|account| account.phase), None | Some(QuotaGuardPhase::Disabled)) => RecoveryDecision { state, policy: ProcessPolicy::EnabledClosed, requires_bootstrap: true },
        LoadRuntime::Corrupt { .. } => {
            let mut state = QuotaGuardRuntimeState::default();
            state.account = Some(AccountRuntime::new(String::new(), now_ms));
            if let Some(account) = state.account.as_mut() { account.phase = QuotaGuardPhase::InterventionRequired; account.last_error = Some("quota guard state is corrupt".into()); }
            RecoveryDecision { state, policy: ProcessPolicy::EnabledClosed, requires_bootstrap: false }
        }
        LoadRuntime::Missing | LoadRuntime::Valid(_) => RecoveryDecision { state: QuotaGuardRuntimeState::default(), policy: ProcessPolicy::EnabledClosed, requires_bootstrap: true },
    }
}

#[cfg(test)]
mod tests {
    use super::recover;
    use crate::shared::quota_guard::gate::ProcessPolicy;
    use crate::shared::quota_guard::persistence::LoadRuntime;
    #[test]
    fn disabled_settings_open_and_discard_stale_runtime() { assert_eq!(recover(false, LoadRuntime::Missing, 1).policy, ProcessPolicy::DisabledOpen); }
    #[test]
    fn enabled_missing_runtime_starts_closed() { let result = recover(true, LoadRuntime::Missing, 1); assert_eq!(result.policy, ProcessPolicy::EnabledClosed); assert!(result.requires_bootstrap); }
}
