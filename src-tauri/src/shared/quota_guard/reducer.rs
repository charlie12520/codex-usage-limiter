use crate::types::QuotaGuardSettings;

use super::evaluator::evaluate_snapshot;
use super::model::{AccountRuntime, EpisodeKey, EpisodePolicy, PendingLocalStart, PendingStartDisposition, PendingInterrupt, QuotaAction, QuotaGuardPhase, QuotaGuardRuntimeState, QuotaWindowKind, RateLimitSnapshot, TurnKey};
use super::parser::is_usage_limit_exceeded;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReducerEffect {
    SetProcessClosed,
    SetProcessOpen,
    Notify { episode: EpisodeKey },
    FinalizeClosedEpisode { transition_id: u64 },
    Interrupt { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8 },
    ReconcileThread { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8 },
    ScheduleDrain { generation: u64, deadline: i64 },
    ScheduleVerification { generation: u64, verify_at: i64 },
    ScheduleInterruptAck { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8, deadline: i64 },
    ScheduleStartExpiry { request_id: u64, generation: u64, deadline: i64 },
    ScheduleProvisionalExpiry { turn: TurnKey, generation: u64, terminal: bool, deadline: i64 },
    ScheduleInterruptCompletion { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8, deadline: i64 },
    ReadFullRateLimits,
}

#[derive(Debug, Clone)]
pub(crate) enum ReducerEvent {
    Enable { account_key: String, now_ms: i64 },
    TurnStarted { turn: TurnKey, now_ms: i64 },
    TurnTerminal { turn: TurnKey, status: String, error: Option<serde_json::Value>, now_ms: i64 },
    StartResponse { request_id: u64, session_epoch: String, workspace_id: String, thread_id: Option<String>, now_ms: i64 },
    Disable { now_ms: i64 },
    Snapshot { snapshot: RateLimitSnapshot, full_read: bool, verification: bool, now_ms: i64 },
    ApplyActionNow { now_ms: i64 },
    FinalizeClosedEpisode { transition_id: u64, now_ms: i64 },
    DrainDeadline { generation: u64, now_ms: i64 },
    KeepWaiting { now_ms: i64 },
    InterruptAcknowledged { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8, now_ms: i64 },
    InterruptRequestFailed { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8, now_ms: i64 },
    InterruptDeadline { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8, acknowledgement: bool, now_ms: i64 },
    InterruptReconciled { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8, active_turn_id: Option<String>, now_ms: i64 },
    InterruptReconcileFailed { turn: TurnKey, generation: u64, operation_id: u64, attempt: u8, reason: String, now_ms: i64 },
    PendingStartExpired { request_id: u64, generation: u64, now_ms: i64 },
    ProvisionalExpired { turn: TurnKey, generation: u64, terminal: bool, now_ms: i64 },
    RehydratePendingInterrupts { now_ms: i64 },
    /// Durable acknowledgement before the request JSON is written.
    PendingStartRecorded { start: PendingLocalStart, now_ms: i64 },
    /// Idempotent reliable failure cleanup for a start that never bound.
    PendingStartFailed { request_id: u64, generation: u64, now_ms: i64 },
}

fn policy(settings: &QuotaGuardSettings) -> EpisodePolicy {
    EpisodePolicy { action: settings.action, drain_timeout_minutes: settings.drain_timeout_minutes, drain_timeout_action: settings.drain_timeout_action, reset_grace_minutes: settings.reset_grace_minutes }
}

fn increment_generation(runtime: &mut QuotaGuardRuntimeState) -> bool {
    match runtime.lifecycle_generation.checked_add(1) { Some(value) => { runtime.lifecycle_generation = value; true }, None => false }
}

const INTERRUPT_ACK_TIMEOUT_MS: i64 = 10_000;
const INTERRUPT_COMPLETION_TIMEOUT_MS: i64 = 30_000;
const START_CONFIRMATION_TIMEOUT_MS: i64 = 10_000;
const PROVISIONAL_OBSERVATION_TIMEOUT_MS: i64 = 10_000;

fn next_operation_id(runtime: &mut QuotaGuardRuntimeState) -> Result<u64, String> {
    runtime.next_operation_id = runtime.next_operation_id.checked_add(1)
        .ok_or_else(|| "interrupt operation counter overflow".to_string())?;
    Ok(runtime.next_operation_id)
}

fn interrupt_empty(account: &AccountRuntime) -> bool {
    account.pending_interrupt_index.is_empty() && account.pending_local_starts.is_empty()
}

fn finish_if_empty(account: &mut AccountRuntime, generation: u64, now_ms: i64, effects: &mut Vec<ReducerEffect>) {
    let empty = match account.phase {
        QuotaGuardPhase::Interrupting => interrupt_empty(account),
        QuotaGuardPhase::Draining | QuotaGuardPhase::AwaitingDrainDecision =>
            account.allowed_drain_turns.is_empty() && account.pending_local_starts.is_empty(),
        _ => false,
    };
    if empty {
        match parked_verification_effect(account, generation) {
            Ok(effect) => effects.push(effect),
            Err(error) => enter_intervention(account, now_ms, &error),
        }
    }
}

fn add_minutes(now_ms: i64, minutes: u16) -> Option<i64> {
    i64::from(minutes).checked_mul(60_000).and_then(|duration| now_ms.checked_add(duration))
}

fn enter_intervention(account: &mut AccountRuntime, now_ms: i64, message: &str) {
    account.phase = QuotaGuardPhase::InterventionRequired;
    account.last_error = Some(message.to_string());
    account.updated_at = now_ms;
}

fn start_episode(account: &mut AccountRuntime, episode: EpisodeKey, settings: &QuotaGuardSettings, _now_ms: i64, effects: &mut Vec<ReducerEffect>, generation: u64) {
    account.fired_episodes.insert(episode.clone());
    account.episode_policy = Some(policy(settings));
    match settings.action {
        QuotaAction::NotifyOnly => {
            account.phase = QuotaGuardPhase::Monitoring;
            account.episode_policy = None;
            effects.push(ReducerEffect::SetProcessOpen);
            effects.push(ReducerEffect::Notify { episode });
        }
        QuotaAction::InterruptImmediately | QuotaAction::FinishCurrentTurn => {
            account.phase = QuotaGuardPhase::Closing;
            effects.push(ReducerEffect::SetProcessClosed);
            effects.push(ReducerEffect::FinalizeClosedEpisode { transition_id: generation });
        }
    }
}

fn verification_at(account: &AccountRuntime, reset_grace_minutes: u16) -> Option<i64> {
    let snapshot = account.snapshot.as_ref()?;
    let latest_reset = if account
        .fired_episodes
        .iter()
        .any(|episode| matches!(episode, EpisodeKey::HardLimit { .. }))
    {
        [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
            .into_iter()
            .flatten()
            .map(|window| window.resets_at)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .max()?
    } else {
        account
            .breached_windows
            .iter()
            .filter_map(|kind| snapshot.window(*kind).and_then(|window| window.resets_at))
            .max()?
    };
    latest_reset
        .checked_mul(1_000)?
        .checked_add(i64::from(reset_grace_minutes).checked_mul(60_000)?)
}

fn begin_interrupting(account: &mut AccountRuntime, turns: Vec<TurnKey>, generation: u64, operation_id: u64, now_ms: i64, effects: &mut Vec<ReducerEffect>) -> Result<(), String> {
    let ack_deadline = now_ms.checked_add(INTERRUPT_ACK_TIMEOUT_MS)
        .ok_or_else(|| "interrupt acknowledgement deadline overflow".to_string())?;
    account.phase = QuotaGuardPhase::Interrupting;
    account.drain_deadline = None;
    for turn in turns {
        if account.pending_interrupt_index.contains_key(&turn.stable_id()) {
            continue;
        }
        let pending = PendingInterrupt {
            turn: turn.clone(),
            generation,
            operation_id,
            attempt: 1,
            acknowledged: false,
            ack_deadline,
            completion_deadline: None,
        };
        account.insert_pending_interrupt(pending);
        effects.push(ReducerEffect::Interrupt { turn: turn.clone(), generation, operation_id, attempt: 1 });
        effects.push(ReducerEffect::ScheduleInterruptAck { turn, generation, operation_id, attempt: 1, deadline: ack_deadline });
    }
    Ok(())
}

fn promote_start(
    account: &mut AccountRuntime,
    start: PendingLocalStart,
    turn: TurnKey,
    generation: u64,
    operation_id: u64,
    now_ms: i64,
    effects: &mut Vec<ReducerEffect>,
) {
    if account.local_turn_registry.iter().all(|candidate| candidate.stable_id() != turn.stable_id()) {
        account.local_turn_registry.push(turn.clone());
    }
    match start.disposition {
        Some(PendingStartDisposition::AllowOnBind) => {
            if account.allowed_drain_turns.iter().all(|candidate| candidate.stable_id() != turn.stable_id()) {
                account.allowed_drain_turns.push(turn);
            }
        }
        Some(PendingStartDisposition::InterruptOnBind)
        | None if matches!(account.phase, QuotaGuardPhase::Closing | QuotaGuardPhase::Interrupting | QuotaGuardPhase::Draining | QuotaGuardPhase::AwaitingDrainDecision) => {
            if let Err(error) = begin_interrupting(account, vec![turn], generation, operation_id, now_ms, effects) {
                enter_intervention(account, now_ms, &error);
            }
        }
        _ => {}
    }
}

pub(crate) fn reduce(mut runtime: QuotaGuardRuntimeState, event: ReducerEvent, settings: &QuotaGuardSettings) -> (QuotaGuardRuntimeState, Vec<ReducerEffect>) {
    let mut effects = Vec::new();
    match event {
        ReducerEvent::Enable { account_key, now_ms } => {
            if !increment_generation(&mut runtime) {
                runtime.account = Some(AccountRuntime::new(account_key, now_ms));
                if let Some(account) = runtime.account.as_mut() { enter_intervention(account, now_ms, "lifecycle generation overflow"); }
            } else if let Some(account) = runtime.account.as_mut().filter(|account| account.account_key == account_key && account.phase == QuotaGuardPhase::Disabled) {
                account.phase = QuotaGuardPhase::Monitoring;
                account.snapshot = None;
                account.breached_windows.clear();
                account.fired_episodes.clear();
                account.episode_policy = None;
                account.drain_deadline = None;
                account.verify_at = None;
                account.monitor_healthy = true;
                account.last_error = None;
                account.updated_at = now_ms;
            } else {
                runtime.account = Some(AccountRuntime::new(account_key, now_ms));
            }
            effects.push(ReducerEffect::SetProcessClosed);
        }
        ReducerEvent::Disable { now_ms } => {
            if !increment_generation(&mut runtime) {
                if let Some(account) = runtime.account.as_mut() { enter_intervention(account, now_ms, "lifecycle generation overflow"); }
                effects.push(ReducerEffect::SetProcessClosed);
            } else if let Some(account) = runtime.account.as_mut() {
                account.phase = QuotaGuardPhase::Disabled;
                account.revalidation_return_phase = None;
                account.snapshot = None;
                account.breached_windows.clear();
                account.fired_episodes.clear();
                account.episode_policy = None;
                account.pending_local_starts.clear();
                account.unmatched_started_turns.clear();
                account.terminal_observations.clear();
                account.allowed_drain_turns.clear();
                account.pending_interrupt_index.clear();
                account.drain_deadline = None;
                account.verify_at = None;
                account.monitor_healthy = true;
                account.last_error = None;
                account.updated_at = now_ms;
                effects.push(ReducerEffect::SetProcessOpen);
            } else {
                effects.push(ReducerEffect::SetProcessOpen);
            }
        }
        ReducerEvent::PendingStartRecorded { mut start, now_ms } => {
            let generation = runtime.lifecycle_generation;
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            if start.generation != generation || start.session_epoch.trim().is_empty() || start.workspace_id.trim().is_empty() {
                return (runtime, effects);
            }
            if matches!(account.phase, QuotaGuardPhase::Closing | QuotaGuardPhase::Interrupting | QuotaGuardPhase::Draining | QuotaGuardPhase::AwaitingDrainDecision) {
                start.disposition = Some(match account.episode_policy.as_ref().map(|policy| policy.action) {
                    Some(QuotaAction::FinishCurrentTurn) if account.phase == QuotaGuardPhase::Closing => PendingStartDisposition::AllowOnBind,
                    _ => PendingStartDisposition::InterruptOnBind,
                });
            }
            let deadline = match now_ms.checked_add(START_CONFIRMATION_TIMEOUT_MS) {
                Some(value) => value,
                None => {
                    enter_intervention(account, now_ms, "start confirmation deadline overflow");
                    return (runtime, effects);
                }
            };
            let request_id = start.request_id;
            account.pending_local_starts.insert(request_id, start);
            account.updated_at = now_ms;
            effects.push(ReducerEffect::ScheduleStartExpiry { request_id, generation, deadline });
        }
        ReducerEvent::PendingStartFailed { request_id, generation, now_ms } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            account.pending_local_starts.remove(&request_id);
            account.updated_at = now_ms;
            finish_if_empty(account, generation, now_ms, &mut effects);
        }
        ReducerEvent::PendingStartExpired { request_id, generation, now_ms } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let Some(start) = account.pending_local_starts.get(&request_id) else { return (runtime, effects) };
            if now_ms < start.registered_at.saturating_add(START_CONFIRMATION_TIMEOUT_MS) { return (runtime, effects); }
            account.pending_local_starts.remove(&request_id);
            if matches!(account.phase, QuotaGuardPhase::Closing | QuotaGuardPhase::Interrupting | QuotaGuardPhase::Draining | QuotaGuardPhase::AwaitingDrainDecision) {
                enter_intervention(account, now_ms, "local start ownership confirmation expired");
            } else {
                finish_if_empty(account, generation, now_ms, &mut effects);
            }
        }
        ReducerEvent::StartResponse { request_id, session_epoch, workspace_id, thread_id, now_ms } => {
            let operation_id = match next_operation_id(&mut runtime) {
                Ok(value) => value,
                Err(error) => {
                    if let Some(account) = runtime.account.as_mut() { enter_intervention(account, now_ms, &error); }
                    return (runtime, effects);
                }
            };
            let generation = runtime.lifecycle_generation;
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let Some(start) = account.pending_local_starts.get_mut(&request_id) else { return (runtime, effects) };
            if start.generation != generation || start.session_epoch != session_epoch || start.workspace_id != workspace_id {
                return (runtime, effects);
            }
            let thread_id = thread_id.or_else(|| start.expected_thread_id.clone()).or_else(|| start.request_thread_id.clone());
            start.response_thread_id = thread_id.clone();
            start.response_received_at = Some(now_ms);
            let Some(thread_id) = thread_id else { return (runtime, effects) };
            let Some(started_index) = account.unmatched_started_turns.iter().position(|observation| {
                observation.turn.session_epoch == session_epoch && observation.turn.workspace_id == workspace_id && observation.turn.thread_id == thread_id
            }) else {
                return (runtime, effects);
            };
            let started = account.unmatched_started_turns[started_index].turn.clone();
            if let Some(terminal_index) = account.terminal_observations.iter().position(|observation| {
                observation.turn.stable_id() == started.stable_id()
            }) {
                account.terminal_observations.remove(terminal_index);
                account.unmatched_started_turns.remove(started_index);
                account.pending_local_starts.remove(&request_id);
                finish_if_empty(account, generation, now_ms, &mut effects);
                return (runtime, effects);
            }
            let observation = account.unmatched_started_turns.remove(started_index);
            let start = account.pending_local_starts.remove(&request_id).expect("matched start exists");
            promote_start(account, start, observation.turn, generation, operation_id, now_ms, &mut effects);
        }
        ReducerEvent::TurnStarted { turn, now_ms } => {
            let operation_id = match next_operation_id(&mut runtime) {
                Ok(value) => value,
                Err(error) => {
                    if let Some(account) = runtime.account.as_mut() { enter_intervention(account, now_ms, &error); }
                    return (runtime, effects);
                }
            };
            let generation = runtime.lifecycle_generation;
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let matching_request = account.pending_local_starts.iter().find_map(|(request_id, start)| {
                (start.generation == generation
                    && start.session_epoch == turn.session_epoch
                    && start.workspace_id == turn.workspace_id
                    && [start.response_thread_id.as_ref(), start.expected_thread_id.as_ref(), start.request_thread_id.as_ref()]
                        .into_iter().flatten().any(|thread_id| thread_id == &turn.thread_id))
                    .then_some(*request_id)
            });
            if let Some(request_id) = matching_request {
                let start = account.pending_local_starts.remove(&request_id).expect("matched start exists");
                promote_start(account, start, turn, generation, operation_id, now_ms, &mut effects);
            } else if account.pending_local_starts.values().any(|start| {
                start.generation == generation && start.session_epoch == turn.session_epoch && start.workspace_id == turn.workspace_id && start.request_kind == "review/start"
            }) {
                let deadline = match now_ms.checked_add(PROVISIONAL_OBSERVATION_TIMEOUT_MS) {
                    Some(value) => value,
                    None => {
                        enter_intervention(account, now_ms, "provisional observation deadline overflow");
                        return (runtime, effects);
                    }
                };
                if let Err(error) = account.push_unmatched_started_turn(super::model::UnmatchedStartedTurn { turn: turn.clone(), generation, observed_at: now_ms }) {
                    enter_intervention(account, now_ms, &error);
                } else {
                    effects.push(ReducerEffect::ScheduleProvisionalExpiry { turn, generation, terminal: false, deadline });
                }
            }
        }
        ReducerEvent::TurnTerminal { turn, status, error, now_ms } => {
            let generation = runtime.lifecycle_generation;
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let id = turn.stable_id();
            let known = account.local_turn_registry.iter().any(|candidate| candidate.stable_id() == id)
                || account.pending_interrupt_index.contains_key(&id)
                || account.allowed_drain_turns.iter().any(|candidate| candidate.stable_id() == id);
            if known && account.phase != QuotaGuardPhase::Disabled && error.as_ref().is_some_and(is_usage_limit_exceeded) {
                let episode = EpisodeKey::HardLimit { account_key: account.account_key.clone() };
                if !account.fired_episodes.contains(&episode) {
                    start_episode(account, episode, settings, now_ms, &mut effects, generation);
                    let before_finalize = effects.len().saturating_sub(1);
                    effects.insert(before_finalize, ReducerEffect::ReadFullRateLimits);
                }
            }
            account.local_turn_registry.retain(|candidate| candidate.stable_id() != id);
            account.allowed_drain_turns.retain(|candidate| candidate.stable_id() != id);
            account.remove_pending_interrupt(&turn);
            let has_exact_unmatched_start = account.unmatched_started_turns.iter()
                .any(|observation| observation.turn.stable_id() == id && observation.generation == generation);
            if !known && has_exact_unmatched_start {
                let deadline = match now_ms.checked_add(PROVISIONAL_OBSERVATION_TIMEOUT_MS) {
                    Some(value) => value,
                    None => {
                        enter_intervention(account, now_ms, "provisional terminal deadline overflow");
                        return (runtime, effects);
                    }
                };
                if let Err(error) = account.push_terminal_observation(super::model::TerminalObservation { turn: turn.clone(), generation, status, error, observed_at: now_ms }) {
                    enter_intervention(account, now_ms, &error);
                    return (runtime, effects);
                }
                effects.push(ReducerEffect::ScheduleProvisionalExpiry { turn, generation, terminal: true, deadline });
            }
            finish_if_empty(account, generation, now_ms, &mut effects);
        }
        ReducerEvent::Snapshot { snapshot, full_read, verification, now_ms } => {
            let generation = runtime.lifecycle_generation;
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let prior = account.snapshot.clone();
            let prior_breaches = account.breached_windows.clone();
            let parked_or_verifying = matches!(account.phase, QuotaGuardPhase::Parked | QuotaGuardPhase::VerifyingReset);
            let verification_due = account.verify_at.is_some_and(|verify_at| now_ms >= verify_at);
            let evaluation = evaluate_snapshot(&account.account_key, &snapshot, prior.as_ref(), settings, &account.fired_episodes, full_read);
            let retain_hard_limit = matches!(
                account.phase,
                QuotaGuardPhase::Closing
                    | QuotaGuardPhase::Interrupting
                    | QuotaGuardPhase::Draining
                    | QuotaGuardPhase::AwaitingDrainDecision
                    | QuotaGuardPhase::Parked
                    | QuotaGuardPhase::VerifyingReset
            );
            for episode in evaluation.rearmed {
                if !(retain_hard_limit && matches!(episode, EpisodeKey::HardLimit { .. })) {
                    account.fired_episodes.remove(&episode);
                }
            }
            account.breached_windows = if parked_or_verifying { prior_breaches.clone() } else { evaluation.breached_windows };
            account.snapshot = Some(snapshot.clone());
            account.updated_at = now_ms;
            if !snapshot.is_fresh_at(now_ms) {
                account.monitor_healthy = false;
                account.last_error = Some("rate limit snapshot is stale".into());
                return (runtime, effects);
            }
            account.monitor_healthy = true;
            account.last_error = None;
            if parked_or_verifying && full_read {
                if !verification_due && !verification {
                    return (runtime, effects);
                }
                let thresholds_healthy = prior_breaches.iter().all(|kind| snapshot.window(*kind).is_some_and(|window| window.used_percent < match kind {
                    QuotaWindowKind::Primary => settings.primary_threshold_percent,
                    QuotaWindowKind::Secondary => settings.secondary_threshold_percent,
                    QuotaWindowKind::HardLimit => 100,
                }));
                if snapshot.rate_limit_reached_type.is_none() && thresholds_healthy {
                    account.phase = QuotaGuardPhase::Ready;
                    account.verify_at = None;
                    account.breached_windows.clear();
                    account.fired_episodes.retain(|episode| !matches!(episode, EpisodeKey::HardLimit { .. }));
                    account.episode_policy = None;
                    effects.push(ReducerEffect::SetProcessOpen);
                    return (runtime, effects);
                }
                match parked_verification_effect(account, generation) {
                    Ok(effect) => effects.push(effect),
                    Err(error) => enter_intervention(account, now_ms, &error),
                }
                return (runtime, effects);
            }
            let mut action_started = false;
            for episode in evaluation.triggered {
                if !action_started {
                    start_episode(account, episode, settings, now_ms, &mut effects, generation);
                    action_started = true;
                } else {
                    account.fired_episodes.insert(episode);
                }
            }
            if account.phase == QuotaGuardPhase::Monitoring && account.episode_policy.is_none() {
                effects.push(ReducerEffect::SetProcessOpen);
            }
        }
        ReducerEvent::ApplyActionNow { now_ms } => {
            let generation = runtime.lifecycle_generation;
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let Some(snapshot) = account.snapshot.clone() else { return (runtime, effects) };
            if account.phase != QuotaGuardPhase::Monitoring || !snapshot.is_fresh_at(now_ms) || snapshot.rate_limit_reached_type.is_some() { return (runtime, effects) }
            for (kind, threshold) in [(QuotaWindowKind::Primary, settings.primary_threshold_percent), (QuotaWindowKind::Secondary, settings.secondary_threshold_percent)] {
                if snapshot.window(kind).is_some_and(|window| window.used_percent >= threshold) {
                    let key = EpisodeKey::Threshold { account_key: account.account_key.clone(), window: kind, threshold_percent: threshold, resets_at: snapshot.window(kind).and_then(|window| window.resets_at) };
                    if !account.fired_episodes.contains(&key) { start_episode(account, key, settings, now_ms, &mut effects, generation); }
                }
            }
        }
        ReducerEvent::FinalizeClosedEpisode { transition_id, now_ms } => {
            if transition_id != runtime.lifecycle_generation { return (runtime, effects); }
            let operation_id = match next_operation_id(&mut runtime) {
                Ok(value) => value,
                Err(error) => {
                    if let Some(account) = runtime.account.as_mut() { enter_intervention(account, now_ms, &error); }
                    return (runtime, effects);
                }
            };
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            if account.phase != QuotaGuardPhase::Closing { return (runtime, effects); }
            match account.episode_policy.as_ref().map(|policy| policy.action) {
                Some(QuotaAction::InterruptImmediately) => {
                    for start in account.pending_local_starts.values_mut().filter(|start| start.generation == transition_id) {
                        start.disposition = Some(PendingStartDisposition::InterruptOnBind);
                    }
                    if let Err(error) = begin_interrupting(account, account.local_turn_registry.clone(), transition_id, operation_id, now_ms, &mut effects) {
                        enter_intervention(account, now_ms, &error);
                    } else {
                        finish_if_empty(account, transition_id, now_ms, &mut effects);
                    }
                }
                Some(QuotaAction::FinishCurrentTurn) => match add_minutes(now_ms, account.episode_policy.as_ref().expect("policy exists").drain_timeout_minutes) {
                    Some(deadline) => {
                        for start in account.pending_local_starts.values_mut().filter(|start| start.generation == transition_id) {
                            start.disposition = Some(PendingStartDisposition::AllowOnBind);
                        }
                        account.allowed_drain_turns = account.local_turn_registry.clone();
                        if account.allowed_drain_turns.is_empty() && account.pending_local_starts.is_empty() {
                            match parked_verification_effect(account, transition_id) {
                                Ok(effect) => effects.push(effect),
                                Err(error) => enter_intervention(account, now_ms, &error),
                            }
                        } else {
                            account.phase = QuotaGuardPhase::Draining;
                            account.drain_deadline = Some(deadline);
                            effects.push(ReducerEffect::ScheduleDrain { generation: transition_id, deadline });
                        }
                    }
                    None => enter_intervention(account, now_ms, "drain deadline overflow"),
                },
                _ => enter_intervention(account, now_ms, "missing enforcing episode policy"),
            }
        }
        ReducerEvent::DrainDeadline { generation, now_ms } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let operation_id = match next_operation_id(&mut runtime) {
                Ok(value) => value,
                Err(error) => {
                    if let Some(account) = runtime.account.as_mut() { enter_intervention(account, now_ms, &error); }
                    return (runtime, effects);
                }
            };
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            if account.phase != QuotaGuardPhase::Draining || account.drain_deadline.is_some_and(|deadline| now_ms < deadline) { return (runtime, effects) }
            match account.episode_policy.as_ref().map(|policy| policy.drain_timeout_action) {
                Some(super::model::DrainTimeoutAction::NotifyAndHold) => {
                    account.phase = QuotaGuardPhase::AwaitingDrainDecision;
                    account.drain_deadline = None;
                    effects.push(ReducerEffect::Notify { episode: EpisodeKey::HardLimit { account_key: account.account_key.clone() } });
                }
                Some(super::model::DrainTimeoutAction::Interrupt) => {
                    if let Err(error) = begin_interrupting(account, account.allowed_drain_turns.clone(), generation, operation_id, now_ms, &mut effects) {
                        enter_intervention(account, now_ms, &error);
                    } else {
                        finish_if_empty(account, generation, now_ms, &mut effects);
                    }
                }
                None => enter_intervention(account, now_ms, "missing drain policy"),
            }
        }
        ReducerEvent::KeepWaiting { now_ms } => {
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            if account.phase != QuotaGuardPhase::AwaitingDrainDecision { return (runtime, effects) }
            match add_minutes(now_ms, account.episode_policy.as_ref().map(|policy| policy.drain_timeout_minutes).unwrap_or(0)) {
                Some(deadline) => {
                    account.phase = QuotaGuardPhase::Draining;
                    account.drain_deadline = Some(deadline);
                    effects.push(ReducerEffect::ScheduleDrain { generation: runtime.lifecycle_generation, deadline });
                }
                None => enter_intervention(account, now_ms, "drain deadline overflow"),
            }
        }
        ReducerEvent::InterruptAcknowledged { turn, generation, operation_id, attempt, now_ms } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let Some(pending) = account.pending_interrupt_index.get_mut(&turn.stable_id()) else { return (runtime, effects) };
            if pending.generation != generation || pending.operation_id != operation_id || pending.attempt != attempt { return (runtime, effects); }
            let Some(deadline) = now_ms.checked_add(INTERRUPT_COMPLETION_TIMEOUT_MS) else {
                enter_intervention(account, now_ms, "interrupt completion deadline overflow");
                return (runtime, effects);
            };
            pending.acknowledged = true;
            pending.completion_deadline = Some(deadline);
            effects.push(ReducerEffect::ScheduleInterruptCompletion { turn, generation, operation_id, attempt, deadline });
        }
        ReducerEvent::InterruptRequestFailed { turn, generation, operation_id, attempt, now_ms: _ } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let Some(pending) = account.pending_interrupt_index.get(&turn.stable_id()) else { return (runtime, effects) };
            if pending.generation == generation && pending.operation_id == operation_id && pending.attempt == attempt && !pending.acknowledged {
                effects.push(ReducerEffect::ReconcileThread { turn, generation, operation_id, attempt });
            }
        }
        ReducerEvent::InterruptDeadline { turn, generation, operation_id, attempt, acknowledgement, now_ms } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let Some(pending) = account.pending_interrupt_index.get(&turn.stable_id()) else { return (runtime, effects) };
            if pending.generation != generation || pending.operation_id != operation_id || pending.attempt != attempt {
                return (runtime, effects);
            }
            let due = if acknowledgement {
                !pending.acknowledged && now_ms >= pending.ack_deadline
            } else {
                pending.acknowledged && pending.completion_deadline.is_some_and(|deadline| now_ms >= deadline)
            };
            if due {
                effects.push(ReducerEffect::ReconcileThread { turn, generation, operation_id, attempt });
            }
        }
        ReducerEvent::InterruptReconciled { turn, generation, operation_id, attempt, active_turn_id, now_ms } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let replacement_operation = if active_turn_id.as_deref() == Some(turn.turn_id.as_str()) && attempt == 1 {
                match next_operation_id(&mut runtime) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        if let Some(account) = runtime.account.as_mut() { enter_intervention(account, now_ms, &error); }
                        return (runtime, effects);
                    }
                }
            } else { None };
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            let Some(pending) = account.pending_interrupt_index.get(&turn.stable_id()) else { return (runtime, effects) };
            if pending.generation != generation || pending.operation_id != operation_id || pending.attempt != attempt { return (runtime, effects); }
            if let Some(new_operation_id) = replacement_operation {
                let ack_deadline = match now_ms.checked_add(INTERRUPT_ACK_TIMEOUT_MS) {
                    Some(value) => value,
                    None => {
                        enter_intervention(account, now_ms, "interrupt acknowledgement deadline overflow");
                        return (runtime, effects);
                    }
                };
                let retry = PendingInterrupt { turn: turn.clone(), generation, operation_id: new_operation_id, attempt: 2, acknowledged: false, ack_deadline, completion_deadline: None };
                account.insert_pending_interrupt(retry);
                effects.push(ReducerEffect::Interrupt { turn: turn.clone(), generation, operation_id: new_operation_id, attempt: 2 });
                effects.push(ReducerEffect::ScheduleInterruptAck { turn, generation, operation_id: new_operation_id, attempt: 2, deadline: ack_deadline });
            } else if active_turn_id.is_none() {
                account.remove_pending_interrupt(&turn);
                account.local_turn_registry.retain(|candidate| candidate.stable_id() != turn.stable_id());
                finish_if_empty(account, generation, now_ms, &mut effects);
            } else {
                enter_intervention(account, now_ms, "interrupt reconciliation found a different active turn");
            }
        }
        ReducerEvent::InterruptReconcileFailed { turn, generation, operation_id, attempt, reason, now_ms } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            if account.pending_interrupt_index.get(&turn.stable_id()).is_some_and(|pending| pending.generation == generation && pending.operation_id == operation_id && pending.attempt == attempt) {
                enter_intervention(account, now_ms, &format!("interrupt reconciliation failed: {reason}"));
            }
        }
        ReducerEvent::ProvisionalExpired { turn, generation, terminal, now_ms } => {
            if generation != runtime.lifecycle_generation { return (runtime, effects); }
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            if terminal {
                account.terminal_observations.retain(|observation| {
                    observation.generation != generation
                        || observation.turn.stable_id() != turn.stable_id()
                        || now_ms < observation.observed_at.saturating_add(PROVISIONAL_OBSERVATION_TIMEOUT_MS)
                });
            } else {
                account.unmatched_started_turns.retain(|observation| {
                    observation.generation != generation
                        || observation.turn.stable_id() != turn.stable_id()
                        || now_ms < observation.observed_at.saturating_add(PROVISIONAL_OBSERVATION_TIMEOUT_MS)
                });
            }
        }
        ReducerEvent::RehydratePendingInterrupts { now_ms: _ } => {
            let generation = runtime.lifecycle_generation;
            let Some(account) = runtime.account.as_mut() else { return (runtime, effects) };
            for pending in account.pending_interrupt_index.values_mut() {
                if pending.generation == 0 {
                    pending.generation = generation;
                }
            }
            for observation in &mut account.unmatched_started_turns {
                if observation.generation == 0 {
                    observation.generation = generation;
                }
            }
            for observation in &mut account.terminal_observations {
                if observation.generation == 0 {
                    observation.generation = generation;
                }
            }
            for pending in account.pending_interrupt_index.values().filter(|pending| pending.generation == generation) {
                let turn = pending.turn.clone();
                effects.push(ReducerEffect::ReconcileThread {
                    turn: turn.clone(),
                    generation,
                    operation_id: pending.operation_id,
                    attempt: pending.attempt,
                });
                if pending.acknowledged {
                    if let Some(deadline) = pending.completion_deadline {
                        effects.push(ReducerEffect::ScheduleInterruptCompletion {
                            turn,
                            generation,
                            operation_id: pending.operation_id,
                            attempt: pending.attempt,
                            deadline,
                        });
                    }
                } else {
                    effects.push(ReducerEffect::ScheduleInterruptAck {
                        turn,
                        generation,
                        operation_id: pending.operation_id,
                        attempt: pending.attempt,
                        deadline: pending.ack_deadline,
                    });
                }
            }
            for observation in &account.unmatched_started_turns {
                if observation.generation == generation {
                    effects.push(ReducerEffect::ScheduleProvisionalExpiry {
                        turn: observation.turn.clone(),
                        generation,
                        terminal: false,
                        deadline: observation.observed_at.saturating_add(PROVISIONAL_OBSERVATION_TIMEOUT_MS),
                    });
                }
            }
            for observation in &account.terminal_observations {
                if observation.generation == generation {
                    effects.push(ReducerEffect::ScheduleProvisionalExpiry {
                        turn: observation.turn.clone(),
                        generation,
                        terminal: true,
                        deadline: observation.observed_at.saturating_add(PROVISIONAL_OBSERVATION_TIMEOUT_MS),
                    });
                }
            }
        }
    }
    (runtime, effects)
}

pub(crate) fn parked_verification_effect(account: &mut AccountRuntime, generation: u64) -> Result<ReducerEffect, String> {
    let grace = account.episode_policy.as_ref().map(|policy| policy.reset_grace_minutes).ok_or_else(|| "missing episode policy".to_string())?;
    let verify_at = verification_at(account, grace).ok_or_else(|| "missing or overflowing reset timestamp".to_string())?;
    account.verify_at = Some(verify_at);
    account.phase = QuotaGuardPhase::Parked;
    Ok(ReducerEffect::ScheduleVerification { generation, verify_at })
}

#[cfg(test)]
mod tests {
    use crate::shared::quota_guard::model::{QuotaAction, RateLimitSnapshot, RateLimitWindow};
    use crate::types::QuotaGuardSettings;
    use super::{reduce, ReducerEffect, ReducerEvent};

    fn snapshot(percent: u8, hard: bool) -> RateLimitSnapshot { RateLimitSnapshot { primary: Some(RateLimitWindow { used_percent: percent, window_duration_mins: None, resets_at: Some(100) }), secondary: None, credits: None, plan_type: None, rate_limit_reached_type: hard.then(|| "limit".into()), observed_at: 10_000 } }
    #[test]
    fn notify_episode_never_closes_or_interrupts() {
        let settings = QuotaGuardSettings::default(); let (state, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 10_000 }, &settings);
        let (_, effects) = reduce(state, ReducerEvent::Snapshot { snapshot: snapshot(90, true), full_read: true, verification: false, now_ms: 10_000 }, &settings);
        assert!(effects.iter().any(|effect| matches!(effect, ReducerEffect::Notify { .. })));
        assert!(!effects.iter().any(|effect| matches!(effect, ReducerEffect::Interrupt { .. } | ReducerEffect::SetProcessClosed)));
    }
    #[test]
    fn immediate_and_finish_enter_closed_transition_with_checked_deadline() {
        let mut settings = QuotaGuardSettings::default(); settings.action = QuotaAction::FinishCurrentTurn;
        let (state, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 10_000 }, &settings);
        let (state, effects) = reduce(state, ReducerEvent::Snapshot { snapshot: snapshot(90, true), full_read: true, verification: false, now_ms: 10_000 }, &settings);
        assert!(effects.contains(&ReducerEffect::SetProcessClosed));
        let generation = state.lifecycle_generation;
        let mut state = state;
        state.account.as_mut().unwrap().local_turn_registry.push(crate::shared::quota_guard::model::TurnKey {
            session_epoch: "epoch".into(), workspace_id: "workspace".into(), thread_id: "thread".into(), turn_id: "turn".into(),
        });
        let (_, final_effects) = reduce(state, ReducerEvent::FinalizeClosedEpisode { transition_id: generation, now_ms: 10_000 }, &settings);
        assert!(matches!(final_effects.as_slice(), [ReducerEffect::ScheduleDrain { deadline: 910_000, .. }]));
    }
    #[test]
    fn disable_fences_prior_finalization() {
        let settings = QuotaGuardSettings::default(); let (state, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 1 }, &settings); let generation = state.lifecycle_generation;
        let (state, _) = reduce(state, ReducerEvent::Disable { now_ms: 2 }, &settings); let (_, effects) = reduce(state, ReducerEvent::FinalizeClosedEpisode { transition_id: generation, now_ms: 3 }, &settings);
        assert!(effects.is_empty());
    }
    #[test]
    fn stale_snapshot_updates_health_without_executing_hard_limit() {
        let settings = QuotaGuardSettings::default();
        let (state, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 700_001 }, &settings);
        let (state, effects) = reduce(state, ReducerEvent::Snapshot { snapshot: snapshot(90, true), full_read: true, verification: false, now_ms: 700_001 }, &settings);
        assert!(effects.is_empty());
        assert!(!state.account.unwrap().monitor_healthy);
    }

    #[test]
    fn drain_interrupt_timeout_persists_exact_targets_before_effects() {
        let mut settings = QuotaGuardSettings::default();
        settings.action = QuotaAction::FinishCurrentTurn;
        settings.drain_timeout_action = crate::types::DrainTimeoutAction::Interrupt;
        let (state, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 10_000 }, &settings);
        let (mut state, _) = reduce(state, ReducerEvent::Snapshot { snapshot: snapshot(90, true), full_read: true, verification: false, now_ms: 10_000 }, &settings);
        let generation = state.lifecycle_generation;
        state.account.as_mut().unwrap().local_turn_registry.push(crate::shared::quota_guard::model::TurnKey {
            session_epoch: "epoch".into(), workspace_id: "workspace".into(), thread_id: "thread".into(), turn_id: "turn".into(),
        });
        let (state, _) = reduce(state, ReducerEvent::FinalizeClosedEpisode { transition_id: generation, now_ms: 10_000 }, &settings);
        let (state, effects) = reduce(state, ReducerEvent::DrainDeadline { generation, now_ms: 910_000 }, &settings);
        let account = state.account.unwrap();
        assert_eq!(account.pending_interrupt_index.len(), 1);
        assert!(effects.iter().any(|effect| matches!(effect, ReducerEffect::Interrupt { turn, .. } if turn.turn_id == "turn")));
    }

    #[test]
    fn disable_then_reenable_executes_same_hard_limit_under_new_generation() {
        let settings = QuotaGuardSettings::default();
        let (state, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 10_000 }, &settings);
        let (state, first) = reduce(state, ReducerEvent::Snapshot { snapshot: snapshot(10, true), full_read: true, verification: false, now_ms: 10_000 }, &settings);
        assert!(first.iter().any(|effect| matches!(effect, ReducerEffect::Notify { .. })));
        let (state, _) = reduce(state, ReducerEvent::Disable { now_ms: 10_001 }, &settings);
        let (state, _) = reduce(state, ReducerEvent::Enable { account_key: "a".into(), now_ms: 10_002 }, &settings);
        let (_, second) = reduce(state, ReducerEvent::Snapshot { snapshot: snapshot(10, true), full_read: true, verification: false, now_ms: 10_002 }, &settings);
        assert!(second.iter().any(|effect| matches!(effect, ReducerEffect::Notify { .. })));
    }

    #[test]
    fn acknowledged_interrupt_ignores_stale_ack_timer_and_early_completion_timer() {
        let mut settings = QuotaGuardSettings::default();
        settings.action = QuotaAction::InterruptImmediately;
        let (mut state, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 10_000 }, &settings);
        let turn = crate::shared::quota_guard::model::TurnKey {
            session_epoch: "epoch".into(), workspace_id: "workspace".into(), thread_id: "thread".into(), turn_id: "turn".into(),
        };
        state.account.as_mut().unwrap().local_turn_registry.push(turn.clone());
        let (state, _) = reduce(state, ReducerEvent::Snapshot { snapshot: snapshot(89, false), full_read: true, verification: false, now_ms: 10_000 }, &settings);
        let (state, _) = reduce(state, ReducerEvent::Snapshot { snapshot: snapshot(90, false), full_read: true, verification: false, now_ms: 10_001 }, &settings);
        let generation = state.lifecycle_generation;
        let (state, _) = reduce(state, ReducerEvent::FinalizeClosedEpisode { transition_id: generation, now_ms: 10_001 }, &settings);
        let pending = state.account.as_ref().unwrap().pending_interrupt_index.get(&turn.stable_id()).unwrap().clone();
        let (state, _) = reduce(state, ReducerEvent::InterruptAcknowledged {
            turn: turn.clone(), generation: pending.generation, operation_id: pending.operation_id, attempt: pending.attempt, now_ms: 10_002,
        }, &settings);
        let (state, stale_ack_effects) = reduce(state, ReducerEvent::InterruptDeadline {
            turn: turn.clone(), generation: pending.generation, operation_id: pending.operation_id, attempt: pending.attempt, acknowledgement: true, now_ms: pending.ack_deadline,
        }, &settings);
        assert!(stale_ack_effects.is_empty());
        let completion_deadline = state.account.as_ref().unwrap().pending_interrupt_index.get(&turn.stable_id()).unwrap().completion_deadline.unwrap();
        let (state, early_completion_effects) = reduce(state, ReducerEvent::InterruptDeadline {
            turn: turn.clone(), generation: pending.generation, operation_id: pending.operation_id, attempt: pending.attempt, acknowledgement: false, now_ms: completion_deadline - 1,
        }, &settings);
        assert!(early_completion_effects.is_empty());
        let (_, due_effects) = reduce(state, ReducerEvent::InterruptDeadline {
            turn, generation: pending.generation, operation_id: pending.operation_id, attempt: pending.attempt, acknowledgement: false, now_ms: completion_deadline,
        }, &settings);
        assert!(matches!(due_effects.as_slice(), [ReducerEffect::ReconcileThread { .. }]));
    }

    #[test]
    fn stale_same_thread_terminal_cannot_consume_a_pending_start() {
        let settings = QuotaGuardSettings::default();
        let (state, _) = reduce(Default::default(), ReducerEvent::Enable { account_key: "a".into(), now_ms: 0 }, &settings);
        let generation = state.lifecycle_generation;
        let pending = crate::shared::quota_guard::model::PendingLocalStart {
            request_id: 1, session_epoch: "epoch".into(), workspace_id: "workspace".into(),
            request_thread_id: Some("thread".into()), expected_thread_id: Some("thread".into()),
            request_kind: "turn/start".into(), response_thread_id: None, response_received_at: None,
            generation, disposition: None, registered_at: 0,
        };
        let (state, _) = reduce(state, ReducerEvent::PendingStartRecorded { start: pending, now_ms: 0 }, &settings);
        let stale = crate::shared::quota_guard::model::TurnKey {
            session_epoch: "epoch".into(), workspace_id: "workspace".into(), thread_id: "thread".into(), turn_id: "old-turn".into(),
        };
        let (state, _) = reduce(state, ReducerEvent::TurnTerminal { turn: stale, status: "completed".into(), error: None, now_ms: 1 }, &settings);
        let (state, _) = reduce(state, ReducerEvent::StartResponse {
            request_id: 1, session_epoch: "epoch".into(), workspace_id: "workspace".into(), thread_id: Some("thread".into()), now_ms: 2,
        }, &settings);
        let account = state.account.unwrap();
        assert!(account.terminal_observations.is_empty());
        assert!(account.pending_local_starts.contains_key(&1));
    }
}
