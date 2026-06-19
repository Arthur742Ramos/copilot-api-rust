//! Daily token-budget admission control.
//!
//! When `dailyTokenBudget` is configured, requests are rejected with a 429 once
//! the total tokens recorded for the current local day reach the cap. The cap is
//! a guardrail against a runaway client or leaked key burning unlimited Copilot
//! quota; it is intentionally a coarse cumulative-spend gate, not a hard
//! per-request reservation.
//!
//! Phase 2 adds an independent *per-key* cap: an `auth.apiKeys` entry in object
//! form may carry its own `dailyTokenBudget`, enforced against just that key's
//! recorded spend for the day (keyed by the same attribution label Phase 1
//! established). A request is rejected if EITHER the global or its per-key cap is
//! exceeded. This is throttling/visibility, not cost isolation: all spend still
//! lands on one upstream Copilot billing identity.
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

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use once_cell::sync::Lazy;
use serde_json::json;

use crate::libs::config::get_daily_token_budget;
use crate::libs::error::HttpError;
use crate::libs::request_auth::get_api_key_daily_budget;
use crate::libs::token_usage::{
    get_token_usage_label_total, get_token_usage_summary, resolve_api_key_label,
};

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

/// Per-attribution-label cached daily totals, analogous to [`CACHED_DAILY_TOTAL`]
/// but keyed by the same label Phase 1 attributes usage under. Only populated for
/// labels that have a configured per-key budget, so deployments without per-key
/// caps never grow this map. Each entry carries its own day-key + TTL so a
/// midnight rollover invalidates it exactly like the global cache.
static CACHED_LABEL_TOTALS: Lazy<Mutex<HashMap<String, CachedTotal>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Local-day key as an `i32` (e.g. 20260617) so a day rollover is detectable
/// without holding any date objects across the cache.
fn local_day_key() -> i32 {
    use chrono::{Datelike, Local};
    let now = Local::now();
    now.year() * 10_000 + now.month() as i32 * 100 + now.day() as i32
}

/// Pure admission decision shared by the global and per-key gates: block when a
/// positive cap is configured and the cumulative recorded spend has reached it.
/// A `None`/non-positive cap means "no cap" and never blocks. Factored out so the
/// threshold logic is unit-testable without a cache or DB.
fn should_block(used: i64, budget: Option<i64>) -> bool {
    matches!(budget, Some(cap) if cap > 0 && used >= cap)
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

/// Per-label analogue of [`current_daily_total`]: today's recorded spend for a
/// single attribution label, cached with the same short TTL. Only invoked when
/// that label has a configured per-key budget, so the map stays small and the
/// scoped `WHERE api_key_label = ?` read is paid solely by capped keys.
fn current_label_total(label: &str) -> i64 {
    let day_key = local_day_key();
    let mut guard = CACHED_LABEL_TOTALS
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(cached) = guard.get(label) {
        if cached.day_key == day_key && cached.fetched_at.elapsed() < CACHE_TTL {
            return cached.total_tokens;
        }
    }

    let total = get_token_usage_label_total("day", label);
    guard.insert(
        label.to_string(),
        CachedTotal {
            day_key,
            total_tokens: total,
            fetched_at: Instant::now(),
        },
    );
    total
}

/// Reject the request with a 429 when either the global daily token budget or the
/// current key's per-key budget has been reached. A no-op when neither is set.
/// The two caps are independent: a request is rejected if EITHER is exceeded.
/// Mirrors the rate-limit admission shape so the Anthropic/OpenAI clients treat it
/// as a retryable 429.
#[allow(clippy::result_large_err)]
pub fn check_token_budget() -> Result<(), HttpError> {
    // Global cap: gates on the whole day's recorded spend across all keys.
    if let Some(budget) = get_daily_token_budget() {
        let used = current_daily_total();
        // Export the spend-vs-cap so operators can chart "approaching daily
        // budget" and alert before clients start getting 429'd. Both are
        // single-series gauges (no labels), set on each check.
        metrics::gauge!("token_budget_daily_tokens_used").set(used as f64);
        metrics::gauge!("token_budget_daily_limit").set(budget as f64);
        if should_block(used, Some(budget)) {
            metrics::counter!("token_budget_rejections_total").increment(1);
            tracing::warn!("Daily token budget exceeded: {used} >= {budget}");
            return Err(budget_exceeded_error(budget, used, None));
        }
    }

    // Per-key cap: gates on just this request's attribution label. Resolved from
    // RequestContext exactly as usage attribution does, so the budget applies to
    // the same identity the spend is recorded under. Anonymous/unlabeled requests
    // resolve to `None` and only ever hit the global cap above.
    if let Some(label) = resolve_api_key_label() {
        if let Some(budget) = get_api_key_daily_budget(&label) {
            let used = current_label_total(&label);
            if should_block(used, Some(budget)) {
                metrics::counter!("token_budget_per_key_rejections_total").increment(1);
                tracing::warn!(
                    "Per-key daily token budget exceeded for {label}: {used} >= {budget}"
                );
                return Err(budget_exceeded_error(budget, used, Some(&label)));
            }
        }
    }

    Ok(())
}

/// Build the shared 429 the budget gates return. The per-key variant names the
/// key so an operator reading the client's error can tell which cap fired, while
/// keeping the same status + Anthropic/OpenAI-friendly body as the global cap.
#[allow(clippy::result_large_err)]
fn budget_exceeded_error(budget: i64, used: i64, label: Option<&str>) -> HttpError {
    let scope = match label {
        Some(label) => format!("Per-key daily token budget for '{label}'"),
        None => "Daily token budget".to_string(),
    };
    HttpError::new(
        "Daily token budget exceeded",
        StatusCode::TOO_MANY_REQUESTS,
        Default::default(),
        json!({
            "message": format!(
                "{scope} of {budget} tokens exceeded ({used} used). \
                 Requests resume after the local day rolls over."
            )
        })
        .to_string(),
    )
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

    /// Seed the per-label cache for `label` so the per-key path can be exercised
    /// without touching SQLite.
    fn seed_label_cache(label: &str, total_tokens: i64) {
        let mut guard = CACHED_LABEL_TOTALS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.insert(
            label.to_string(),
            CachedTotal {
                day_key: local_day_key(),
                total_tokens,
                fetched_at: Instant::now(),
            },
        );
    }

    fn clear_label_cache() {
        CACHED_LABEL_TOTALS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
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

    #[test]
    #[serial_test::serial]
    fn exports_budget_metrics() {
        crate::libs::metrics::init_build_info(); // installs the recorder
        let cfg = AppConfig {
            daily_token_budget: Some(1000),
            ..Default::default()
        };
        set_cached_config_for_test(cfg);

        // Over the cap -> rejection counter + usage/limit gauges are emitted.
        seed_cache(2000);
        assert!(check_token_budget().is_err());

        let out = crate::libs::metrics::render();
        assert!(
            out.contains("token_budget_rejections_total"),
            "expected rejection counter, got:\n{out}"
        );
        assert!(
            out.contains("token_budget_daily_tokens_used"),
            "expected used gauge"
        );
        assert!(
            out.contains("token_budget_daily_limit"),
            "expected limit gauge"
        );

        clear_cache();
        reset_cached_config_for_test();
    }

    #[test]
    fn should_block_threshold_logic() {
        // No cap configured -> never blocks, regardless of spend.
        assert!(!should_block(0, None));
        assert!(!should_block(1_000_000, None));
        // Non-positive cap is treated as disabled.
        assert!(!should_block(5, Some(0)));
        assert!(!should_block(5, Some(-1)));
        // Positive cap: blocks at or over, passes strictly under.
        assert!(!should_block(999, Some(1000)));
        assert!(should_block(1000, Some(1000)));
        assert!(should_block(5000, Some(1000)));
    }

    #[test]
    #[serial_test::serial]
    fn current_label_total_uses_seeded_cache() {
        // A fresh seeded entry is returned without hitting SQLite.
        clear_label_cache();
        seed_label_cache("team-a", 4242);
        assert_eq!(current_label_total("team-a"), 4242);
        clear_label_cache();
    }

    /// Build a config whose single object-form key carries a per-key budget.
    fn cfg_with_keyed_budget(label: &str, budget: i64) -> AppConfig {
        use crate::libs::config::AuthConfig;
        AppConfig {
            auth: Some(AuthConfig {
                api_keys: Some(vec![json!({
                    "key": "secret-key",
                    "label": label,
                    "dailyTokenBudget": budget,
                })]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    #[serial_test::serial]
    fn anonymous_request_only_hits_global_cap() {
        // A per-key budget is configured and its label is already over cap, but
        // an anonymous request (no attribution label in context) must NOT be
        // gated by it — only the global cap applies, and here there is none.
        set_cached_config_for_test(cfg_with_keyed_budget("team-a", 1000));
        clear_cache();
        clear_label_cache();
        seed_label_cache("team-a", 999_999);

        // resolve_api_key_label() is None outside a request context, so the
        // per-key branch is skipped and admission passes.
        assert!(check_token_budget().is_ok());

        clear_label_cache();
        reset_cached_config_for_test();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn per_key_cap_rejects_labeled_request() {
        use crate::libs::request_context::{
            run_with_context, set_request_api_key_label, RequestContext,
        };

        set_cached_config_for_test(cfg_with_keyed_budget("team-a", 1000));
        clear_cache(); // no global cap seeded; global budget is unset
        clear_label_cache();
        seed_label_cache("team-a", 2000); // team-a is over its per-key cap

        let ctx = RequestContext::new("trace".to_string(), 0, "test".to_string(), None, None);
        let result = run_with_context(ctx, async {
            set_request_api_key_label("team-a".to_string());
            check_token_budget()
        })
        .await;

        let err = result.expect_err("labeled request over per-key cap must be rejected");
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);

        // A different label under no cap (and no global cap) still passes.
        let ctx2 = RequestContext::new("trace2".to_string(), 0, "test".to_string(), None, None);
        let ok = run_with_context(ctx2, async {
            set_request_api_key_label("other".to_string());
            check_token_budget()
        })
        .await;
        assert!(ok.is_ok());

        clear_label_cache();
        reset_cached_config_for_test();
    }
}
