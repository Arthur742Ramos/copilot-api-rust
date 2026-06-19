//! Premium-interaction quota-aware admission control.
//!
//! GitHub Copilot meters "premium interactions" as the *actual billable
//! resource* behind chat/image requests on metered and overage plans. The local
//! [`crate::libs::token_budget`] gate only sees the token proxy's own counters;
//! it has no view of how much premium-interaction entitlement the account has
//! left upstream. This module closes that gap: a background loop periodically
//! reads the `premium_interactions` quota snapshot from the shared
//! `/copilot_internal/user` endpoint, caches it in [`crate::libs::state`], and a
//! synchronous admission check rejects requests with a 429 once the cached
//! snapshot crosses a configured threshold or tips into overage.
//!
//! Design honesty (this is a *coarse* guardrail):
//! - The snapshot is account-wide and refreshes on a minutes cadence (the
//!   upstream endpoint is shared and rate-limited), so enforcement lags reality
//!   and can overshoot by whatever was spent since the last refresh — exactly
//!   like the daily token budget.
//! - It only adds value on metered/overage plans. When the plan reports the
//!   premium-interaction quota as `unlimited`, the gate is always a no-op.
//! - It is opt-in: with neither config knob set, `check_premium_interactions`
//!   returns `Ok(())` without touching state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use once_cell::sync::Lazy;
use serde_json::json;

use crate::libs::config::{get_block_on_premium_overage, get_min_premium_interactions_remaining};
use crate::libs::error::HttpError;
use crate::libs::state;
use crate::services::github::get_copilot_usage::get_copilot_usage;

/// Cached premium-interaction quota snapshot. Only the four fields the gate
/// reasons about are retained (remaining / entitlement / overage_permitted /
/// unlimited); the full upstream `QuotaDetail` is not held in global state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PremiumInteractionsSnapshot {
    pub remaining: f64,
    pub entitlement: f64,
    pub overage_permitted: bool,
    pub unlimited: bool,
}

/// The resolved gate configuration, kept separate from the snapshot so the
/// decision logic is a pure function of (snapshot, config) and unit-testable
/// without global state.
#[derive(Debug, Clone, Copy, Default)]
pub struct PremiumGateConfig {
    /// Reject when remaining falls strictly below this. `None` disables the
    /// remaining-threshold check.
    pub min_remaining: Option<f64>,
    /// Reject once the account has exhausted its entitlement and is into overage.
    pub block_on_overage: bool,
}

impl PremiumGateConfig {
    /// True when no knob is set — used to short-circuit before reading state.
    fn is_noop(&self) -> bool {
        self.min_remaining.is_none() && !self.block_on_overage
    }
}

/// Resolve the gate configuration from the cached app config.
fn resolved_gate_config() -> PremiumGateConfig {
    PremiumGateConfig {
        min_remaining: get_min_premium_interactions_remaining(),
        block_on_overage: get_block_on_premium_overage(),
    }
}

/// Pure admission decision. Returns `Some(reason)` when the request should be
/// rejected, `None` when it should be admitted.
///
/// Always a no-op when the plan reports the quota as `unlimited`, regardless of
/// configuration — an unlimited plan has no billable premium-interaction
/// resource to protect. "Into overage" is modelled as `remaining <= 0` (the
/// entitlement is exhausted, so any further interaction bills as overage).
pub fn should_block(
    snapshot: &PremiumInteractionsSnapshot,
    config: &PremiumGateConfig,
) -> Option<String> {
    if snapshot.unlimited {
        return None;
    }
    if let Some(min) = config.min_remaining {
        if snapshot.remaining < min {
            return Some(format!(
                "premium-interaction quota near exhaustion: {:.0} remaining (< configured minimum {:.0})",
                snapshot.remaining, min
            ));
        }
    }
    if config.block_on_overage && snapshot.remaining <= 0.0 {
        return Some(format!(
            "premium-interaction entitlement exhausted ({:.0} remaining of {:.0}); \
             blocking on overage",
            snapshot.remaining, snapshot.entitlement
        ));
    }
    None
}

/// Reject the request with a 429 when the cached premium-interaction snapshot
/// crosses the configured guardrail. A no-op when neither knob is set, when no
/// snapshot has been fetched yet, or when the plan is unlimited.
///
/// Returns the SAME 429 shape as the daily token budget so OpenAI/Anthropic
/// clients treat it as a consistent, retryable overload signal.
#[allow(clippy::result_large_err)]
pub fn check_premium_interactions() -> Result<(), HttpError> {
    let config = resolved_gate_config();
    if config.is_noop() {
        return Ok(());
    }

    let snapshot = match state::with_state(|s| s.premium_interactions) {
        // No snapshot yet (refresher hasn't completed a fetch, or the plan has no
        // premium_interactions quota): fail open rather than reject blindly.
        None => return Ok(()),
        Some(snap) => snap,
    };

    record_snapshot_metrics(&snapshot);

    if let Some(reason) = should_block(&snapshot, &config) {
        metrics::counter!("copilot_premium_interactions_rejections_total").increment(1);
        tracing::warn!("Premium-interaction admission gate rejecting request: {reason}");
        return Err(HttpError::new(
            "Premium-interaction quota exhausted",
            StatusCode::TOO_MANY_REQUESTS,
            Default::default(),
            json!({
                "message": format!(
                    "Premium-interaction quota guardrail tripped: {reason}. \
                     Requests resume after the upstream Copilot quota recovers."
                )
            })
            .to_string(),
        ));
    }
    Ok(())
}

// --- Prometheus metrics -----------------------------------------------------

/// Export the cached snapshot as bounded single-series gauges, mirroring the
/// style of [`crate::libs::copilot_rate_limit`]. Booleans are exposed as 0/1 so
/// "overage permitted" / "unlimited" are alertable.
fn record_snapshot_metrics(snapshot: &PremiumInteractionsSnapshot) {
    metrics::gauge!("copilot_premium_interactions_remaining").set(snapshot.remaining);
    metrics::gauge!("copilot_premium_interactions_entitlement").set(snapshot.entitlement);
    metrics::gauge!("copilot_premium_interactions_overage_permitted")
        .set(if snapshot.overage_permitted { 1.0 } else { 0.0 });
    metrics::gauge!("copilot_premium_interactions_unlimited").set(if snapshot.unlimited {
        1.0
    } else {
        0.0
    });
}

/// Preregister the rejection counter at 0 so `rate()`/`increase()` and
/// "rejections > N" alerts read 0 rather than "no data" before the first
/// rejection. The gauges intentionally aren't seeded with a value — emitting a
/// fake `remaining = 0` at startup would look like exhaustion; they appear after
/// the first successful refresh. Call once at startup after the recorder is
/// installed.
pub fn preregister_premium_interactions_metrics() {
    metrics::counter!("copilot_premium_interactions_rejections_total").increment(0);
}

// --- Background refresher ----------------------------------------------------

/// Poll cadence between successful snapshot refreshes. The
/// `/copilot_internal/user` endpoint is shared and rate-limited, so this is on a
/// minutes scale, NOT seconds (cf. the ~15s token refresh poll in `token.rs`).
const PREMIUM_REFRESH_INTERVAL_MS: i64 = 5 * 60 * 1000;
/// Up to this much random jitter is added to each scheduled refresh so multiple
/// proxy instances sharing an account don't stampede the shared endpoint in
/// lockstep (mirrors the jitter on the token refresh retry path).
const PREMIUM_REFRESH_JITTER_MS: i64 = 60 * 1000;
/// Initial backoff after a failed refresh, doubled up to the max.
const PREMIUM_RETRY_DELAY_MS: i64 = 30 * 1000;
const PREMIUM_MAX_RETRY_DELAY_MS: i64 = 30 * 60 * 1000;
/// Upper bound on a single sleep so abort is observed promptly even when the
/// next refresh is minutes away.
const PREMIUM_POLL_INTERVAL_MS: i64 = 15 * 1000;

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct LoopController {
    aborted: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

static PREMIUM_REFRESH: Lazy<Mutex<Option<LoopController>>> = Lazy::new(|| Mutex::new(None));

/// Stop the premium-interaction refresh loop if running (idempotent).
pub fn stop_premium_interactions_refresh_loop() {
    if let Some(controller) = PREMIUM_REFRESH.lock().unwrap().take() {
        controller.aborted.store(true, Ordering::SeqCst);
        controller.handle.abort();
    }
}

/// Whether either admission-gate knob is configured. The refresher only polls
/// the shared `/copilot_internal/user` endpoint when at least one knob is set,
/// so an operator who never opts in pays no background polling (and sees no
/// warn-log churn on transient failures).
fn gate_is_configured() -> bool {
    get_min_premium_interactions_remaining().is_some() || get_block_on_premium_overage()
}

/// Spawn the background refresher. Mirrors the structure of the token refresh
/// loop: a single owned task with an abort flag, replacing any prior loop.
/// No-op when neither guardrail knob is configured, keeping the feature truly
/// opt-in (re-evaluated whenever this is called, e.g. after a config reload).
pub fn start_premium_interactions_refresh_loop() {
    stop_premium_interactions_refresh_loop();
    if !gate_is_configured() {
        tracing::debug!(
            "Premium-interaction guardrail unconfigured; not starting the usage refresher"
        );
        return;
    }
    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_clone = aborted.clone();
    let handle = tokio::spawn(async move {
        run_premium_refresh_loop(aborted_clone).await;
    });
    *PREMIUM_REFRESH.lock().unwrap() = Some(LoopController { aborted, handle });
}

/// Extract the four cached fields from a usage response, if it carries a
/// `premium_interactions` quota snapshot.
fn extract_snapshot(
    usage: &crate::services::github::get_copilot_usage::CopilotUsageResponse,
) -> Option<PremiumInteractionsSnapshot> {
    let detail = usage
        .quota_snapshots
        .as_ref()?
        .premium_interactions
        .as_ref()?;
    Some(PremiumInteractionsSnapshot {
        remaining: detail.remaining,
        entitlement: detail.entitlement,
        overage_permitted: detail.overage_permitted,
        unlimited: detail.unlimited,
    })
}

async fn run_premium_refresh_loop(aborted: Arc<AtomicBool>) {
    // Refresh once promptly on startup so the gate has data within a poll
    // interval, then settle into the minutes cadence.
    let mut refresh_at_ms = now_millis();
    let mut retry_delay_ms = PREMIUM_RETRY_DELAY_MS;

    while !aborted.load(Ordering::SeqCst) {
        let next_delay_ms = (refresh_at_ms - now_millis()).clamp(0, PREMIUM_POLL_INTERVAL_MS);
        if next_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(next_delay_ms as u64)).await;
            continue;
        }

        let snapshot = state::snapshot();
        match get_copilot_usage(&snapshot, None).await {
            Ok(usage) => {
                if let Some(snap) = extract_snapshot(&usage) {
                    state::with_state_mut(|s| s.premium_interactions = Some(snap));
                    record_snapshot_metrics(&snap);
                    tracing::debug!(
                        "Premium-interaction quota refreshed: {:.0} remaining of {:.0} (unlimited={})",
                        snap.remaining,
                        snap.entitlement,
                        snap.unlimited
                    );
                } else {
                    // The plan no longer reports a premium_interactions quota
                    // (plan change, or GitHub stopped returning it). Clear any
                    // cached snapshot so the gate fails OPEN on fresh data rather
                    // than enforcing a stale snapshot indefinitely.
                    state::with_state_mut(|s| s.premium_interactions = None);
                    tracing::debug!(
                        "Copilot usage has no premium_interactions snapshot; cleared cache, gate stays a no-op"
                    );
                }
                let jitter = (rand::random::<u64>() % PREMIUM_REFRESH_JITTER_MS as u64) as i64;
                refresh_at_ms = now_millis() + PREMIUM_REFRESH_INTERVAL_MS + jitter;
                retry_delay_ms = PREMIUM_RETRY_DELAY_MS;
            }
            Err(e) => {
                tracing::warn!("Failed to refresh premium-interaction quota: {e}");
                let jitter = (rand::random::<u64>() % PREMIUM_REFRESH_JITTER_MS as u64) as i64;
                let delay_ms = std::cmp::min(retry_delay_ms + jitter, PREMIUM_MAX_RETRY_DELAY_MS);
                refresh_at_ms = now_millis() + delay_ms;
                retry_delay_ms = std::cmp::min(retry_delay_ms * 2, PREMIUM_MAX_RETRY_DELAY_MS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(
        remaining: f64,
        entitlement: f64,
        overage: bool,
        unlimited: bool,
    ) -> PremiumInteractionsSnapshot {
        PremiumInteractionsSnapshot {
            remaining,
            entitlement,
            overage_permitted: overage,
            unlimited,
        }
    }

    #[test]
    fn no_op_when_no_knobs_set() {
        let cfg = PremiumGateConfig::default();
        assert!(cfg.is_noop());
        // Even at zero remaining, an unconfigured gate admits.
        assert!(should_block(&snap(0.0, 300.0, true, false), &cfg).is_none());
    }

    #[test]
    fn no_op_when_unlimited_regardless_of_config() {
        let cfg = PremiumGateConfig {
            min_remaining: Some(100.0),
            block_on_overage: true,
        };
        // Unlimited plan: never blocked even with remaining at/below thresholds.
        assert!(should_block(&snap(0.0, 0.0, true, true), &cfg).is_none());
        assert!(should_block(&snap(-50.0, 300.0, true, true), &cfg).is_none());
    }

    #[test]
    fn min_remaining_threshold_blocks_below_not_at_or_above() {
        let cfg = PremiumGateConfig {
            min_remaining: Some(100.0),
            block_on_overage: false,
        };
        // Strictly below the minimum → blocked.
        assert!(should_block(&snap(99.0, 300.0, true, false), &cfg).is_some());
        // At the minimum → admitted (threshold is exclusive).
        assert!(should_block(&snap(100.0, 300.0, true, false), &cfg).is_none());
        // Above the minimum → admitted.
        assert!(should_block(&snap(250.0, 300.0, true, false), &cfg).is_none());
    }

    #[test]
    fn block_on_overage_triggers_only_when_entitlement_exhausted() {
        let cfg = PremiumGateConfig {
            min_remaining: None,
            block_on_overage: true,
        };
        // Still has entitlement → admitted.
        assert!(should_block(&snap(5.0, 300.0, true, false), &cfg).is_none());
        // Exactly exhausted → blocked.
        assert!(should_block(&snap(0.0, 300.0, true, false), &cfg).is_some());
        // Into overage (negative remaining) → blocked.
        assert!(should_block(&snap(-10.0, 300.0, true, false), &cfg).is_some());
    }

    #[test]
    fn both_knobs_remaining_threshold_takes_precedence() {
        let cfg = PremiumGateConfig {
            min_remaining: Some(50.0),
            block_on_overage: true,
        };
        // Below threshold but not yet in overage → blocked by the threshold.
        let reason = should_block(&snap(20.0, 300.0, true, false), &cfg).unwrap();
        assert!(reason.contains("near exhaustion"), "got: {reason}");
    }
}
