//! Daily token-budget admission control.
//!
//! When `dailyTokenBudget` is configured, requests are rejected with a 429 once
//! the total tokens recorded for the current local day reach the cap. The cap is
//! a guardrail against a runaway client or leaked key burning unlimited Copilot
//! quota; it is intentionally a coarse cumulative-spend gate, not a hard
//! per-request reservation.
//!
//! Two nuances drive the design:
//! - Usage is recorded *after* a response completes (SSE finalizers), so
//!   enforcement gates on spend-so-far and can overshoot by the tokens of the
//!   requests already in flight when the cap is crossed.
//! - `get_token_usage_summary` is a synchronous SQLite read (via the token-usage
//!   connection pool in [`crate::libs::sqlite`]). Re-querying it on every request
//!   would add a pooled DB round-trip to the hot path, so the daily total is
//!   cached with a short TTL and only refreshed when stale. The refresh runs
//!   while holding the cache lock, so a burst of concurrent requests that find
//!   the cache stale collapses into a single DB read rather than a thundering
//!   herd.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use once_cell::sync::Lazy;
use serde_json::json;

use crate::libs::config::get_daily_token_budget;
use crate::libs::error::HttpError;
use crate::libs::token_usage::get_token_usage_summary;

/// How long a cached daily total stays fresh before the next admission check
/// re-reads it from SQLite. Short enough that the cap engages promptly, long
/// enough that a burst of requests doesn't repeatedly hit the usage store.
const CACHE_TTL: Duration = Duration::from_secs(5);

struct CachedTotal {
    /// The local-day key (YYYYMMDD) the cached total belongs to, so a rollover
    /// past midnight invalidates the cache even within the TTL window.
    day_key: i32,
    total_tokens: i64,
    fetched_at: Instant,
}

static CACHED_DAILY_TOTAL: Lazy<Mutex<Option<CachedTotal>>> = Lazy::new(|| Mutex::new(None));

/// Local-day key as an `i32` (e.g. 20260617) so a day rollover is detectable
/// without holding any date objects across the cache.
fn local_day_key() -> i32 {
    use chrono::{Datelike, Local};
    let now = Local::now();
    now.year() * 10_000 + now.month() as i32 * 100 + now.day() as i32
}

/// Read today's total tokens, using the short-TTL cache when fresh. Only called
/// when a budget is configured, so the SQLite read cost is paid solely by
/// budget-enabled deployments. Holds the cache lock across the refresh so that
/// concurrent callers finding the cache stale collapse into one DB read.
fn current_daily_total() -> i64 {
    let day_key = local_day_key();
    let mut guard = CACHED_DAILY_TOTAL.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(cached) = guard.as_ref() {
        if cached.day_key == day_key && cached.fetched_at.elapsed() < CACHE_TTL {
            return cached.total_tokens;
        }
    }

    // Stale or absent: refresh under the lock. The DB read is held off the hot
    // path by the TTL, and serializing the refresh avoids a thundering herd.
    let total = get_token_usage_summary("day").totals.total_tokens;
    *guard = Some(CachedTotal {
        day_key,
        total_tokens: total,
        fetched_at: Instant::now(),
    });
    total
}

/// Reject the request with a 429 when the configured daily token budget has been
/// reached. A no-op when no budget is set. Mirrors the rate-limit admission
/// shape so the Anthropic/OpenAI clients treat it as a retryable 429.
#[allow(clippy::result_large_err)]
pub fn check_token_budget() -> Result<(), HttpError> {
    let budget = match get_daily_token_budget() {
        Some(b) => b,
        None => return Ok(()),
    };

    let used = current_daily_total();
    if used >= budget {
        tracing::warn!("Daily token budget exceeded: {used} >= {budget}");
        return Err(HttpError::new(
            "Daily token budget exceeded",
            StatusCode::TOO_MANY_REQUESTS,
            Default::default(),
            json!({
                "message": format!(
                    "Daily token budget of {budget} tokens exceeded ({used} used). \
                     Requests resume after the local day rolls over."
                )
            })
            .to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::config::{
        reset_cached_config_for_test, set_cached_config_for_test, AppConfig,
    };

    /// Seed the daily-total cache so the rejection path can be exercised without
    /// touching SQLite. The fresh `fetched_at` keeps it within the TTL.
    fn seed_cache(total_tokens: i64) {
        let mut guard = CACHED_DAILY_TOTAL.lock().unwrap_or_else(|p| p.into_inner());
        *guard = Some(CachedTotal {
            day_key: local_day_key(),
            total_tokens,
            fetched_at: Instant::now(),
        });
    }

    fn clear_cache() {
        *CACHED_DAILY_TOTAL.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    #[test]
    fn local_day_key_is_ordered_yyyymmdd() {
        // Sanity: the key is a positive YYYYMMDD-shaped integer, so a rollover
        // produces a strictly greater value the next day.
        let key = local_day_key();
        assert!(key > 10_000_000, "expected YYYYMMDD-shaped key, got {key}");
    }

    #[test]
    #[serial_test::serial]
    fn no_budget_configured_is_a_noop() {
        // With no dailyTokenBudget set, admission always passes and never reads
        // the usage store.
        set_cached_config_for_test(AppConfig::default());
        assert!(check_token_budget().is_ok());
        reset_cached_config_for_test();
    }

    #[test]
    #[serial_test::serial]
    fn non_positive_budget_is_disabled() {
        let cfg = AppConfig {
            daily_token_budget: Some(0),
            ..Default::default()
        };
        set_cached_config_for_test(cfg);
        // A zero/negative budget is treated as "no budget", not "block everything".
        assert!(check_token_budget().is_ok());
        reset_cached_config_for_test();
    }

    #[test]
    #[serial_test::serial]
    fn rejects_once_at_or_over_budget() {
        let cfg = AppConfig {
            daily_token_budget: Some(1000),
            ..Default::default()
        };
        set_cached_config_for_test(cfg);

        // Under budget: passes.
        seed_cache(999);
        assert!(check_token_budget().is_ok());

        // At the cap: rejected with a 429.
        seed_cache(1000);
        let err = check_token_budget().expect_err("should reject at the cap");
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);

        // Over the cap: also rejected.
        seed_cache(5000);
        assert!(check_token_budget().is_err());

        clear_cache();
        reset_cached_config_for_test();
    }
}
