use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Number of tasks currently sleeping in a WaitUntil reservation.
///
/// Used to enforce `COPILOT_API_RATE_LIMIT_MAX_WAITERS`: when the count
/// reaches the configured limit, new arrivals are rejected rather than queued.
static WAITER_COUNT: AtomicUsize = AtomicUsize::new(0);

enum AdmissionDecision {
    Admit,
    Reject(Duration),
    WaitUntil(Instant, Duration),
}

/// RAII guard for a reserved rate-limit slot.
///
/// Decrements the global waiter count when dropped regardless of whether the
/// sleep completed. If dropped without [`SlotGuard::commit`] being called first
/// (i.e. the enclosing future was cancelled mid-sleep), also performs a
/// best-effort rollback of the reserved slot so the wasted reservation does not
/// permanently inflate the wait time for subsequent callers.
struct SlotGuard {
    /// The admission instant we reserved (our slot in the queue).
    reserved: Instant,
    /// The configured interval, used to compute `expected_next` for rollback.
    interval: Duration,
    /// Set to `true` when the sleep completes successfully; suppresses rollback.
    committed: bool,
}

impl SlotGuard {
    fn new(reserved: Instant, interval: Duration) -> Self {
        WAITER_COUNT.fetch_add(1, Ordering::AcqRel);
        Self {
            reserved,
            interval,
            committed: false,
        }
    }

    /// Mark the reservation as committed (sleep completed successfully).
    /// The waiter count is still decremented on drop; this only suppresses
    /// the slot rollback that would otherwise fire for a cancelled future.
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        WAITER_COUNT.fetch_sub(1, Ordering::AcqRel);
        if self.committed {
            return;
        }
        // Cancelled mid-sleep: best-effort slot rollback.
        //
        // When we reserved our slot, `gate.next_admission` was advanced from
        // `reserved` to `reserved + interval`. If no later caller has since
        // advanced it further, rolling back to `reserved` lets the next waiter
        // take our slot rather than waiting one extra interval.
        //
        // `try_lock()` is used because we are in a synchronous `Drop` and
        // cannot `.await`. If it fails (another task holds the lock), the
        // phantom slot is wasted — the next request simply experiences one
        // extra interval of delay, which is safe and bounded.
        let expected_next = self
            .reserved
            .checked_add(self.interval)
            .unwrap_or(self.reserved);
        if let Ok(mut gate) = RATE_LIMIT_GATE.try_lock() {
            if gate.next_admission == Some(expected_next) {
                gate.next_admission = Some(self.reserved);
            }
        }
    }
}

/// Enforce the configured process-wide minimum interval between requests.
///
/// In reject mode, a caller arriving before the next slot gets a 429 without
/// changing the schedule. In wait mode, callers atomically reserve successive
/// slots and sleep without the mutex, so a burst is paced one interval apart
/// instead of waking and proceeding together.
///
/// Two optional bounds on the wait queue can be set via environment variables:
/// * `COPILOT_API_RATE_LIMIT_MAX_WAITERS` — reject when this many tasks are
///   already sleeping (prevents unbounded queue growth).
/// * `COPILOT_API_RATE_LIMIT_MAX_WAIT_SECS` — reject if the wait would exceed
///   this many seconds (prevents extremely stale requests from being accepted).
///
/// If a waiter's future is cancelled before the sleep completes (e.g. because
/// the client disconnected), the reserved slot is rolled back so subsequent
/// callers do not inherit a phantom delay.
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
            // Reject early if there are already too many queued waiters.
            if let Some(max_waiters) = max_waiters_limit() {
                if WAITER_COUNT.load(Ordering::Acquire) >= max_waiters {
                    let wait_seconds = rounded_up_seconds(remaining);
                    tracing::warn!(
                        "Rate limit queue full ({max_waiters} waiters). Rejecting request."
                    );
                    // Roll back the slot we just reserved inside the lock.
                    let expected_next = deadline.checked_add(interval).unwrap_or(deadline);
                    if let Ok(mut gate) = RATE_LIMIT_GATE.try_lock() {
                        if gate.next_admission == Some(expected_next) {
                            gate.next_admission = Some(deadline);
                        }
                    }
                    return Err(rate_limit_error(wait_seconds));
                }
            }
            // Reject if the wait would exceed the configured maximum.
            if let Some(max_secs) = max_wait_secs_limit() {
                if remaining > Duration::from_secs(max_secs) {
                    let wait_seconds = rounded_up_seconds(remaining);
                    tracing::warn!(
                        "Rate limit wait {wait_seconds}s exceeds max {max_secs}s. Rejecting."
                    );
                    // Roll back the slot we just reserved inside the lock.
                    let expected_next = deadline.checked_add(interval).unwrap_or(deadline);
                    if let Ok(mut gate) = RATE_LIMIT_GATE.try_lock() {
                        if gate.next_admission == Some(expected_next) {
                            gate.next_admission = Some(deadline);
                        }
                    }
                    return Err(rate_limit_error(wait_seconds));
                }
            }

            let wait_seconds = rounded_up_seconds(remaining);
            tracing::warn!(
                "Rate limit reached. Waiting {wait_seconds} seconds before proceeding..."
            );
            // SlotGuard increments WAITER_COUNT and rolls back on drop if
            // the future is cancelled before commit().
            let mut guard = SlotGuard::new(deadline, interval);
            tokio::time::sleep_until(deadline).await;
            guard.commit();
            // Drop guard explicitly to decrement WAITER_COUNT before
            // recording admission time (keeps count accurate in tests).
            drop(guard);
            record_admission_time();
            tracing::info!("Rate limit wait completed, proceeding with request");
            Ok(())
        }
    }
}

/// Read `COPILOT_API_RATE_LIMIT_MAX_WAITERS` at call time so tests can set the
/// env var before calling without fighting with `Lazy` initialization order.
fn max_waiters_limit() -> Option<usize> {
    std::env::var("COPILOT_API_RATE_LIMIT_MAX_WAITERS")
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Read `COPILOT_API_RATE_LIMIT_MAX_WAIT_SECS` at call time for the same reason.
fn max_wait_secs_limit() -> Option<u64> {
    std::env::var("COPILOT_API_RATE_LIMIT_MAX_WAIT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
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

    /// AC3: A waiter whose future is cancelled before its sleep completes must
    /// not leave a phantom slot reservation that inflates wait times for
    /// subsequent callers.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn cancelled_waiter_does_not_leave_phantom_slot() {
        configure(Some(10), true).await;

        // First caller takes the immediate slot (T+0).
        check_rate_limit()
            .await
            .expect("first request is immediate");

        // Second caller enters WaitUntil and reserves T+10. Abort the task
        // immediately to simulate a client disconnect / cancellation.
        let second = tokio::spawn(check_rate_limit());
        tokio::task::yield_now().await; // let second reserve the slot
        second.abort();
        let _ = second.await; // join (always Err(Cancelled))

        // Without cancellation safety, gate.next_admission would be T+20, so a
        // third caller would sleep until T+20. With SlotGuard rollback the gate
        // should be reset to T+10, so the third caller wakes at T+10.
        let third = tokio::spawn(check_rate_limit());
        tokio::task::yield_now().await;
        assert!(
            !third.is_finished(),
            "third caller should be sleeping (T+10 not reached yet)"
        );

        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert!(
            third.is_finished(),
            "third caller should have woken at T+10 thanks to rollback (not T+20)"
        );
        third.await.expect("third task").expect("admitted");

        configure(None, false).await;
    }

    /// AC3: Reject incoming wait-mode requests when the waiter queue is full.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn wait_mode_rejects_when_max_waiters_exceeded() {
        // SAFETY: serial_test ensures no concurrent env mutations in this suite.
        unsafe {
            std::env::set_var("COPILOT_API_RATE_LIMIT_MAX_WAITERS", "1");
        }
        configure(Some(10), true).await;

        // Take the immediate slot.
        check_rate_limit().await.expect("immediate slot");

        // First waiter reserves T+10 and is within the limit (1 allowed).
        let first_waiter = tokio::spawn(check_rate_limit());
        tokio::task::yield_now().await;
        assert!(!first_waiter.is_finished());

        // Second waiter: WAITER_COUNT == 1 >= limit 1, should be rejected.
        let result = check_rate_limit().await;
        assert!(
            result.is_err(),
            "second waiter should be rejected (queue full)"
        );
        assert_eq!(result.unwrap_err().status, StatusCode::TOO_MANY_REQUESTS);

        // Clean up: advance time and join the first waiter.
        tokio::time::advance(Duration::from_secs(10)).await;
        first_waiter
            .await
            .expect("first waiter task")
            .expect("admitted");

        unsafe {
            std::env::remove_var("COPILOT_API_RATE_LIMIT_MAX_WAITERS");
        }
        configure(None, false).await;
    }

    /// AC3: Reject incoming wait-mode requests when the projected wait time
    /// exceeds the configured maximum.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn wait_mode_rejects_when_wait_exceeds_max_secs() {
        unsafe {
            std::env::set_var("COPILOT_API_RATE_LIMIT_MAX_WAIT_SECS", "5");
        }
        configure(Some(10), true).await;

        // Take the immediate slot.
        check_rate_limit().await.expect("immediate slot");

        // Would need to wait 10s, but max is 5s → must be rejected.
        let result = check_rate_limit().await;
        assert!(
            result.is_err(),
            "waiter should be rejected (wait 10s > max 5s)"
        );
        assert_eq!(result.unwrap_err().status, StatusCode::TOO_MANY_REQUESTS);

        unsafe {
            std::env::remove_var("COPILOT_API_RATE_LIMIT_MAX_WAIT_SECS");
        }
        configure(None, false).await;
    }
}
