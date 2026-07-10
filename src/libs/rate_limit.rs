use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use once_cell::sync::Lazy;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::libs::error::HttpError;
use crate::libs::state;

/// Serialized scheduler state for the process-wide minimum request interval.
///
/// A wall-clock timestamp in the shared `State` used to make the old check a
/// read/compare/write race: concurrent callers could all observe the same old
/// value and proceed. A Tokio mutex plus a monotonic deadline lets each caller
/// atomically admit or reserve exactly one slot without holding a lock while it
/// sleeps.
#[derive(Default)]
struct RateLimitGate {
    interval: Option<Duration>,
    next_admission: Option<Instant>,
}

static RATE_LIMIT_GATE: Lazy<Mutex<RateLimitGate>> =
    Lazy::new(|| Mutex::new(RateLimitGate::default()));

// Keeps the default-disabled hot path lock-free while still letting a runtime
// transition to `None` clear an earlier schedule (primarily useful to tests and
// embedders that mutate State after startup).
static RATE_LIMIT_ACTIVE: AtomicBool = AtomicBool::new(false);

enum AdmissionDecision {
    Admit,
    Reject(Duration),
    WaitUntil(Instant, Duration),
}

/// Enforce the configured process-wide minimum interval between requests.
///
/// In reject mode, a caller arriving before the next slot gets a 429 without
/// changing the schedule. In wait mode, callers atomically reserve successive
/// slots and sleep without the mutex, so a burst is paced one interval apart
/// instead of waking and proceeding together.
#[allow(clippy::result_large_err)]
pub async fn check_rate_limit() -> Result<(), HttpError> {
    let (rate_limit_seconds, wait) =
        state::with_state(|s| (s.rate_limit_seconds, s.rate_limit_wait));
    let Some(seconds) = rate_limit_seconds.filter(|seconds| *seconds > 0) else {
        clear_disabled_gate().await;
        return Ok(());
    };

    RATE_LIMIT_ACTIVE.store(true, Ordering::Release);
    let interval = Duration::from_secs(seconds);
    let now = Instant::now();

    let decision = {
        let mut gate = RATE_LIMIT_GATE.lock().await;

        // The CLI setting is normally immutable, but reset cleanly if an
        // embedding/test changes it at runtime.
        if gate.interval != Some(interval) {
            gate.interval = Some(interval);
            gate.next_admission = None;
        }

        match gate.next_admission {
            Some(next) if next > now => {
                let remaining = next.duration_since(now);
                if wait {
                    // Reserve this slot before releasing the mutex. The next
                    // waiter sees the following slot and cannot wake with us.
                    gate.next_admission = next.checked_add(interval).or(Some(next));
                    AdmissionDecision::WaitUntil(next, remaining)
                } else {
                    AdmissionDecision::Reject(remaining)
                }
            }
            _ => {
                gate.next_admission = now.checked_add(interval);
                AdmissionDecision::Admit
            }
        }
    };

    match decision {
        AdmissionDecision::Admit => {
            record_admission_time();
            Ok(())
        }
        AdmissionDecision::Reject(remaining) => {
            let wait_seconds = rounded_up_seconds(remaining);
            tracing::warn!("Rate limit exceeded. Need to wait {wait_seconds} more seconds.");
            Err(rate_limit_error(wait_seconds))
        }
        AdmissionDecision::WaitUntil(deadline, remaining) => {
            let wait_seconds = rounded_up_seconds(remaining);
            tracing::warn!(
                "Rate limit reached. Waiting {wait_seconds} seconds before proceeding..."
            );
            tokio::time::sleep_until(deadline).await;
            record_admission_time();
            tracing::info!("Rate limit wait completed, proceeding with request");
            Ok(())
        }
    }
}

async fn clear_disabled_gate() {
    if !RATE_LIMIT_ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let mut gate = RATE_LIMIT_GATE.lock().await;
    *gate = RateLimitGate::default();
}

fn rounded_up_seconds(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
        .max(1)
}

fn rate_limit_error(wait_seconds: u64) -> HttpError {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&wait_seconds.to_string()) {
        headers.insert(axum::http::header::RETRY_AFTER, value);
    }
    HttpError::new(
        "Rate limit exceeded",
        StatusCode::TOO_MANY_REQUESTS,
        headers,
        json!({ "message": "Rate limit exceeded" }).to_string(),
    )
}

fn record_admission_time() {
    state::with_state_mut(|s| s.last_request_timestamp = Some(now_millis()));
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn configure(seconds: Option<u64>, wait: bool) {
        state::with_state_mut(|s| {
            s.rate_limit_seconds = seconds;
            s.rate_limit_wait = wait;
            s.last_request_timestamp = None;
        });
        if seconds.is_none() || seconds == Some(0) {
            clear_disabled_gate().await;
        } else {
            let mut gate = RATE_LIMIT_GATE.lock().await;
            *gate = RateLimitGate::default();
            RATE_LIMIT_ACTIVE.store(false, Ordering::Release);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reject_mode_admits_only_one_concurrent_caller() {
        configure(Some(60), false).await;

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                check_rate_limit().await
            }));
        }

        let mut admitted = 0;
        let mut rejected = 0;
        for task in tasks {
            match task.await.expect("rate-limit task") {
                Ok(()) => admitted += 1,
                Err(error) => {
                    assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
                    assert!(error.headers.contains_key(axum::http::header::RETRY_AFTER));
                    rejected += 1;
                }
            }
        }
        assert_eq!(admitted, 1);
        assert_eq!(rejected, 7);

        configure(None, false).await;
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn wait_mode_reserves_spaced_slots() {
        configure(Some(1), true).await;
        check_rate_limit()
            .await
            .expect("first request is immediate");

        let first_waiter = tokio::spawn(check_rate_limit());
        let second_waiter = tokio::spawn(check_rate_limit());
        tokio::task::yield_now().await;
        assert!(!first_waiter.is_finished());
        assert!(!second_waiter.is_finished());

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_ne!(
            first_waiter.is_finished(),
            second_waiter.is_finished(),
            "exactly one waiter should own the first reserved slot"
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        first_waiter
            .await
            .expect("first waiter task")
            .expect("admitted");
        second_waiter
            .await
            .expect("second waiter task")
            .expect("admitted");

        configure(None, false).await;
    }
}
