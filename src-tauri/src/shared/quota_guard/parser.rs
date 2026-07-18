use serde_json::Value;

use super::model::{RateLimitSnapshot, RateLimitWindow};

fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    value.get(camel).or_else(|| value.get(snake))
}

fn non_negative_integer(value: &Value, name: &str) -> Result<i64, String> {
    value
        .as_i64()
        .filter(|value| *value >= 0)
        .ok_or_else(|| format!("{name} must be a non-negative integer"))
}

fn optional_non_negative_integer(value: &Value, camel: &str, snake: &str, prior: Option<i64>) -> Result<Option<i64>, String> {
    match field(value, camel, snake) {
        Some(Value::Null) => Ok(None),
        Some(raw) => Ok(Some(non_negative_integer(raw, camel)?)),
        None => Ok(prior),
    }
}

fn parse_used_percent(value: &Value, previous: Option<&RateLimitWindow>) -> Result<Option<u8>, String> {
    if let Some(raw) = field(value, "usedPercent", "used_percent") {
        if raw.is_null() {
            return Ok(previous.map(|window| window.used_percent));
        }
        let used = non_negative_integer(raw, "usedPercent")?;
        return u8::try_from(used)
            .ok()
            .filter(|used| *used <= 100)
            .map(Some)
            .ok_or_else(|| "used percent is outside 0..=100".to_string());
    }
    if let Some(raw) = field(value, "remainingPercent", "remaining_percent").or_else(|| value.get("remaining")) {
        if raw.is_null() {
            return Ok(previous.map(|window| window.used_percent));
        }
        let remaining = non_negative_integer(raw, "remainingPercent")?;
        return u8::try_from(remaining)
            .ok()
            .filter(|remaining| *remaining <= 100)
            .map(|remaining| Some(100 - remaining))
            .ok_or_else(|| "remaining percent is outside 0..=100".to_string());
    }
    Ok(previous.map(|window| window.used_percent))
}

fn parse_window(value: &Value, previous: Option<&RateLimitWindow>) -> Result<Option<RateLimitWindow>, String> {
    if !value.is_object() {
        return Err("rate limit window must be an object".into());
    }
    let Some(used_percent) = parse_used_percent(value, previous)? else {
        return Ok(None);
    };
    let duration = optional_non_negative_integer(
        value,
        "windowDurationMins",
        "window_duration_mins",
        previous.and_then(|window| window.window_duration_mins).map(|value| value as i64),
    )?
    .map(|value| u64::try_from(value).expect("non-negative i64 fits u64"));
    let resets_at = optional_non_negative_integer(
        value,
        "resetsAt",
        "resets_at",
        previous.and_then(|window| window.resets_at),
    )?;
    Ok(Some(RateLimitWindow { used_percent, window_duration_mins: duration, resets_at }))
}

fn optional_string(value: &Value, camel: &str, snake: &str, prior: Option<String>) -> Result<Option<String>, String> {
    match field(value, camel, snake) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(format!("{camel} must be a non-empty string or null")),
        None => Ok(prior),
    }
}

/// Parses both Codex spellings and sparse update payloads. Missing fields
/// inherit their prior value; only an explicit JSON null clears nullable
/// metadata. Invalid protocol data is rejected so it can update monitor health
/// but can never manufacture a targetable quota episode.
pub(crate) fn parse_rate_limits(value: &Value, previous: Option<&RateLimitSnapshot>, observed_at: i64) -> Result<RateLimitSnapshot, String> {
    if observed_at < 0 {
        return Err("observedAt must be a non-negative Unix millisecond timestamp".into());
    }
    let envelope = value.get("result").unwrap_or(value);
    let source = envelope.get("rateLimits").or_else(|| envelope.get("rate_limits")).unwrap_or(envelope);
    if !source.is_object() {
        return Err("rate limits must be an object".into());
    }
    let parse_kind = |camel: &str, snake: &str, prior: Option<&RateLimitWindow>| -> Result<Option<RateLimitWindow>, String> {
        match field(source, camel, snake) {
            Some(Value::Null) => Ok(None),
            Some(raw) => parse_window(raw, prior),
            None => Ok(prior.cloned()),
        }
    };
    let primary = parse_kind("primary", "primary", previous.and_then(|snapshot| snapshot.primary.as_ref()))?;
    let secondary = parse_kind("secondary", "secondary", previous.and_then(|snapshot| snapshot.secondary.as_ref()))?;
    let rate_limit_reached_type = optional_string(
        source,
        "rateLimitReachedType",
        "rate_limit_reached_type",
        previous.and_then(|snapshot| snapshot.rate_limit_reached_type.clone()),
    )?;
    let plan_type = optional_string(source, "planType", "plan_type", previous.and_then(|snapshot| snapshot.plan_type.clone()))?;
    let credits = match field(source, "credits", "credits") {
        Some(Value::Null) => None,
        Some(value) => Some(value.clone()),
        None => previous.and_then(|snapshot| snapshot.credits.clone()),
    };
    Ok(RateLimitSnapshot { primary, secondary, credits, plan_type, rate_limit_reached_type, observed_at })
}

pub(crate) fn is_usage_limit_exceeded(error: &Value) -> bool {
    let candidate = error.get("codexErrorInfo")
        .or_else(|| error.get("codex_error_info"))
        .or_else(|| error.get("error").and_then(|nested| nested.get("codexErrorInfo")))
        .or_else(|| error.get("error").and_then(|nested| nested.get("codex_error_info")));
    matches!(candidate, Some(Value::String(value)) if value == "usageLimitExceeded")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{is_usage_limit_exceeded, parse_rate_limits};
    #[test]
    fn parses_full_json_rpc_rate_limits_response() {
        let snapshot = parse_rate_limits(
            &json!({
                "id": 42,
                "result": {
                    "rateLimits": {
                        "primary": { "usedPercent": 67, "windowDurationMins": 300, "resetsAt": 2_000 },
                        "secondary": { "usedPercent": 12, "resetsAt": 3_000 },
                        "planType": "pro"
                    }
                }
            }),
            None,
            1,
        )
        .unwrap();
        assert_eq!(snapshot.primary.expect("primary").used_percent, 67);
        assert_eq!(snapshot.secondary.expect("secondary").resets_at, Some(3_000));
        assert_eq!(snapshot.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn rejects_malformed_json_rpc_rate_limits_envelope() {
        assert!(parse_rate_limits(&json!({ "result": { "rateLimits": 17 } }), None, 1).is_err());
        assert!(parse_rate_limits(&json!({ "result": 17 }), None, 1).is_err());
    }


    #[test]
    fn sparse_snake_case_update_preserves_existing_window_fields_and_hard_limit() {
        let initial = parse_rate_limits(&json!({"primary":{"usedPercent":40,"resetsAt":42},"rateLimitReachedType":"hard"}), None, 1).unwrap();
        let sparse = parse_rate_limits(&json!({"primary":{"used_percent":90}}), Some(&initial), 2).unwrap();
        assert_eq!(sparse.primary.unwrap().resets_at, Some(42));
        assert_eq!(sparse.rate_limit_reached_type.as_deref(), Some("hard"));
    }

    #[test]
    fn explicit_null_clears_hard_limit_only_when_present() {
        let initial = parse_rate_limits(&json!({"rateLimitReachedType":"hard"}), None, 1).unwrap();
        let cleared = parse_rate_limits(&json!({"rate_limit_reached_type":null}), Some(&initial), 2).unwrap();
        assert_eq!(cleared.rate_limit_reached_type, None);
    }

    #[test]
    fn malformed_percentage_timestamp_and_observed_time_are_rejected() {
        assert!(parse_rate_limits(&json!({"primary":{"usedPercent":101}}), None, 1).is_err());
        assert!(parse_rate_limits(&json!({"primary":{"remaining":101}}), None, 1).is_err());
        assert!(parse_rate_limits(&json!({"primary":{"usedPercent":1,"resetsAt":"tomorrow"}}), None, 1).is_err());
        assert!(parse_rate_limits(&json!({"primary":{"usedPercent":1}}), None, -1).is_err());
    }

    #[test]
    fn recognizes_terminal_usage_limit_in_both_protocol_shapes() {
        assert!(is_usage_limit_exceeded(&json!({"codexErrorInfo":"usageLimitExceeded"})));
        assert!(is_usage_limit_exceeded(&json!({"error":{"codex_error_info":"usageLimitExceeded"}})));
        assert!(!is_usage_limit_exceeded(&json!({"codexErrorInfo":"other"})));
    }
}
