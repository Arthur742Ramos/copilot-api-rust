use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::libs::error::AppError;
use crate::libs::models::to_client_model_id;
use crate::libs::state;
use crate::libs::utils::cache_models;

/// GET /models — mirrors routes/models/route.ts.
pub async fn get_models_route() -> Response {
    match build_models().await {
        Ok(value) => Json(value).into_response(),
        Err(error) => AppError::into_response(error),
    }
}

async fn build_models() -> Result<Value, AppError> {
    if state::with_state(|s| s.models.is_none()) {
        cache_models()
            .await
            .map_err(AppError::Other)?;
    }

    let models = state::with_state(|s| s.models.as_ref().map(|m| m.data.clone()).unwrap_or_default());

    let data: Vec<Value> = models
        .into_iter()
        .map(|model| {
            let context_window = model
                .capabilities
                .limits
                .max_context_window_tokens
                .unwrap_or(0);
            let client_id = to_client_model_id(&model.id);
            let is_1m = context_window >= 1_000_000;
            let claude_model_id = if is_1m {
                format!("{client_id}[1m]")
            } else {
                client_id.clone()
            };

            // Start from the model's own JSON, then overlay the route's fields.
            let mut obj = serde_json::to_value(&model).unwrap_or(json!({}));
            if let Some(map) = obj.as_object_mut() {
                map.insert("claude_model_id".to_string(), json!(claude_model_id));
                map.insert("id".to_string(), json!(client_id));
                map.insert("object".to_string(), json!("model"));
                map.insert("type".to_string(), json!("model"));
                map.insert("created".to_string(), json!(0));
                map.insert(
                    "created_at".to_string(),
                    json!("1970-01-01T00:00:00.000Z"),
                );
                map.insert("owned_by".to_string(), json!(model.vendor));
                map.insert("display_name".to_string(), json!(model.name));
            }
            obj
        })
        .collect();

    Ok(json!({
        "object": "list",
        "data": data,
        "has_more": false,
    }))
}
