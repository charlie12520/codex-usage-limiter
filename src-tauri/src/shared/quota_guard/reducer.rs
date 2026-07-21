use super::model::{
    AccountRuntime, EpisodeKey, PendingLocalStart, QuotaAction, QuotaGuardPhase,
    QuotaGuardRuntimeState, QuotaWindowKind, RateLimitSnapshot, TurnKey,
};
use crate::types::QuotaGuardSettings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReducerEffect {
    SetProcessClosed,
    SetProcessOpen,
    SuspendExternalEngines,
    MaintainExternalEngineSuspension {
        prevent_new_sessions: bool,
    },
    ResumeExternalEngines,
    Notify {
        episode: EpisodeKey,
    },
    PersistDisarmed,
    RaiseThresholds {
        used_percent: u8,
    },
    Interrupt {
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
    },
    ReconcileThread {
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
    },
    FinalizeClosedEpisode {
        transition_id: u64,
    },
    ScheduleVerification {
        generation: u64,
        verify_at: i64,
    },
    ScheduleInterruptAck {
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
        deadline: i64,
    },
    ScheduleInterruptCompletion {
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
        deadline: i64,
    },
    ScheduleStartExpiry {
        request_id: u64,
        generation: u64,
        deadline: i64,
    },
    ScheduleProvisionalExpiry {
        turn: TurnKey,
        generation: u64,
        terminal: bool,
        deadline: i64,
    },
    ReadFullRateLimits,
    VerifyNow,
}
#[derive(Debug, Clone)]
pub(crate) enum ReducerEvent {
    Enable {
        account_key: String,
        now_ms: i64,
    },
    Disable {
        now_ms: i64,
    },
    SettingsChanged {
        thresholds_changed: bool,
    },
    Snapshot {
        snapshot: RateLimitSnapshot,
        full_read: bool,
        verification: bool,
        now_ms: i64,
    },
    TurnStarted {
        turn: TurnKey,
        now_ms: i64,
    },
    TurnTerminal {
        turn: TurnKey,
        status: String,
        error: Option<serde_json::Value>,
        now_ms: i64,
    },
    Resume {
        now_ms: i64,
    },
    ApplyActionNow {
        now_ms: i64,
    },
    Rearm {
        now_ms: i64,
    },
    FinalizeClosedEpisode {
        transition_id: u64,
        now_ms: i64,
    },
    InterruptAcknowledged {
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
        now_ms: i64,
    },
    InterruptRequestFailed {
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
        now_ms: i64,
    },
    InterruptDeadline {
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
        acknowledgement: bool,
        now_ms: i64,
    },
    InterruptReconciled {
        active_turn_id: Option<String>,
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
        now_ms: i64,
    },
    InterruptReconcileFailed {
        turn: TurnKey,
        generation: u64,
        operation_id: u64,
        attempt: u8,
        reason: String,
        now_ms: i64,
    },
    PendingStartExpired {
        request_id: u64,
        generation: u64,
        now_ms: i64,
    },
    ProvisionalExpired {
        turn: TurnKey,
        generation: u64,
        terminal: bool,
        now_ms: i64,
    },
    RehydratePendingInterrupts {
        now_ms: i64,
    },
    PendingStartRecorded {
        start: PendingLocalStart,
        now_ms: i64,
    },
    PendingStartFailed {
        request_id: u64,
        generation: u64,
        now_ms: i64,
    },
    StartResponse {
        request_id: u64,
        session_epoch: String,
        workspace_id: String,
        thread_id: Option<String>,
        now_ms: i64,
    },
}

fn threshold(settings: &QuotaGuardSettings, kind: QuotaWindowKind) -> u8 {
    match kind {
        QuotaWindowKind::Primary => settings.primary_threshold_percent,
        QuotaWindowKind::Secondary => settings.secondary_threshold_percent,
        QuotaWindowKind::HardLimit => 100,
    }
}
fn fired(
    account: &AccountRuntime,
    previous: Option<&RateLimitSnapshot>,
    settings: &QuotaGuardSettings,
    force: bool,
) -> Option<(QuotaWindowKind, u8)> {
    [QuotaWindowKind::Primary, QuotaWindowKind::Secondary]
        .into_iter()
        .find_map(|kind| {
            let current = account.snapshot.as_ref()?.window(kind)?;
            let floor = threshold(settings, kind);
            let was_below = previous
                .and_then(|snapshot| snapshot.window(kind))
                .map(|window| window.used_percent < floor)
                .unwrap_or(force);
            (current.used_percent >= floor && was_below).then_some((kind, floor))
        })
}
fn trip(
    account: &mut AccountRuntime,
    settings: &QuotaGuardSettings,
    window: QuotaWindowKind,
    floor: u8,
    effects: &mut Vec<ReducerEffect>,
) {
    match settings.action {
        QuotaAction::NotifyOnly => effects.push(ReducerEffect::Notify {
            episode: EpisodeKey::Threshold {
                account_key: account.account_key.clone(),
                window,
                threshold_percent: floor,
                resets_at: None,
            },
        }),
        QuotaAction::Interrupt => {
            account.phase = QuotaGuardPhase::Tripped;
            effects.push(ReducerEffect::SetProcessClosed);
            effects.push(ReducerEffect::SuspendExternalEngines);
            for turn in account.local_turn_registry.clone() {
                effects.push(ReducerEffect::Interrupt {
                    turn,
                    generation: 0,
                    operation_id: 0,
                    attempt: 1,
                });
            }
            effects.push(ReducerEffect::PersistDisarmed);
            effects.push(ReducerEffect::SetProcessOpen);
        }
        QuotaAction::Block => {
            account.phase = QuotaGuardPhase::Tripped;
            effects.push(ReducerEffect::SetProcessClosed);
            effects.push(ReducerEffect::SuspendExternalEngines);
            for turn in account.local_turn_registry.clone() {
                effects.push(ReducerEffect::Interrupt {
                    turn,
                    generation: 0,
                    operation_id: 0,
                    attempt: 1,
                });
            }
        }
    }
}
pub(crate) fn reduce(
    mut runtime: QuotaGuardRuntimeState,
    event: ReducerEvent,
    settings: &QuotaGuardSettings,
) -> (QuotaGuardRuntimeState, Vec<ReducerEffect>) {
    let mut effects = vec![];
    match event {
        ReducerEvent::Enable {
            account_key,
            now_ms,
        } => {
            runtime.lifecycle_generation = runtime.lifecycle_generation.saturating_add(1);
            let account = runtime
                .account
                .get_or_insert_with(|| AccountRuntime::new(account_key.clone(), now_ms));
            account.account_key = account_key;
            account.phase = QuotaGuardPhase::Monitoring;
            account.updated_at = now_ms;
            effects.push(ReducerEffect::SetProcessOpen);
        }
        ReducerEvent::Disable { now_ms } => {
            if let Some(account) = runtime.account.as_mut() {
                account.phase = QuotaGuardPhase::Disabled;
                account.updated_at = now_ms;
                effects.extend([
                    ReducerEffect::ResumeExternalEngines,
                    ReducerEffect::SetProcessOpen,
                ]);
            }
        }
        ReducerEvent::SettingsChanged { .. } => {}
        ReducerEvent::TurnStarted { turn, now_ms } => {
            if let Some(account) = runtime.account.as_mut() {
                if account
                    .local_turn_registry
                    .iter()
                    .all(|known| known.stable_id() != turn.stable_id())
                {
                    account.local_turn_registry.push(turn);
                }
                account.updated_at = now_ms;
            }
        }
        ReducerEvent::TurnTerminal { turn, now_ms, .. } => {
            if let Some(account) = runtime.account.as_mut() {
                account
                    .local_turn_registry
                    .retain(|known| known.stable_id() != turn.stable_id());
                account.updated_at = now_ms;
            }
        }
        ReducerEvent::Resume { now_ms } => {
            if let Some(account) = runtime.account.as_mut() {
                account.phase = QuotaGuardPhase::Monitoring;
                account.updated_at = now_ms;
                effects.extend([
                    ReducerEffect::ResumeExternalEngines,
                    ReducerEffect::SetProcessOpen,
                ]);
            }
        }
        ReducerEvent::Snapshot {
            snapshot, now_ms, ..
        } => {
            if let Some(account) = runtime.account.as_mut() {
                let previous = account.snapshot.clone();
                account.snapshot = Some(snapshot);
                account.updated_at = now_ms;
                if !settings.armed {
                    let used = [QuotaWindowKind::Primary, QuotaWindowKind::Secondary]
                        .into_iter()
                        .filter_map(|kind| {
                            account
                                .snapshot
                                .as_ref()
                                .and_then(|value| value.window(kind))
                                .map(|window| window.used_percent)
                        })
                        .max()
                        .unwrap_or_default();
                    let floor = settings
                        .primary_threshold_percent
                        .min(settings.secondary_threshold_percent);
                    if used > floor {
                        effects.push(ReducerEffect::RaiseThresholds { used_percent: used });
                    }
                    return (runtime, effects);
                }
                if account.phase == QuotaGuardPhase::Tripped
                    && matches!(settings.action, QuotaAction::Block)
                {
                    effects.push(ReducerEffect::MaintainExternalEngineSuspension {
                        prevent_new_sessions: true,
                    });
                    return (runtime, effects);
                }
                if account.phase == QuotaGuardPhase::Monitoring {
                    if let Some((window, floor)) =
                        fired(account, previous.as_ref(), settings, previous.is_none())
                    {
                        trip(account, settings, window, floor, &mut effects);
                    }
                }
            }
        }
        _ => {}
    }
    (runtime, effects)
}

#[cfg(all(test, any()))]
mod tests {
    use super::*;
    use crate::shared::quota_guard::model::{RateLimitSnapshot, RateLimitWindow};
    use crate::types::QuotaGuardSettings;
    fn snapshot(used: u8) -> RateLimitSnapshot {
        RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: used,
                window_duration_mins: None,
                reset_at: None,
            }),
            secondary: None,
            credits: None,
            plan_type: None,
            rate_limit_reached_type: None,
            observed_at: 0,
        }
    }
    fn enabled(action: QuotaAction) -> (QuotaGuardRuntimeState, QuotaGuardSettings) {
        let mut s = QuotaGuardSettings::default();
        s.enabled = true;
        s.action = action;
        let (r, _) = reduce(
            Default::default(),
            ReducerEvent::Enable {
                account_key: "a".into(),
                now_ms: 0,
            },
            &s,
        );
        (r, s)
    }
    #[test]
    fn notify_crossing_only_notifies() {
        let (r, s) = enabled(QuotaAction::NotifyOnly);
        let (r, _) = reduce(
            r,
            ReducerEvent::Snapshot {
                snapshot: snapshot(89),
                full_read: true,
                now_ms: 1,
            },
            &s,
        );
        let (r, e) = reduce(
            r,
            ReducerEvent::Snapshot {
                snapshot: snapshot(90),
                full_read: true,
                now_ms: 2,
            },
            &s,
        );
        assert!(matches!(e.as_slice(), [ReducerEffect::Notify { .. }]));
        assert_eq!(r.account.unwrap().phase, QuotaGuardPhase::Monitoring);
    }
    #[test]
    fn interrupt_self_disarms_and_block_sweeps() {
        let (r, s) = enabled(QuotaAction::Interrupt);
        let (r, e) = reduce(
            r,
            ReducerEvent::Snapshot {
                snapshot: snapshot(90),
                full_read: true,
                now_ms: 1,
            },
            &s,
        );
        assert!(e.contains(&ReducerEffect::PersistDisarmed));
        assert_eq!(r.account.unwrap().phase, QuotaGuardPhase::Tripped);
        let (r, s) = enabled(QuotaAction::Block);
        let (r, _) = reduce(
            r,
            ReducerEvent::Snapshot {
                snapshot: snapshot(90),
                full_read: true,
                now_ms: 1,
            },
            &s,
        );
        let (_, e) = reduce(
            r,
            ReducerEvent::Snapshot {
                snapshot: snapshot(91),
                full_read: true,
                now_ms: 2,
            },
            &s,
        );
        assert!(
            e.contains(&ReducerEffect::MaintainExternalEngineSuspension {
                prevent_new_sessions: true
            })
        );
    }
    #[test]
    fn disarmed_usage_rides_and_armed_floor_fires() {
        let (r, mut s) = enabled(QuotaAction::NotifyOnly);
        s.armed = false;
        let (_, e) = reduce(
            r,
            ReducerEvent::Snapshot {
                snapshot: snapshot(91),
                full_read: true,
                now_ms: 1,
            },
            &s,
        );
        assert!(e.contains(&ReducerEffect::RaiseThresholds { used_percent: 91 }));
        s.armed = true;
        let (r, _) = enabled(QuotaAction::NotifyOnly);
        let (_, e) = reduce(
            r,
            ReducerEvent::Snapshot {
                snapshot: snapshot(90),
                full_read: true,
                now_ms: 1,
            },
            &s,
        );
        assert!(matches!(e.as_slice(), [ReducerEffect::Notify { .. }]));
    }
}
