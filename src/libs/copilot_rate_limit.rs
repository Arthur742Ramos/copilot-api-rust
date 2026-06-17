use axum::http::HeaderMap;

/// Mirrors src/lib/copilot-rate-limit.ts. Parses the `x-usage-ratelimit-*`
/// headers and logs the remaining quota.
const RATE_LIMIT_TYPES: [(&str, &str); 2] = [
    ("session", "x-usage-ratelimit-session"),
    ("weekly", "x-usage-ratelimit-weekly"),
];

struct RateLimitUsage {
    type_name: &'static str,
    remaining: String,
    reset_at: String,
}

fn get_header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Parse a `rem=..&rst=..` urlencoded header value.
fn parse_rate_limit_header(header_value: &str) -> Option<(String, String)> {
    let mut remaining = None;
    let mut reset_at = None;
    for pair in header_value.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next().unwrap_or("");
        let value = it.next().unwrap_or("");
        match key {
            "rem" => remaining = Some(decode_component(value)),
            "rst" => reset_at = Some(decode_component(value)),
            _ => {}
        }
    }
    match (remaining, reset_at) {
        (Some(rem), Some(rst)) if !rem.is_empty() && !rst.is_empty() => Some((rem, rst)),
        _ => None,
    }
}

fn decode_component(value: &str) -> String {
    // URLSearchParams decodes '+' to space and percent-escapes; the upstream
    // values are simple numbers/timestamps, so a light decode suffices.
    value.replace('+', " ")
}

fn get_rate_limit_usage(
    headers: &HeaderMap,
    type_name: &'static str,
    header_name: &str,
) -> Option<RateLimitUsage> {
    let header_value = get_header_value(headers, header_name)?;
    let (remaining, reset_at) = parse_rate_limit_header(&header_value)?;
    Some(RateLimitUsage {
        type_name,
        remaining,
        reset_at,
    })
}

pub fn log_copilot_rate_limits(headers: &HeaderMap) {
    for (type_name, header_name) in RATE_LIMIT_TYPES {
        if let Some(usage) = get_rate_limit_usage(headers, type_name, header_name) {
            tracing::info!(
                "Copilot {} quota remaining: {}, resets at: {}",
                usage.type_name,
                usage.remaining,
                usage.reset_at
            );
            record_rate_limit_metrics(&usage);
        }
    }
}

/// Surface the parsed quota as bounded Prometheus gauges so the one failure mode
/// that disables the whole proxy (a session/weekly cap exhaustion → upstream
/// 429/529) is alertable as a leading indicator, not just visible in logs.
/// `type` is bounded to {session, weekly}; values that don't parse are skipped.
fn record_rate_limit_metrics(usage: &RateLimitUsage) {
    if let Ok(remaining) = usage.remaining.trim().parse::<f64>() {
        metrics::gauge!("copilot_rate_limit_remaining", "type" => usage.type_name).set(remaining);
    }
    // `rst` is a unix timestamp (seconds); expose seconds-until-reset, clamped at
    // 0, which is more directly useful for alerting than the raw epoch.
    if let Ok(reset_at) = usage.reset_at.trim().parse::<i64>() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let seconds_until = (reset_at - now).max(0) as f64;
        metrics::gauge!("copilot_rate_limit_reset_seconds", "type" => usage.type_name)
            .set(seconds_until);
    }
}
