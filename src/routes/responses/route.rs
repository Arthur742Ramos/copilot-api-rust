//! POST /responses — mirrors routes/responses/route.ts.

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::routes::parse_json_body;

use super::handler::handle_responses;

/// POST `/responses` (and `/v1/responses`). Errors use the OpenAI envelope
/// expected by Codex rather than the crate's Anthropic default.
pub async fn post_responses(headers: HeaderMap, body: Bytes) -> Response {
    let payload = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_openai_response(),
    };
    match handle_responses(payload, headers).await {
        Ok(response) => response,
        Err(error) => error.into_openai_response(),
    }
}
