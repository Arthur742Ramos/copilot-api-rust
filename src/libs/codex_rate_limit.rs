//! Port of `src/lib/codex-rate-limit.ts`.
//!
//! Parses Codex `type: "codex.rate_limits"` stream events and logs the
//! primary/secondary window usage + reset. Logging-only; no response shape
//! change.

use chrono::{Local, TimeZone, Utc};
use serde_json::Value;

const CODEX_RATE_LIMIT_SCOPES: [&str; 2] = ["primary", "secondary"];

/// Mirrors `formatCodexRateLimitResetAt`: render `reset_at` (unix seconds) as a
/// human-readable local timestamp, falling back to the raw value when it cannot
/// be converted.
pub fn format_codex_rate_limit_reset_at(reset_at: f64) -> String {
    if !reset_at.is_finite() {
        return format_reset_at_raw(reset_at);
    }
    let millis = (reset_at * 1000.0) as i64;
    match Utc.timestamp_millis_opt(millis).single() {
        Some(dt) => dt
            .with_timezone(&Local)
            .format("%-m/%-d/%Y, %-I:%M:%S %p")
            .to_string(),
        None => format_reset_at_raw(reset_at),
    }
}

/// `String(resetAt)` — integers print without a trailing `.0`.
fn format_reset_at_raw(reset_at: f64) -> String {
    if reset_at.fract() == 0.0 && reset_at.is_finite() {
        format!("{}", reset_at as i64)
    } else {
        format!("{reset_at}")
    }
}

/// Mirrors `logCodexRateLimitsEvent`: log the primary/secondary rate-limit
/// windows from a `codex.rate_limits` event. Accepts any parsed stream event;
/// non-matching events are ignored.
pub fn log_codex_rate_limits_event(event: &Value) {
    let Some(event_record) = event.as_object() else {
        return;
    };
    if event_record.get("type").and_then(Value::as_str) != Some("codex.rate_limits") {
        return;
    }

    let Some(rate_limits) = event_record.get("rate_limits").and_then(Value::as_object) else {
        return;
    };

    let plan_type = event_record.get("plan_type").and_then(Value::as_str);
    let allowed = rate_limits.get("allowed").and_then(Value::as_bool);
    let limit_reached = rate_limits.get("limit_reached").and_then(Value::as_bool);

    for scope in CODEX_RATE_LIMIT_SCOPES {
        let Some(window) = rate_limits
            .get(scope)
            .and_then(parse_codex_rate_limit_window)
        else {
            continue;
        };

        let mut summary: Vec<String> = Vec::new();
        if let Some(allowed) = allowed {
            summary.push(format!("allowed={allowed}"));
        }
        if let Some(limit_reached) = limit_reached {
            summary.push(format!("limit_reached={limit_reached}"));
        }
        summary.push(format!("used={}%", format_number(window.used_percent)));
        summary.push(format!(
            "reset_at={}",
            format_codex_rate_limit_reset_at(window.reset_at)
        ));

        let label = match plan_type {
            Some(plan) => format!("Codex {scope} rate limit ({plan})"),
            None => format!("Codex {scope} rate limit"),
        };
        tracing::info!("{label}: {}", summary.join(", "));
    }
}

/// A Codex rate-limit window. All four numeric fields must be present for the
/// window to be considered valid (mirrors `isCodexRateLimitWindow`).
struct CodexRateLimitWindow {
    used_percent: f64,
    reset_at: f64,
}

/// Mirrors `isCodexRateLimitWindow`: require all four numeric fields.
fn parse_codex_rate_limit_window(value: &Value) -> Option<CodexRateLimitWindow> {
    let record = value.as_object()?;
    let used_percent = record.get("used_percent").and_then(Value::as_f64)?;
    let reset_at = record.get("reset_at").and_then(Value::as_f64)?;
    // The two remaining fields are required to validate the shape even though
    // they are not logged directly.
    record.get("reset_after_seconds").and_then(Value::as_f64)?;
    record.get("window_minutes").and_then(Value::as_f64)?;
    Some(CodexRateLimitWindow {
        used_percent,
        reset_at,
    })
}

/// Render a number the way `${value}` would: integers without a decimal point.
fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_valid_window() {
        let window = parse_codex_rate_limit_window(&json!({
            "reset_after_seconds": 60,
            "reset_at": 1_700_000_000,
            "used_percent": 42,
            "window_minutes": 5,
        }))
        .expect("valid window");
        assert_eq!(window.used_percent, 42.0);
        assert_eq!(window.reset_at, 1_700_000_000.0);
    }

    #[test]
    fn rejects_window_missing_fields() {
        // Missing window_minutes.
        assert!(parse_codex_rate_limit_window(&json!({
            "reset_after_seconds": 60,
            "reset_at": 1_700_000_000,
            "used_percent": 42,
        }))
        .is_none());
        // Not an object.
        assert!(parse_codex_rate_limit_window(&json!("nope")).is_none());
    }

    #[test]
    fn ignores_non_rate_limit_events() {
        // Should not panic / no validation needed — exercises the early returns.
        log_codex_rate_limits_event(&json!({ "type": "response.completed" }));
        log_codex_rate_limits_event(&json!({ "type": "codex.rate_limits" }));
        log_codex_rate_limits_event(&json!("not an object"));
    }

    #[test]
    fn logs_codex_rate_limits_event_smoke() {
        // A full, valid event drives the formatting/log path without panicking.
        log_codex_rate_limits_event(&json!({
            "type": "codex.rate_limits",
            "plan_type": "pro",
            "rate_limits": {
                "allowed": true,
                "limit_reached": false,
                "primary": {
                    "reset_after_seconds": 60,
                    "reset_at": 1_700_000_000,
                    "used_percent": 12,
                    "window_minutes": 5,
                },
                "secondary": {
                    "reset_after_seconds": 120,
                    "reset_at": 1_700_000_500,
                    "used_percent": 30,
                    "window_minutes": 60,
                },
            },
        }));
    }

    #[test]
    fn format_reset_at_handles_non_finite() {
        assert_eq!(format_codex_rate_limit_reset_at(f64::NAN), "NaN");
    }
}
