//! Shared admission checks for every billable upstream request.
//!
//! Keep provider routing and other early-return dispatch branches *after* this
//! gate. Otherwise a provider alias (or a fulfilled web-search request) can
//! bypass the operator's rate limit and global/per-key daily token budgets.

use std::sync::Arc;

use once_cell::sync::Lazy;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::libs::error::HttpError;

/// An RAII guard that holds a semaphore permit for the duration of a single
/// in-flight request. The permit is released when this value is dropped —
/// meaning the slot is automatically freed when the streaming response
/// completes or the client disconnects. Wrapping in `Option` lets the
/// `no-limit` path (when `COPILOT_API_MAX_IN_FLIGHT` is unset) use
/// `InFlightPermit(None)` without any synchronisation overhead.
#[derive(Default)]
pub struct InFlightPermit(pub(crate) Option<OwnedSemaphorePermit>);

impl std::fmt::Debug for InFlightPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightPermit")
            .field("held", &self.0.is_some())
            .finish()
    }
}

/// Global semaphore controlling maximum concurrent in-flight requests.
/// `None` means unlimited. Reads `COPILOT_API_MAX_IN_FLIGHT` at first use.
static IN_FLIGHT_SEMAPHORE: Lazy<Option<Arc<Semaphore>>> = Lazy::new(|| {
    let max = std::env::var("COPILOT_API_MAX_IN_FLIGHT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)?;
    Some(Arc::new(Semaphore::new(max)))
});

/// Apply admission policies shared by Copilot, Codex, and third-party provider
/// requests. Provider-specific quota checks (for example Copilot premium
/// interactions) intentionally remain in their respective dispatch paths.
///
/// Returns an [`InFlightPermit`] that the caller must hold for the lifetime of
/// the request (typically by storing it in `StreamTimer` via
/// [`StreamTimer::with_in_flight_permit`]). When the permit is dropped the
/// semaphore slot is released, allowing the next queued request to proceed.
#[allow(clippy::result_large_err)]
pub async fn check_shared_admission() -> Result<InFlightPermit, HttpError> {
    crate::libs::rate_limit::check_rate_limit().await?;
    crate::libs::token_budget::check_token_budget().await?;

    // Acquire an in-flight slot (if a cap is configured). The `acquire_owned`
    // call parks the task until a slot is available rather than returning an
    // error, so callers behind the rate-limit gate may queue briefly here; the
    // rate-limit gate already serialises them so this is bounded in practice.
    let permit = match IN_FLIGHT_SEMAPHORE.as_ref() {
        None => InFlightPermit(None),
        Some(sem) => {
            let owned =
                sem.clone().acquire_owned().await.map_err(|_| {
                    HttpError::internal("In-flight limit reached — semaphore closed")
                })?;
            InFlightPermit(Some(owned))
        }
    };

    Ok(permit)
}
