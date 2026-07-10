//! Shared admission checks for every billable upstream request.
//!
//! Keep provider routing and other early-return dispatch branches *after* this
//! gate. Otherwise a provider alias (or a fulfilled web-search request) can
//! bypass the operator's rate limit and global/per-key daily token budgets.

use crate::libs::error::HttpError;

/// Apply admission policies shared by Copilot, Codex, and third-party provider
/// requests. Provider-specific quota checks (for example Copilot premium
/// interactions) intentionally remain in their respective dispatch paths.
#[allow(clippy::result_large_err)]
pub async fn check_shared_admission() -> Result<(), HttpError> {
    crate::libs::rate_limit::check_rate_limit().await?;
    crate::libs::token_budget::check_token_budget()?;
    Ok(())
}
