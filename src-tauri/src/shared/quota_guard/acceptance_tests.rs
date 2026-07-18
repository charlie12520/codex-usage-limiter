use crate::shared::quota_guard::model::{
    PendingLocalStart, QuotaAction, QuotaGuardPhase, RateLimitSnapshot, RateLimitWindow,
    TerminalObservation, TurnKey, UnmatchedStartedTurn,
};
use crate::shared::quota_guard::coordinator::{
    AppServerControl, ControlCall, FakeAppServerControl, QuotaGuardHarness,
};
use crate::shared::quota_guard::reducer::{reduce, ReducerEffect, ReducerEvent};
use crate::types::QuotaGuardSettings;

const NOW: i64 = 1_000_000;

fn snapshot(used_percent: u8, reset_at: i64) -> RateLimitSnapshot {
    RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent,
            window_duration_mins: Some(60),
            resets_at: Some(reset_at),
        }),
        secondary: None,
        credits: None,
        plan_type: None,
        rate_limit_reached_type: None,
        observed_at: NOW,
    }
}

fn turn(turn_id: &str) -> TurnKey {
    TurnKey {
        session_epoch: "epoch-1".into(),
        workspace_id: "workspace-1".into(),
        thread_id: "thread-1".into(),
        turn_id: turn_id.into(),
    }
}

fn pending_start(request_id: u64, generation: u64) -> PendingLocalStart {
    PendingLocalStart {
        request_id,
        session_epoch: "epoch-1".into(),
        workspace_id: "workspace-1".into(),
        request_thread_id: Some("thread-1".into()),
        expected_thread_id: Some("thread-1".into()),
        request_kind: "turn/start".into(),
        generation,
        response_thread_id: None,
        response_received_at: None,
        disposition: None,
        registered_at: NOW,
    }
}

#[test]
fn stale_async_start_generation_cannot_survive_disable_reenable() {
    let settings = QuotaGuardSettings::default();
    let (runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    let old_generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(runtime, ReducerEvent::Disable { now_ms: NOW + 1 }, &settings);
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW + 2,
        },
        &settings,
    );

    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::PendingStartRecorded {
            start: pending_start(7, old_generation),
            now_ms: NOW + 3,
        },
        &settings,
    );

    assert!(effects.is_empty());
    assert!(runtime
        .account
        .expect("enabled account")
        .pending_local_starts
        .is_empty());
}

#[test]
fn terminal_completion_consumes_only_its_exact_pending_interrupt() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let (mut runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    let first = turn("turn-1");
    let second = turn("turn-2");
    runtime
        .account
        .as_mut()
        .expect("enabled account")
        .local_turn_registry = vec![first.clone(), second.clone()];
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::FinalizeClosedEpisode {
            transition_id: generation,
            now_ms: NOW,
        },
        &settings,
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, ReducerEffect::Interrupt { .. }))
            .count(),
        2
    );

    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::TurnTerminal {
            turn: first.clone(),
            status: "interrupted".into(),
            error: None,
            now_ms: NOW + 1,
        },
        &settings,
    );
    assert!(effects.is_empty(), "the other exact turn remains pending");
    let account = runtime.account.expect("account remains enforcing");
    assert_eq!(account.phase, QuotaGuardPhase::Interrupting);
    assert_eq!(account.pending_interrupt_index.len(), 1);
    assert!(account.pending_interrupt_index.contains_key(&second.stable_id()));
    assert!(!account.pending_interrupt_index.contains_key(&first.stable_id()));
}

#[test]
fn unmatched_review_and_terminal_buffers_are_bounded() {
    let mut account = crate::shared::quota_guard::model::AccountRuntime::new("account".into(), NOW);
    for sequence in 0..32 {
        let observed_turn = turn(&format!("review-{sequence}"));
        account
            .push_unmatched_started_turn(UnmatchedStartedTurn {
                turn: observed_turn.clone(),
                generation: 1,
                observed_at: NOW + sequence,
            })
            .expect("buffer remains bounded before capacity");
        account
            .push_terminal_observation(TerminalObservation {
                turn: observed_turn,
                generation: 1,
                status: "completed".into(),
                error: None,
                observed_at: NOW + sequence,
            })
            .expect("buffer remains bounded before capacity");
    }

    assert!(account
        .push_unmatched_started_turn(UnmatchedStartedTurn {
            turn: turn("overflow-review"),
            generation: 1,
            observed_at: NOW + 33,
        })
        .is_err());
    assert!(account
        .push_terminal_observation(TerminalObservation {
            turn: turn("overflow-terminal"),
            generation: 1,
            status: "completed".into(),
            error: None,
            observed_at: NOW + 33,
        })
        .is_err());
    assert_eq!(account.unmatched_started_turns.len(), 32);
    assert_eq!(account.terminal_observations.len(), 32);
}

#[test]
fn ordinary_response_then_notification_promotes_exact_owned_turn() {
    let settings = QuotaGuardSettings::default();
    let (runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::PendingStartRecorded {
            start: pending_start(11, generation),
            now_ms: NOW,
        },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::StartResponse {
            request_id: 11,
            session_epoch: "epoch-1".into(),
            workspace_id: "workspace-1".into(),
            thread_id: Some("thread-1".into()),
            now_ms: NOW + 1,
        },
        &settings,
    );
    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::TurnStarted {
            turn: turn("ordinary-turn"),
            now_ms: NOW + 2,
        },
        &settings,
    );

    assert!(effects.is_empty());
    let account = runtime.account.expect("account remains active");
    assert!(account.pending_local_starts.is_empty());
    assert_eq!(account.local_turn_registry, vec![turn("ordinary-turn")]);
}

#[test]
fn review_notification_before_response_is_matched_without_guessing_a_turn_id() {
    let settings = QuotaGuardSettings::default();
    let (runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let mut review = pending_start(12, generation);
    review.request_kind = "review/start".into();
    review.request_thread_id = None;
    review.expected_thread_id = None;
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::PendingStartRecorded {
            start: review,
            now_ms: NOW,
        },
        &settings,
    );
    let review_turn = TurnKey {
        session_epoch: "epoch-1".into(),
        workspace_id: "workspace-1".into(),
        thread_id: "review-thread".into(),
        turn_id: "review-turn".into(),
    };
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::TurnStarted {
            turn: review_turn.clone(),
            now_ms: NOW + 1,
        },
        &settings,
    );
    assert_eq!(
        runtime
            .account
            .as_ref()
            .expect("account")
            .unmatched_started_turns
            .len(),
        1
    );
    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::StartResponse {
            request_id: 12,
            session_epoch: "epoch-1".into(),
            workspace_id: "workspace-1".into(),
            thread_id: Some("review-thread".into()),
            now_ms: NOW + 2,
        },
        &settings,
    );

    assert!(effects.is_empty());
    let account = runtime.account.expect("account");
    assert!(account.pending_local_starts.is_empty());
    assert!(account.unmatched_started_turns.is_empty());
    assert_eq!(account.local_turn_registry, vec![review_turn]);
}

#[test]
fn terminal_without_an_exact_started_notification_cannot_claim_pending_ownership() {
    let settings = QuotaGuardSettings::default();
    let (runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::PendingStartRecorded {
            start: pending_start(13, generation),
            now_ms: NOW,
        },
        &settings,
    );
    let completed_turn = turn("already-completed");
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::TurnTerminal {
            turn: completed_turn,
            status: "completed".into(),
            error: None,
            now_ms: NOW + 1,
        },
        &settings,
    );
    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::StartResponse {
            request_id: 13,
            session_epoch: "epoch-1".into(),
            workspace_id: "workspace-1".into(),
            thread_id: Some("thread-1".into()),
            now_ms: NOW + 2,
        },
        &settings,
    );

    assert!(effects.is_empty());
    let account = runtime.account.expect("account");
    assert_eq!(account.pending_local_starts.len(), 1);
    assert!(account.unmatched_started_turns.is_empty());
    assert!(account.terminal_observations.is_empty());
    assert!(account.local_turn_registry.is_empty());
}

#[test]
fn enforcing_start_confirmation_expiry_fails_closed() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let (runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1 },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::PendingStartRecorded {
            start: pending_start(14, generation),
            now_ms: NOW + 2,
        },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::PendingStartExpired {
            request_id: 14,
            generation,
            now_ms: NOW + 10_002,
        },
        &settings,
    );

    let account = runtime.account.expect("account");
    assert_eq!(account.phase, QuotaGuardPhase::InterventionRequired);
    assert!(
        account
            .last_error
            .as_deref()
            .is_some_and(|message| message.contains("confirmation expired"))
    );
}

#[test]
fn interrupt_timeout_reconciles_retries_once_then_requires_intervention() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let target = turn("retry-turn");
    let (mut runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    runtime
        .account
        .as_mut()
        .expect("account")
        .local_turn_registry
        .push(target.clone());
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1 },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::FinalizeClosedEpisode {
            transition_id: generation,
            now_ms: NOW + 1,
        },
        &settings,
    );
    let pending = runtime
        .account
        .as_ref()
        .expect("account")
        .pending_interrupt_index
        .get(&target.stable_id())
        .expect("persisted first interrupt")
        .clone();
    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::InterruptDeadline {
            turn: target.clone(),
            generation: pending.generation,
            operation_id: pending.operation_id,
            attempt: pending.attempt,
            acknowledgement: true,
            now_ms: pending.ack_deadline,
        },
        &settings,
    );
    assert!(effects.iter().any(|effect| {
        matches!(
            effect,
            ReducerEffect::ReconcileThread {
                turn,
                operation_id,
                attempt,
                ..
            } if turn == &target && *operation_id == pending.operation_id && *attempt == 1
        )
    }));
    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::InterruptReconciled {
            turn: target.clone(),
            generation: pending.generation,
            operation_id: pending.operation_id,
            attempt: 1,
            active_turn_id: Some("retry-turn".into()),
            now_ms: NOW + 10_001,
        },
        &settings,
    );
    let retry = runtime
        .account
        .as_ref()
        .expect("account")
        .pending_interrupt_index
        .get(&target.stable_id())
        .expect("persisted retry")
        .clone();
    assert_eq!(retry.attempt, 2);
    assert!(effects.iter().any(|effect| {
        matches!(
            effect,
            ReducerEffect::Interrupt {
                turn,
                operation_id,
                attempt: 2,
                ..
            } if turn == &target && *operation_id == retry.operation_id
        )
    }));
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::InterruptReconciled {
            turn: target,
            generation: retry.generation,
            operation_id: retry.operation_id,
            attempt: 2,
            active_turn_id: Some("retry-turn".into()),
            now_ms: NOW + 20_001,
        },
        &settings,
    );
    assert_eq!(
        runtime.account.expect("account").phase,
        QuotaGuardPhase::InterventionRequired
    );
}

#[test]
fn completion_before_ack_removes_pending_interrupt_and_late_ack_is_inert() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let target = turn("terminal-first");
    let (mut runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    runtime
        .account
        .as_mut()
        .expect("account")
        .local_turn_registry
        .push(target.clone());
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1 },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::FinalizeClosedEpisode {
            transition_id: generation,
            now_ms: NOW + 1,
        },
        &settings,
    );
    let pending = runtime
        .account
        .as_ref()
        .expect("account")
        .pending_interrupt_index
        .get(&target.stable_id())
        .expect("pending interrupt")
        .clone();
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::TurnTerminal {
            turn: target.clone(),
            status: "interrupted".into(),
            error: None,
            now_ms: NOW + 2,
        },
        &settings,
    );
    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::InterruptAcknowledged {
            turn: target,
            generation: pending.generation,
            operation_id: pending.operation_id,
            attempt: pending.attempt,
            now_ms: NOW + 3,
        },
        &settings,
    );

    assert!(effects.is_empty());
    let account = runtime.account.expect("account");
    assert!(account.pending_interrupt_index.is_empty());
    assert_eq!(account.phase, QuotaGuardPhase::Parked);
}

#[test]
fn parked_transition_schedules_one_verification_without_a_periodic_quota_read() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let (runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1 },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let (runtime, effects) = reduce(
        runtime,
        ReducerEvent::FinalizeClosedEpisode {
            transition_id: generation,
            now_ms: NOW + 1,
        },
        &settings,
    );

    assert_eq!(runtime.account.expect("account").phase, QuotaGuardPhase::Parked);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, ReducerEffect::ScheduleVerification { .. }))
            .count(),
        1
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, ReducerEffect::ReadFullRateLimits)));
}

#[test]
fn restart_preserves_pending_exact_turn_and_does_not_replay_prior_effects() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let target = turn("restart-turn");
    let mut harness = QuotaGuardHarness::new(settings, NOW);
    harness.dispatch(ReducerEvent::Enable {
        account_key: "account".into(),
        now_ms: NOW,
    });
    harness.runtime.account.as_mut().expect("account").local_turn_registry = vec![target.clone()];
    harness.dispatch(ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW });
    harness.dispatch(ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1 });
    let generation = harness.runtime.lifecycle_generation;
    harness.dispatch(ReducerEvent::FinalizeClosedEpisode {
        transition_id: generation,
        now_ms: NOW + 1,
    });
    let pending_before_restart = harness
        .runtime
        .account
        .as_ref()
        .expect("account")
        .pending_interrupt_index
        .get(&target.stable_id())
        .expect("pending exact turn")
        .clone();
    let effects_before_restart = harness.take_effects();
    assert!(effects_before_restart.iter().any(
        |effect| matches!(effect, ReducerEffect::Interrupt { turn, .. } if turn == &target)
    ));

    let mut restarted = harness.restart();
    let account = restarted.runtime.account.as_ref().expect("persisted account");
    assert_eq!(account.phase, QuotaGuardPhase::Interrupting);
    assert_eq!(
        account.pending_interrupt_index.get(&target.stable_id()),
        Some(&pending_before_restart)
    );
    assert!(restarted.take_effects().is_empty(), "restart must reconcile, not replay an old effect");
}

#[test]
fn fake_control_receives_only_the_exact_turn_identity() {
    let control = FakeAppServerControl::default();
    let exact = TurnKey {
        session_epoch: "epoch-7".into(),
        workspace_id: "workspace-7".into(),
        thread_id: "thread-7".into(),
        turn_id: "turn-7".into(),
    };
    control.queue(
        format!("interrupt:{}", exact.stable_id()),
        Ok(serde_json::Value::Null),
    );
    tauri::async_runtime::block_on(async {
        control
            .interrupt_turn(exact.clone())
            .await
            .expect("queued exact interrupt succeeds");
    });

    assert_eq!(control.calls(), vec![ControlCall::Interrupt(exact)]);
}

#[test]
fn control_effect_runner_acknowledges_only_the_persisted_exact_interrupt() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let target = turn("controlled-turn");
    let mut harness = QuotaGuardHarness::new(settings, NOW);
    harness.dispatch(ReducerEvent::Enable {
        account_key: "account".into(),
        now_ms: NOW,
    });
    harness.runtime.account.as_mut().expect("account").local_turn_registry = vec![target.clone()];
    harness.dispatch(ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW });
    harness.dispatch(ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1 });
    let generation = harness.runtime.lifecycle_generation;
    harness.dispatch(ReducerEvent::FinalizeClosedEpisode {
        transition_id: generation,
        now_ms: NOW + 1,
    });

    let control = FakeAppServerControl::default();
    control.queue(
        format!("interrupt:{}", target.stable_id()),
        Ok(serde_json::Value::Null),
    );
    tauri::async_runtime::block_on(harness.execute_control_effects(&control));

    assert_eq!(control.calls(), vec![ControlCall::Interrupt(target.clone())]);
    let (operation_id, attempt) = {
        let pending = harness
            .runtime
            .account
            .as_ref()
            .expect("account")
            .pending_interrupt_index
            .get(&target.stable_id())
            .expect("exact pending interrupt");
        assert!(pending.acknowledged);
        assert!(pending.completion_deadline.is_some());
        (pending.operation_id, pending.attempt)
    };
    assert!(harness.take_effects().iter().any(|effect| {
        matches!(
            effect,
            ReducerEffect::ScheduleInterruptCompletion {
                turn,
                operation_id: scheduled_operation,
                attempt: scheduled_attempt,
                ..
            } if turn == &target && *scheduled_operation == operation_id && *scheduled_attempt == attempt
        )
    }));
}

#[test]
fn event_queue_overflow_fails_closed_and_requires_reconciliation() {
    let mut harness = QuotaGuardHarness::new(QuotaGuardSettings::default(), NOW);
    harness.dispatch(ReducerEvent::Enable {
        account_key: "account".into(),
        now_ms: NOW,
    });
    harness.take_effects();
    harness.advance_to(NOW + 1);
    harness.overflow();

    let account = harness.runtime.account.as_ref().expect("guarded account");
    assert_eq!(account.phase, QuotaGuardPhase::InterventionRequired);
    assert!(!account.monitor_healthy);
    assert_eq!(account.last_error.as_deref(), Some("event channel overflow"));
    assert!(
        harness.take_effects().is_empty(),
        "an overflow must fail closed rather than execute an uncorrelated action"
    );
}

#[test]
fn configured_workspaces_remain_visible_through_disconnect_and_reconnect() {
    let mut harness = QuotaGuardHarness::new(QuotaGuardSettings::default(), NOW);
    harness.configure_workspaces(["workspace-a".to_string(), "workspace-b".to_string()]);

    let initial = harness.public_admission();
    assert_eq!(initial["workspace-a"].reason, "workspaceUnbound");
    assert_eq!(initial["workspace-b"].reason, "workspaceUnbound");
    assert!(!initial["workspace-a"].open);

    harness.bind_workspace("workspace-a".into(), "epoch-1".into());
    let connected = harness.public_admission();
    assert_eq!(connected["workspace-a"].session_epoch.as_deref(), Some("epoch-1"));
    assert!(connected["workspace-a"].open);
    assert_eq!(connected["workspace-b"].reason, "workspaceUnbound");

    harness.disconnect_workspace("workspace-a");
    let disconnected = harness.public_admission();
    assert_eq!(disconnected["workspace-a"].reason, "workspaceUnbound");
    assert!(!disconnected["workspace-a"].open);

    harness.bind_workspace("workspace-a".into(), "epoch-2".into());
    let reconnected = harness.public_admission();
    assert_eq!(reconnected["workspace-a"].session_epoch.as_deref(), Some("epoch-2"));
    assert!(reconnected["workspace-a"].open);
}

#[test]
fn concurrent_settings_change_cannot_rewrite_the_episode_already_closing() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::FinishCurrentTurn;
    let mut harness = QuotaGuardHarness::new(settings, NOW);
    harness.dispatch(ReducerEvent::Enable {
        account_key: "account".into(),
        now_ms: NOW,
    });
    let allowed = turn("pre-threshold-turn");
    harness.runtime.account.as_mut().expect("account").local_turn_registry = vec![allowed];
    harness.dispatch(ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW });
    harness.dispatch(ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1 });
    let generation = harness.runtime.lifecycle_generation;

    harness.settings.action = QuotaAction::InterruptImmediately;
    harness.dispatch(ReducerEvent::FinalizeClosedEpisode {
        transition_id: generation,
        now_ms: NOW + 1,
    });

    let account = harness.runtime.account.expect("account");
    assert_eq!(
        account.episode_policy.expect("captured episode policy").action,
        QuotaAction::FinishCurrentTurn
    );
    assert_eq!(account.phase, QuotaGuardPhase::Draining);
    assert_eq!(account.allowed_drain_turns.len(), 1);
}

#[test]
fn stale_or_early_interrupt_deadlines_cannot_trigger_extra_reconciliation() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let target = turn("deadline-turn");
    let mut harness = QuotaGuardHarness::new(settings, NOW);
    harness.dispatch(ReducerEvent::Enable {
        account_key: "account".into(),
        now_ms: NOW,
    });
    harness.runtime.account.as_mut().expect("account").local_turn_registry = vec![target.clone()];
    harness.dispatch(ReducerEvent::Snapshot { snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW });
    harness.dispatch(ReducerEvent::Snapshot { snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1 });
    let generation = harness.runtime.lifecycle_generation;
    harness.dispatch(ReducerEvent::FinalizeClosedEpisode {
        transition_id: generation,
        now_ms: NOW + 1,
    });
    let initial = harness
        .runtime
        .account
        .as_ref()
        .expect("account")
        .pending_interrupt_index
        .get(&target.stable_id())
        .expect("pending interrupt")
        .clone();
    harness.take_effects();

    harness.dispatch(ReducerEvent::InterruptAcknowledged {
        turn: target.clone(),
        generation: initial.generation,
        operation_id: initial.operation_id,
        attempt: initial.attempt,
        now_ms: NOW + 2,
    });
    let acknowledged = harness
        .runtime
        .account
        .as_ref()
        .expect("account")
        .pending_interrupt_index
        .get(&target.stable_id())
        .expect("acknowledged interrupt")
        .clone();
    let completion_deadline = acknowledged
        .completion_deadline
        .expect("ack persists a completion deadline");
    harness.take_effects();

    harness.dispatch(ReducerEvent::InterruptDeadline {
        turn: target.clone(),
        generation: initial.generation,
        operation_id: initial.operation_id,
        attempt: initial.attempt,
        acknowledgement: true,
        now_ms: initial.ack_deadline,
    });
    assert!(
        harness.take_effects().is_empty(),
        "the stale acknowledgement timer must not reconcile after acknowledgement"
    );

    harness.dispatch(ReducerEvent::InterruptDeadline {
        turn: target.clone(),
        generation: acknowledged.generation,
        operation_id: acknowledged.operation_id,
        attempt: acknowledged.attempt,
        acknowledgement: false,
        now_ms: completion_deadline - 1,
    });
    harness.dispatch(ReducerEvent::InterruptDeadline {
        turn: target.clone(),
        generation: acknowledged.generation,
        operation_id: acknowledged.operation_id + 1,
        attempt: acknowledged.attempt,
        acknowledgement: false,
        now_ms: completion_deadline,
    });
    assert!(
        harness.take_effects().is_empty(),
        "early or fabricated completion timers must not reconcile a live turn"
    );

    harness.dispatch(ReducerEvent::InterruptDeadline {
        turn: target.clone(),
        generation: acknowledged.generation,
        operation_id: acknowledged.operation_id,
        attempt: acknowledged.attempt,
        acknowledgement: false,
        now_ms: completion_deadline,
    });
    assert!(harness.take_effects().iter().any(|effect| {
        matches!(
            effect,
            ReducerEffect::ReconcileThread {
                turn,
                operation_id,
                attempt,
                ..
            } if turn == &target
                && *operation_id == acknowledged.operation_id
                && *attempt == acknowledged.attempt
        )
    }));
}

#[test]
fn stale_terminal_cannot_consume_a_new_unmatched_review_turn() {
    let settings = QuotaGuardSettings::default();
    let (runtime, _) = reduce(
        Default::default(),
        ReducerEvent::Enable {
            account_key: "account".into(),
            now_ms: NOW,
        },
        &settings,
    );
    let generation = runtime.lifecycle_generation;
    let mut review = pending_start(15, generation);
    review.request_kind = "review/start".into();
    review.request_thread_id = None;
    review.expected_thread_id = None;
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::PendingStartRecorded {
            start: review,
            now_ms: NOW,
        },
        &settings,
    );
    let new_turn = TurnKey {
        session_epoch: "epoch-1".into(),
        workspace_id: "workspace-1".into(),
        thread_id: "review-thread".into(),
        turn_id: "new-turn".into(),
    };
    let stale_turn = TurnKey {
        turn_id: "old-turn".into(),
        ..new_turn.clone()
    };
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::TurnStarted {
            turn: new_turn.clone(),
            now_ms: NOW + 1,
        },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::TurnTerminal {
            turn: stale_turn,
            status: "completed".into(),
            error: None,
            now_ms: NOW + 2,
        },
        &settings,
    );
    let account = runtime.account.as_ref().expect("account");
    assert_eq!(account.pending_local_starts.len(), 1);
    assert_eq!(account.unmatched_started_turns.len(), 1);
    assert_eq!(account.unmatched_started_turns[0].turn, new_turn);
    assert!(
        account.terminal_observations.is_empty(),
        "a same-thread terminal for a different turn must not be buffered as the pending review"
    );

    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::TurnTerminal {
            turn: new_turn,
            status: "completed".into(),
            error: None,
            now_ms: NOW + 3,
        },
        &settings,
    );
    let (runtime, _) = reduce(
        runtime,
        ReducerEvent::StartResponse {
            request_id: 15,
            session_epoch: "epoch-1".into(),
            workspace_id: "workspace-1".into(),
            thread_id: Some("review-thread".into()),
            now_ms: NOW + 4,
        },
        &settings,
    );
    let account = runtime.account.expect("account");
    assert!(account.pending_local_starts.is_empty());
    assert!(account.unmatched_started_turns.is_empty());
    assert!(account.terminal_observations.is_empty());
    assert!(account.local_turn_registry.is_empty());
}

#[test]
fn owned_usage_limit_terminal_starts_hard_episode_and_reads_once() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let target = turn("owned-usage-limit");
    let (mut runtime, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "account".into(), now_ms: NOW }, &settings);
    runtime.account.as_mut().expect("account").local_turn_registry.push(target.clone());

    let (_, effects) = reduce(
        runtime,
        ReducerEvent::TurnTerminal {
            turn: target,
            status: "failed".into(),
            error: Some(serde_json::json!({"codexErrorInfo":"usageLimitExceeded"})),
            now_ms: NOW + 1,
        },
        &settings,
    );

    assert!(effects.iter().any(|effect| matches!(effect, ReducerEffect::SetProcessClosed)));
    assert_eq!(
        effects.iter().filter(|effect| matches!(effect, ReducerEffect::ReadFullRateLimits)).count(),
        1,
    );
}

#[test]
fn no_rate_update_after_owned_usage_limit_parks_with_fetched_reset_scope() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let target = turn("owned-usage-limit-no-update");
    let (mut runtime, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "account".into(), now_ms: NOW }, &settings);
    let (mut runtime, _) = reduce(runtime, ReducerEvent::Snapshot {
        snapshot: snapshot(20, 2_000), full_read: true, verification: false, now_ms: NOW,
    }, &settings);
    runtime.account.as_mut().expect("account").local_turn_registry.push(target.clone());
    let (runtime, _) = reduce(runtime, ReducerEvent::TurnTerminal {
        turn: target,
        status: "failed".into(),
        error: Some(serde_json::json!({"codexErrorInfo":"usageLimitExceeded"})),
        now_ms: NOW + 1,
    }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::Snapshot {
        snapshot: snapshot(20, 2_000), full_read: true, verification: false, now_ms: NOW + 2,
    }, &settings);
    let generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(runtime, ReducerEvent::FinalizeClosedEpisode {
        transition_id: generation, now_ms: NOW + 2,
    }, &settings);

    let account = runtime.account.expect("account");
    assert_eq!(account.phase, QuotaGuardPhase::Parked);
    assert!(account.verify_at.is_some());
    assert!(account.fired_episodes.iter().any(|episode| matches!(episode, crate::shared::quota_guard::model::EpisodeKey::HardLimit { .. })));
}

#[test]
fn restart_rehydrates_exact_pending_interrupt_with_fenced_reconciliation() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let target = turn("rehydrate-target");
    let (mut runtime, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "account".into(), now_ms: NOW }, &settings);
    runtime.account.as_mut().expect("account").local_turn_registry.push(target.clone());
    let (runtime, _) = reduce(runtime, ReducerEvent::Snapshot {
        snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW,
    }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::Snapshot {
        snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1,
    }, &settings);
    let generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(runtime, ReducerEvent::FinalizeClosedEpisode { transition_id: generation, now_ms: NOW }, &settings);
    let pending = runtime.account.as_ref().expect("account").pending_interrupt_index[&target.stable_id()].clone();

    let (restarted, effects) = reduce(
        runtime,
        ReducerEvent::RehydratePendingInterrupts { now_ms: NOW + 1 },
        &settings,
    );

    assert_eq!(restarted.account.expect("account").pending_interrupt_index[&target.stable_id()], pending);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        ReducerEffect::ReconcileThread { turn, generation, operation_id, attempt }
            if turn == &target && *generation == pending.generation && *operation_id == pending.operation_id && *attempt == pending.attempt
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        ReducerEffect::ScheduleInterruptAck { turn, generation, operation_id, attempt, .. }
            if turn == &target && *generation == pending.generation && *operation_id == pending.operation_id && *attempt == pending.attempt
    )));
}

#[test]
fn healthy_full_read_before_due_keeps_parked_breach_scope_closed() {
    let mut settings = QuotaGuardSettings::default();
    settings.action = QuotaAction::InterruptImmediately;
    let (runtime, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "account".into(), now_ms: NOW }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::Snapshot {
        snapshot: snapshot(89, 2_000), full_read: true, verification: false, now_ms: NOW,
    }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::Snapshot {
        snapshot: snapshot(90, 2_000), full_read: true, verification: false, now_ms: NOW + 1,
    }, &settings);
    let generation = runtime.lifecycle_generation;
    let (runtime, _) = reduce(runtime, ReducerEvent::FinalizeClosedEpisode { transition_id: generation, now_ms: NOW }, &settings);
    let verify_at = runtime.account.as_ref().expect("parked account").verify_at.expect("verification deadline");
    let (runtime, effects) = reduce(runtime, ReducerEvent::Snapshot {
        snapshot: snapshot(1, 2_000), full_read: true, verification: false, now_ms: verify_at - 1,
    }, &settings);

    let account = runtime.account.expect("account");
    assert_eq!(account.phase, QuotaGuardPhase::Parked);
    assert!(!account.breached_windows.is_empty());
    assert!(!effects.iter().any(|effect| matches!(effect, ReducerEffect::SetProcessOpen)));
}

#[test]
fn disable_enable_retains_only_previously_owned_active_turn() {
    let settings = QuotaGuardSettings::default();
    let owned = turn("owned-before-disable");
    let external = turn("external-after-disable");
    let (mut runtime, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "account".into(), now_ms: NOW }, &settings);
    runtime.account.as_mut().expect("account").local_turn_registry.push(owned.clone());
    let (runtime, _) = reduce(runtime, ReducerEvent::Disable { now_ms: NOW + 1 }, &settings);
    assert_eq!(runtime.account.as_ref().expect("disabled account").phase, QuotaGuardPhase::Disabled);
    let (runtime, _) = reduce(runtime, ReducerEvent::TurnStarted { turn: external, now_ms: NOW + 2 }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::Enable { account_key: "account".into(), now_ms: NOW + 3 }, &settings);

    assert_eq!(runtime.account.expect("enabled account").local_turn_registry, vec![owned]);
}

#[test]
fn stale_provisionals_expire_before_delayed_response_or_terminal_can_claim_ownership() {
    let settings = QuotaGuardSettings::default();
    let (runtime, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "account".into(), now_ms: NOW }, &settings);
    let generation = runtime.lifecycle_generation;
    let mut review = pending_start(99, generation);
    review.request_kind = "review/start".into();
    review.request_thread_id = None;
    review.expected_thread_id = None;
    let (runtime, _) = reduce(runtime, ReducerEvent::PendingStartRecorded { start: review, now_ms: NOW }, &settings);
    let provisional = TurnKey {
        session_epoch: "epoch-1".into(), workspace_id: "workspace-1".into(), thread_id: "review-thread".into(), turn_id: "provisional".into(),
    };
    let (runtime, _) = reduce(runtime, ReducerEvent::TurnStarted { turn: provisional.clone(), now_ms: NOW + 1 }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::TurnTerminal {
        turn: provisional.clone(), status: "completed".into(), error: None, now_ms: NOW + 2,
    }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::ProvisionalExpired {
        turn: provisional.clone(), generation, terminal: true, now_ms: NOW + 10_002,
    }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::ProvisionalExpired {
        turn: provisional.clone(), generation, terminal: false, now_ms: NOW + 10_002,
    }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::StartResponse {
        request_id: 99, session_epoch: "epoch-1".into(), workspace_id: "workspace-1".into(),
        thread_id: Some("review-thread".into()), now_ms: NOW + 10_003,
    }, &settings);
    let (runtime, _) = reduce(runtime, ReducerEvent::TurnTerminal {
        turn: provisional, status: "completed".into(), error: None, now_ms: NOW + 10_004,
    }, &settings);

    let account = runtime.account.expect("account");
    assert!(account.local_turn_registry.is_empty());
    assert!(account.unmatched_started_turns.is_empty());
    assert!(account.terminal_observations.is_empty());
}
