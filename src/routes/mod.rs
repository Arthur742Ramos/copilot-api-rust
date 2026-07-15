//! HTTP route handlers for the proxy's public API surface (chat completions,
//! messages, responses, embeddings, models, token, and usage endpoints).

pub mod admin_config;
pub mod alpha_search;
pub mod chat_completions;
pub mod embeddings;
pub mod files;
pub mod images;
pub mod messages;
pub mod models;
pub mod provider;
pub mod responses;
pub mod token;
pub mod token_usage;
pub mod usage;

/// Parse a raw request body as JSON, returning a JSON-shaped 400
/// (`invalid_request_error`) on failure instead of axum's default plain-text
/// `Json<Value>` extractor rejection. Used by the POST route wrappers so a
/// malformed body produces the same error envelope as every other client error.
#[allow(clippy::result_large_err)]
pub fn parse_json_body(
    body: &axum::body::Bytes,
) -> Result<serde_json::Value, crate::libs::error::AppError> {
    serde_json::from_slice(body)
        .map_err(|e| crate::libs::error::AppError::BadRequest(format!("Invalid JSON: {e}")))
}
