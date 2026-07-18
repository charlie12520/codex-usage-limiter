use std::collections::BTreeSet;

use crate::types::QuotaGuardSettings;

use super::model::{EpisodeKey, QuotaWindowKind, RateLimitSnapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Evaluation {
    pub(crate) triggered: Vec<EpisodeKey>,
    pub(crate) breached_windows: BTreeSet<QuotaWindowKind>,
    pub(crate) rearmed: Vec<EpisodeKey>,
    pub(crate) baseline: bool,
}

pub(crate) fn evaluate_snapshot(
    account_key: &str,
    current: &RateLimitSnapshot,
    previous: Option<&RateLimitSnapshot>,
    settings: &QuotaGuardSettings,
    fired: &BTreeSet<EpisodeKey>,
    is_full_read: bool,
) -> Evaluation {
    // Only a full authoritative read establishes the percentage baseline.
    // Sparse events before it must neither execute nor suppress the baseline.
    let baseline = previous.is_none() && is_full_read;
    let mut triggered = Vec::new();
    let mut rearmed = Vec::new();
    let mut breached_windows = BTreeSet::new();
    let policy = [(QuotaWindowKind::Primary, settings.primary_threshold_percent), (QuotaWindowKind::Secondary, settings.secondary_threshold_percent)];

    for (kind, threshold) in policy {
        let Some(window) = current.window(kind) else { continue };
        if window.used_percent >= threshold { breached_windows.insert(kind); }
        let key = EpisodeKey::Threshold {
            account_key: account_key.to_string(),
            window: kind,
            threshold_percent: threshold,
            resets_at: window.resets_at,
        };
        let previous_window = previous.and_then(|snapshot| snapshot.window(kind));
        let crossed = previous_window
            .map(|old| old.used_percent < threshold && window.used_percent >= threshold)
            .unwrap_or(false);
        let reset_changed = previous_window
            .map(|old| old.resets_at != window.resets_at)
            .unwrap_or(false);
        let hysteresis = window.used_percent < threshold.saturating_sub(2);
        let same_family: Vec<_> = fired
            .iter()
            .filter(|entry| matches!(entry, EpisodeKey::Threshold {
                account_key: existing_account,
                window: existing_window,
                threshold_percent: existing_threshold,
                ..
            } if existing_account.as_str() == account_key && *existing_window == kind && *existing_threshold == threshold))
            .cloned()
            .collect();
        if reset_changed || hysteresis {
            rearmed.extend(same_family);
        }
        // Rearming is applied by the caller with the evaluation result.  A new
        // reset timestamp deliberately has a different episode key, so it may
        // trigger immediately when already over threshold.
        if (crossed || (reset_changed && window.used_percent >= threshold)) && !fired.contains(&key) {
            triggered.push(key);
        }
    }

    let hard_key = EpisodeKey::HardLimit { account_key: account_key.to_string() };
    if current.rate_limit_reached_type.is_some() {
        if !fired.contains(&hard_key) { triggered.push(hard_key); }
    } else if is_full_read && fired.contains(&hard_key) {
        rearmed.push(hard_key);
    }
    // Percentage data on bootstrap establishes only a baseline. A hard limit is
    // deliberately evaluated above and is never baseline-suppressed.
    if baseline { triggered.retain(|key| matches!(key, EpisodeKey::HardLimit { .. })); }
    Evaluation { triggered, breached_windows, rearmed, baseline }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use crate::types::QuotaGuardSettings;
    use super::evaluate_snapshot;
    use crate::shared::quota_guard::model::{EpisodeKey, RateLimitSnapshot, RateLimitWindow};

    fn snapshot(percent: u8, reset: i64, hard: bool) -> RateLimitSnapshot {
        RateLimitSnapshot { primary: Some(RateLimitWindow { used_percent: percent, window_duration_mins: None, resets_at: Some(reset) }), secondary: None, credits: None, plan_type: None, rate_limit_reached_type: hard.then(|| "hard".into()), observed_at: 1 }
    }

    #[test]
    fn baseline_suppresses_threshold_but_not_hard_limit() {
        let settings = QuotaGuardSettings::default();
        let result = evaluate_snapshot("account", &snapshot(90, 10, true), None, &settings, &BTreeSet::new(), true);
        assert_eq!(result.triggered, vec![EpisodeKey::HardLimit { account_key: "account".into() }]);
    }

    #[test]
    fn threshold_crosses_once_and_rearms_after_hysteresis() {
        let settings = QuotaGuardSettings::default();
        let base = snapshot(89, 10, false);
        let crossed = snapshot(90, 10, false);
        let event = evaluate_snapshot("account", &crossed, Some(&base), &settings, &BTreeSet::new(), true);
        let key = event.triggered[0].clone();
        let mut fired = BTreeSet::new(); fired.insert(key.clone());
        assert!(evaluate_snapshot("account", &crossed, Some(&crossed), &settings, &fired, true).triggered.is_empty());
        let low = snapshot(87, 10, false);
        assert_eq!(evaluate_snapshot("account", &low, Some(&crossed), &settings, &fired, true).rearmed, vec![key]);
    }

    #[test]
    fn primary_and_secondary_thresholds_are_independent() {
        let settings = QuotaGuardSettings::default();
        let prior = RateLimitSnapshot {
            primary: Some(RateLimitWindow { used_percent: 89, window_duration_mins: None, resets_at: Some(10) }),
            secondary: Some(RateLimitWindow { used_percent: 89, window_duration_mins: None, resets_at: Some(20) }),
            credits: None, plan_type: None, rate_limit_reached_type: None, observed_at: 1,
        };
        let current = RateLimitSnapshot {
            primary: Some(RateLimitWindow { used_percent: 90, window_duration_mins: None, resets_at: Some(10) }),
            secondary: Some(RateLimitWindow { used_percent: 89, window_duration_mins: None, resets_at: Some(20) }),
            ..prior.clone()
        };
        let result = evaluate_snapshot("account", &current, Some(&prior), &settings, &BTreeSet::new(), true);
        assert_eq!(result.triggered.len(), 1);
        assert!(matches!(result.triggered[0], EpisodeKey::Threshold { window: crate::shared::quota_guard::model::QuotaWindowKind::Primary, .. }));
    }

    #[test]
    fn hard_limit_rearms_only_after_authoritative_clear() {
        let settings = QuotaGuardSettings::default();
        let hard = snapshot(10, 10, true);
        let key = EpisodeKey::HardLimit { account_key: "account".into() };
        let mut fired = BTreeSet::new();
        fired.insert(key.clone());
        assert!(evaluate_snapshot("account", &hard, Some(&hard), &settings, &fired, false).rearmed.is_empty());
        let clear = snapshot(10, 10, false);
        assert!(evaluate_snapshot("account", &clear, Some(&hard), &settings, &fired, false).rearmed.is_empty());
        assert_eq!(evaluate_snapshot("account", &clear, Some(&hard), &settings, &fired, true).rearmed, vec![key]);
    }

    #[test]
    fn reset_timestamp_rearms_a_threshold_and_sparse_first_event_is_not_baseline() {
        let settings = QuotaGuardSettings::default();
        let prior = snapshot(90, 10, false);
        let key = EpisodeKey::Threshold {
            account_key: "account".into(),
            window: crate::shared::quota_guard::model::QuotaWindowKind::Primary,
            threshold_percent: 90,
            resets_at: Some(10),
        };
        let mut fired = BTreeSet::new();
        fired.insert(key.clone());
        let next = snapshot(90, 20, false);
        let result = evaluate_snapshot("account", &next, Some(&prior), &settings, &fired, true);
        assert_eq!(result.rearmed, vec![key]);
        assert_eq!(result.triggered.len(), 1);
        assert!(!evaluate_snapshot("account", &next, None, &settings, &BTreeSet::new(), false).baseline);
    }
}
