//! Admission controls for upstream proxy requests.
//!
//! Concurrency admission is enforced by router middleware before request
//! preprocessing. Billing/rate policies remain in the handlers because some
//! internal dispatch paths deliberately share one outer request admission.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use http_body_util::BodyExt;
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::libs::error::HttpError;

/// Recommended desktop limit: each admitted proxy request normally owns one
/// inbound and one upstream socket, leaving headroom under a common 256-FD soft
/// limit. The CLI remains unlimited unless the operator configures this value.
pub const RECOMMENDED_MAX_CONCURRENT_REQUESTS: usize = 64;

/// Short client backoff for local load shedding. The request was never dispatched
/// upstream, so retrying after capacity drains is safe.
pub const OVERLOAD_RETRY_AFTER_SECS: u64 = 1;

const OVERLOAD_LOG_INTERVAL_SECS: u64 = 10;
const LIMIT_METRIC: &str = "proxy_upstream_concurrency_limit";
const ACTIVE_METRIC: &str = "proxy_upstream_requests_active";
const REJECTIONS_METRIC: &str = "proxy_upstream_overload_rejections_total";

static LAST_OVERLOAD_LOG_SECS: AtomicU64 = AtomicU64::new(0);
static SUPPRESSED_OVERLOAD_LOGS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct AdmissionInner {
    semaphore: Option<Arc<Semaphore>>,
    limit: Option<NonZeroUsize>,
    active: AtomicUsize,
}

#[derive(Clone, Debug)]
pub struct AdmissionController {
    inner: Arc<AdmissionInner>,
}

impl AdmissionController {
    pub fn new(limit: Option<NonZeroUsize>) -> Self {
        let controller = Self {
            inner: Arc::new(AdmissionInner {
                semaphore: limit.map(|value| Arc::new(Semaphore::new(value.get()))),
                limit,
                active: AtomicUsize::new(0),
            }),
        };
        controller.initialize_metrics();
        controller
    }

    pub fn limited(limit: NonZeroUsize) -> Self {
        Self::new(Some(limit))
    }

    pub fn unlimited() -> Self {
        Self::new(None)
    }

    pub fn limit(&self) -> Option<usize> {
        self.inner.limit.map(NonZeroUsize::get)
    }

    pub fn current(&self) -> usize {
        self.inner.active.load(Ordering::Relaxed)
    }

    /// Acquire immediately or reject. This never waits and therefore never builds
    /// a queue of requests that retain inbound sockets while the proxy is full.
    pub fn try_acquire(&self) -> Option<AdmissionPermit> {
        let semaphore_permit = match &self.inner.semaphore {
            Some(semaphore) => match Arc::clone(semaphore).try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    self.record_overload();
                    return None;
                }
            },
            None => None,
        };

        self.inner.active.fetch_add(1, Ordering::Relaxed);
        metrics::gauge!(ACTIVE_METRIC).increment(1.0);
        Some(AdmissionPermit {
            inner: Arc::clone(&self.inner),
            _semaphore_permit: semaphore_permit,
        })
    }

    fn initialize_metrics(&self) {
        let _ = crate::libs::metrics::metrics_handle();
        metrics::gauge!(LIMIT_METRIC).set(self.limit().unwrap_or(0) as f64);
        metrics::gauge!(ACTIVE_METRIC).set(0.0);
        metrics::counter!(REJECTIONS_METRIC).increment(0);
    }

    fn record_overload(&self) {
        metrics::counter!(REJECTIONS_METRIC).increment(1);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let last = LAST_OVERLOAD_LOG_SECS.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= OVERLOAD_LOG_INTERVAL_SECS
            && LAST_OVERLOAD_LOG_SECS
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let suppressed = SUPPRESSED_OVERLOAD_LOGS.swap(0, Ordering::Relaxed);
            tracing::warn!(
                limit = self.limit().unwrap_or(0),
                active = self.current(),
                suppressed,
                "upstream request rejected because proxy concurrency is saturated"
            );
        } else {
            SUPPRESSED_OVERLOAD_LOGS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Default for AdmissionController {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// Tracks one active request and, when configured, owns one semaphore slot.
/// Dropping the response body drops this guard on every terminal path: EOF, body
/// error, downstream cancellation, or panic unwinding.
#[derive(Debug)]
pub struct AdmissionPermit {
    inner: Arc<AdmissionInner>,
    _semaphore_permit: Option<OwnedSemaphorePermit>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.inner.active.fetch_sub(1, Ordering::Relaxed);
        metrics::gauge!(ACTIVE_METRIC).decrement(1.0);
    }
}

/// Middleware variant that guards every route in the router it is applied to.
/// The production router uses the same implementation after selecting only
/// upstream-facing route templates.
pub async fn admission_middleware(
    State(controller): State<AdmissionController>,
    request: Request,
    next: Next,
) -> Response {
    admit_request(controller, request, next).await
}

pub async fn admit_request(
    controller: AdmissionController,
    request: Request,
    next: Next,
) -> Response {
    let openai_native = crate::libs::error::is_openai_native_path(request.uri().path());
    let Some(permit) = controller.try_acquire() else {
        return overload_response(openai_native);
    };

    let response = next.run(request).await;
    attach_permit(response, permit)
}

fn attach_permit(response: Response, permit: AdmissionPermit) -> Response {
    let (parts, body) = response.into_parts();
    let guarded = body.map_frame(move |frame| {
        let _keep_permit_alive = &permit;
        frame
    });
    Response::from_parts(parts, Body::new(guarded))
}

fn overload_response(openai_native: bool) -> Response {
    let mut response = if openai_native {
        crate::libs::error::openai_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            Some("server_overloaded"),
            "Proxy concurrency limit reached. Retry shortly.",
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "overloaded_error",
                    "message": "Proxy concurrency limit reached. Retry shortly.",
                }
            })),
        )
            .into_response()
    };
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from(OVERLOAD_RETRY_AFTER_SECS),
    );
    response
}

/// Apply billing/rate policies shared by Copilot, Codex, and third-party provider
/// requests. The router-level concurrency guard always runs before this function.
#[allow(clippy::result_large_err)]
pub async fn check_shared_admission() -> Result<(), HttpError> {
    crate::libs::rate_limit::check_rate_limit().await?;
    crate::libs::token_budget::check_token_budget().await?;
    Ok(())
}
