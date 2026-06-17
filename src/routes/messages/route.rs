//! Mirrors `src/routes/messages/route.ts`.
//!
//! Thin axum handlers that wrap the message-completion lifecycle and render
//! [`AppError`] via the shared `forwardError` equivalent.

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::libs::error::AppError;
use crate::routes::parse_json_body;

use super::handler::handle_completion;

/// POST /v1/messages — mirrors `messageRoutes.post("/")`.
pub async fn post_messages(headers: HeaderMap, body: Bytes) -> Response {
    let payload = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    match handle_completion(payload, headers).await {
        Ok(response) => response,
        Err(error) => AppError::into_response(error),
    }
}

/// POST /v1/messages/count_tokens — mirrors `messageRoutes.post("/count_tokens")`.
pub async fn post_count_tokens(headers: HeaderMap, body: Bytes) -> Response {
    let payload = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    match super::count_tokens_handler::handle_count_tokens(payload, headers).await {
        Ok(response) => response,
        Err(error) => AppError::into_response(error),
    }
}
