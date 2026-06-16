//! Mirrors `src/routes/messages/route.ts`.
//!
//! Thin axum handlers that wrap the message-completion lifecycle and render
//! [`AppError`] via the shared `forwardError` equivalent.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::libs::error::AppError;

use super::handler::handle_completion;

/// POST /v1/messages — mirrors `messageRoutes.post("/")`.
pub async fn post_messages(headers: HeaderMap, body: Json<Value>) -> Response {
    match handle_completion(body.0, headers).await {
        Ok(response) => response,
        Err(error) => AppError::into_response(error),
    }
}

/// POST /v1/messages/count_tokens — mirrors `messageRoutes.post("/count_tokens")`.
///
/// TODO count_tokens: the orchestrator wires the tiktoken-backed handler
/// separately. Until then this returns a 501 stub.
pub async fn post_count_tokens(_headers: HeaderMap, _body: Json<Value>) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": {
                "message": "count_tokens is not yet implemented",
                "type": "invalid_request_error",
            }
        })),
    )
        .into_response()
}
