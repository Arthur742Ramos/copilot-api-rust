//! Mirrors `src/routes/messages/route.ts`.
//!
//! Thin axum handlers that wrap the message-completion lifecycle and render
//! [`AppError`] via the shared `forwardError` equivalent.

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

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
pub async fn post_count_tokens(headers: HeaderMap, body: Json<Value>) -> Response {
    match super::count_tokens_handler::handle_count_tokens(body.0, headers).await {
        Ok(response) => response,
        Err(error) => AppError::into_response(error),
    }
}
