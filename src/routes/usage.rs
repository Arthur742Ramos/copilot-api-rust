use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::libs::state;
use crate::services::github::get_copilot_usage::get_copilot_usage;

/// GET /usage — mirrors routes/usage/route.ts.
pub async fn get_usage() -> Response {
    let snapshot = state::snapshot();
    match get_copilot_usage(&snapshot, None).await {
        Ok(usage) => Json(usage).into_response(),
        Err(error) => {
            tracing::error!("Error fetching Copilot usage: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to fetch Copilot usage" })),
            )
                .into_response()
        }
    }
}
