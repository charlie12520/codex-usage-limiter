use super::model::{
    AccountRuntime, EpisodeKey, PendingLocalStart, QuotaAction, QuotaGuardActivityEntry,
    QuotaGuardActivityKind, QuotaGuardPhase, QuotaGuardRuntimeState, QuotaWindowKind,
    RateLimitSnapshot, TurnKey,
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
    PersistAutoRearm {
        threshold_percent: u8,
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
            let was_below = force
                || previous
                    .and_then(|snapshot| snapshot.window(kind))
                    .map(|window| window.used_percent < floor)
                    .unwrap_or(true);
            (current.used_percent >= floor && was_below).then_some((kind, floor))
        })
}

fn primary_reset_observed(
    previous: Option<&RateLimitSnapshot>,
    current: &RateLimitSnapshot,
) -> bool {
    let Some(previous) = previous.and_then(|snapshot| snapshot.primary.as_ref()) else {
        return false;
    };
    let Some(current) = current.primary.as_ref() else {
        return false;
    };
    match (previous.reset_at, current.reset_at) {
        (Some(before), Some(after)) => after > before,
        _ => previous.used_percent.saturating_sub(current.used_percent) >= 20,
    }
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
                let reset_observed = primary_reset_observed(previous.as_ref(), &snapshot);
                account.snapshot = Some(snapshot);
                account.updated_at = now_ms;
                if reset_observed {
                    if let Some(percent_left) = settings.rearm_after_reset_percent_left {
                        let current_used = account
                            .snapshot
                            .as_ref()
                            .and_then(|value| value.primary.as_ref())
                            .map(|window| window.used_percent)
                            .unwrap_or_default();
                        let threshold_percent = (100 - percent_left).max(current_used);
                        account.phase = QuotaGuardPhase::Monitoring;
                        account.fire_at_or_above_on_next_snapshot =
                            threshold_percent == current_used;
                        account.push_activity(QuotaGuardActivityEntry {
                            id: None,
                            kind: QuotaGuardActivityKind::StateChanged,
                            timestamp: now_ms,
                            operation_id: None,
                            workspace_id: None,
                            thread_id: None,
                            turn_id: None,
                            attempt: None,
                            message: Some(format!(
                                "Automatically rearmed after usage reset at {percent_left}% left."
                            )),
                        });
                        effects.extend([
                            ReducerEffect::ResumeExternalEngines,
                            ReducerEffect::SetProcessOpen,
                            ReducerEffect::PersistAutoRearm { threshold_percent },
                        ]);
                        return (runtime, effects);
                    }
                }
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
                    let force = previous.is_none() || account.fire_at_or_above_on_next_snapshot;
                    account.fire_at_or_above_on_next_snapshot = false;
                    if let Some((window, floor)) =
                        fired(account, previous.as_ref(), settings, force)
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

#[cfg(test)]
mod rearm_after_reset_tests {
    use super::*;

    fn snapshot(used_percent: u8, reset_at: Option<i64>) -> RateLimitSnapshot {
        RateLimitSnapshot {
            primary: Some(super::super::model::RateLimitWindow {
                used_percent,
                window_duration_mins: Some(300),
                reset_at,
            }),
            secondary: None,
            credits: None,
            plan_type: None,
            rate_limit_reached_type: None,
            observed_at: 0,
        }
    }

    fn enabled_runtime(settings: &QuotaGuardSettings) -> QuotaGuardRuntimeState {
        reduce(
            QuotaGuardRuntimeState::default(),
            ReducerEvent::Enable {
                account_key: "account".into(),
                now_ms: 0,
            },
            settings,
        )
        .0
    }

    #[test]
    fn reset_rearms_resumes_and_persists_the_requested_floor() {
        let mut settings = QuotaGuardSettings::default();
        settings.armed = false;
        settings.rearm_after_reset_percent_left = Some(40);
        let runtime = enabled_runtime(&settings);
        let (mut runtime, _) = reduce(
            runtime,
            ReducerEvent::Snapshot {
                snapshot: snapshot(92, Some(100)),
                full_read: true,
                verification: false,
                now_ms: 1,
            },
            &settings,
        );
        runtime.account.as_mut().unwrap().phase = QuotaGuardPhase::Tripped;
        let (runtime, effects) = reduce(
            runtime,
            ReducerEvent::Snapshot {
                snapshot: snapshot(18, Some(200)),
                full_read: true,
                verification: false,
                now_ms: 2,
            },
            &settings,
        );

        assert_eq!(
            runtime.account.as_ref().unwrap().phase,
            QuotaGuardPhase::Monitoring
        );
        assert!(effects.contains(&ReducerEffect::ResumeExternalEngines));
        assert!(effects.contains(&ReducerEffect::SetProcessOpen));
        assert!(effects.contains(&ReducerEffect::PersistAutoRearm {
            threshold_percent: 60
        }));
        assert!(runtime
            .account
            .unwrap()
            .activity_entries
            .iter()
            .any(|entry| entry
                .message
                .as_deref()
                .is_some_and(|message| message.contains("Automatically rearmed"))));
    }

    #[test]
    fn reset_without_rearm_setting_does_not_change_state() {
        let mut settings = QuotaGuardSettings::default();
        settings.armed = false;
        let runtime = enabled_runtime(&settings);
        let (runtime, _) = reduce(
            runtime,
            ReducerEvent::Snapshot {
                snapshot: snapshot(90, Some(100)),
                full_read: true,
                verification: false,
                now_ms: 1,
            },
            &settings,
        );
        let (runtime, effects) = reduce(
            runtime,
            ReducerEvent::Snapshot {
                snapshot: snapshot(10, Some(200)),
                full_read: true,
                verification: false,
                now_ms: 2,
            },
            &settings,
        );

        assert_eq!(runtime.account.unwrap().phase, QuotaGuardPhase::Monitoring);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, ReducerEffect::PersistAutoRearm { .. })));
    }

    #[test]
    fn reset_rearm_clamps_the_used_threshold_to_current_usage() {
        for action in [
            QuotaAction::NotifyOnly,
            QuotaAction::Interrupt,
            QuotaAction::Block,
        ] {
            let mut settings = QuotaGuardSettings::default();
            settings.action = action;
            settings.rearm_after_reset_percent_left = Some(40);
            let runtime = enabled_runtime(&settings);
            let (runtime, _) = reduce(
                runtime,
                ReducerEvent::Snapshot {
                    snapshot: snapshot(89, Some(100)),
                    full_read: true,
                    verification: false,
                    now_ms: 1,
                },
                &settings,
            );
            let (runtime, effects) = reduce(
                runtime,
                ReducerEvent::Snapshot {
                    snapshot: snapshot(75, Some(200)),
                    full_read: true,
                    verification: false,
                    now_ms: 2,
                },
                &settings,
            );

            assert!(effects.contains(&ReducerEffect::PersistAutoRearm {
                threshold_percent: 75
            }));
            let mut rearmed_settings = settings;
            rearmed_settings.armed = true;
            rearmed_settings.primary_threshold_percent = 75;
            rearmed_settings.secondary_threshold_percent = 75;
            assert!(runtime
                .account
                .as_ref()
                .is_some_and(|account| account.fire_at_or_above_on_next_snapshot));
            let (runtime, effects) = reduce(
                runtime,
                ReducerEvent::Snapshot {
                    snapshot: snapshot(75, Some(200)),
                    full_read: true,
                    verification: false,
                    now_ms: 3,
                },
                &rearmed_settings,
            );

            match action {
                QuotaAction::NotifyOnly => {
                    assert!(effects
                        .iter()
                        .any(|effect| matches!(effect, ReducerEffect::Notify { .. })));
                    assert_eq!(runtime.account.unwrap().phase, QuotaGuardPhase::Monitoring);
                }
                QuotaAction::Interrupt => {
                    assert!(effects.contains(&ReducerEffect::SetProcessClosed));
                    assert!(effects.contains(&ReducerEffect::PersistDisarmed));
                    assert_eq!(runtime.account.unwrap().phase, QuotaGuardPhase::Tripped);
                }
                QuotaAction::Block => {
                    assert!(effects.contains(&ReducerEffect::SetProcessClosed));
                    assert!(!effects.contains(&ReducerEffect::PersistDisarmed));
                    assert_eq!(runtime.account.unwrap().phase, QuotaGuardPhase::Tripped);
                }
            }
        }
    }
}
