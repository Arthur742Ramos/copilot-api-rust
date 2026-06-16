//! POST /responses — mirrors routes/responses/route.ts.

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::libs::error::AppError;

use super::handler::handle_responses;

/// POST `/responses` (and `/v1/responses`). The thin route fn converts handler
/// errors via `AppError::into_response`, mirroring the TS `forwardError` wrap.
pub async fn post_responses(headers: HeaderMap, body: Json<Value>) -> Response {
    match handle_responses(body.0, headers).await {
        Ok(response) => response,
        Err(error) => AppError::into_response(error),
    }
}
