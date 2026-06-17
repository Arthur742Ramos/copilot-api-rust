//! POST /responses — mirrors routes/responses/route.ts.

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::libs::error::AppError;
use crate::routes::parse_json_body;

use super::handler::handle_responses;

/// POST `/responses` (and `/v1/responses`). The thin route fn converts handler
/// errors via `AppError::into_response`, mirroring the TS `forwardError` wrap.
pub async fn post_responses(headers: HeaderMap, body: Bytes) -> Response {
    let payload = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    match handle_responses(payload, headers).await {
        Ok(response) => response,
        Err(error) => AppError::into_response(error),
    }
}
