use axum::http::StatusCode;
use serde_json::json;

use crate::libs::error::HttpError;
use crate::libs::state;

/// Mirrors src/lib/rate-limit.ts `checkRateLimit`. Operates on the global state:
/// reads `rate_limit_seconds`, compares against `last_request_timestamp`, and
/// either throws a 429 HttpError or waits.
pub async fn check_rate_limit() -> Result<(), HttpError> {
    let rate_limit_seconds = match state::with_state(|s| s.rate_limit_seconds) {
        Some(v) => v,
        None => return Ok(()),
    };

    let now = now_millis();

    let last = state::with_state(|s| s.last_request_timestamp);
    let last = match last {
        Some(ts) => ts,
        None => {
            state::with_state_mut(|s| s.last_request_timestamp = Some(now));
            return Ok(());
        }
    };

    let elapsed_seconds = (now - last) as f64 / 1000.0;

    if elapsed_seconds > rate_limit_seconds as f64 {
        state::with_state_mut(|s| s.last_request_timestamp = Some(now));
        return Ok(());
    }

    let wait_time_seconds = (rate_limit_seconds as f64 - elapsed_seconds).ceil() as i64;

    if !state::with_state(|s| s.rate_limit_wait) {
        tracing::warn!("Rate limit exceeded. Need to wait {wait_time_seconds} more seconds.");
        return Err(HttpError::new(
            "Rate limit exceeded",
            StatusCode::TOO_MANY_REQUESTS,
            Default::default(),
            json!({ "message": "Rate limit exceeded" }).to_string(),
        ));
    }

    let wait_time_ms = (wait_time_seconds * 1000).max(0) as u64;
    tracing::warn!("Rate limit reached. Waiting {wait_time_seconds} seconds before proceeding...");
    tokio::time::sleep(std::time::Duration::from_millis(wait_time_ms)).await;
    state::with_state_mut(|s| s.last_request_timestamp = Some(now));
    tracing::info!("Rate limit wait completed, proceeding with request");
    Ok(())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
