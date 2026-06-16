use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::libs::config::{get_model_mappings, set_model_mappings};
use crate::libs::paths::PATHS;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelMappingsRequest {
    model_mappings: BTreeMap<String, String>,
}

fn config_path_string() -> String {
    PATHS.config_path.to_string_lossy().into_owned()
}

/// GET /admin/config/model-mappings — mirrors routes/admin/config/route.ts.
pub async fn get_model_mappings_route() -> Response {
    Json(json!({
        "configPath": config_path_string(),
        "modelMappings": get_model_mappings(),
    }))
    .into_response()
}

/// POST /admin/config/model-mappings — validates and persists the mapping table.
pub async fn post_model_mappings_route(body: Bytes) -> Response {
    // Mirror `await c.req.json()`: an unparseable body throws -> forwardError (500).
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return forwarded_error(&error.to_string()),
    };

    // Mirror the zod safeParse gate: valid JSON of the wrong shape -> 400.
    let parsed: ModelMappingsRequest = match serde_json::from_value(value) {
        Ok(parsed) => parsed,
        Err(_) => return invalid_request("Invalid request body."),
    };

    // Mirror setModelMappings(): validation/persistence failures throw -> 500.
    match set_model_mappings(&parsed.model_mappings) {
        Ok(updated) => Json(json!({
            "configPath": config_path_string(),
            "modelMappings": updated,
        }))
        .into_response(),
        Err(error) => forwarded_error(&error.to_string()),
    }
}

fn invalid_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
            }
        })),
    )
        .into_response()
}

/// Mirrors forwardError() for a non-HTTPError throw: 500 with `type: "error"`.
fn forwarded_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "message": message,
                "type": "error",
            }
        })),
    )
        .into_response()
}
