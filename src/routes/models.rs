//! `/v1/models` endpoint: serves the list of available Copilot models.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::libs::error::AppError;
use crate::libs::models::{is_context_1m_model, strip_context_1m_suffix, to_client_model_id};
use crate::libs::state;
use crate::libs::utils::cache_models;
use crate::services::copilot::get_models::Model;

/// GET /models — mirrors routes/models/route.ts.
pub async fn get_models_route() -> Response {
    match build_models().await {
        Ok(value) => Json(value).into_response(),
        Err(error) => AppError::into_response(error),
    }
}

/// GET /models/:id — retrieve a single model object (OpenAI Models API). Honors
/// the non-standard `[1m]` 1M-context variant suffix, and 404s an unknown id.
pub async fn get_model_route(Path(requested): Path<String>) -> Response {
    if state::with_state(|s| s.models.is_none()) {
        if let Err(error) = cache_models().await {
            return AppError::into_response(AppError::Other(error));
        }
    }

    let models = state::with_state(|s| {
        s.models
            .as_ref()
            .map(|m| m.data.clone())
            .unwrap_or_default()
    });

    let want_1m = is_context_1m_model(&requested);
    let base_requested = strip_context_1m_suffix(&requested);

    let found = models
        .iter()
        .find(|model| to_client_model_id(&model.id) == base_requested);

    match found {
        Some(model) => Json(shape_model(model, want_1m)).into_response(),
        None => model_not_found(&requested),
    }
}

fn model_not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "message": format!("The model `{id}` does not exist."),
                "type": "not_found_error",
            }
        })),
    )
        .into_response()
}

/// Shape one upstream [`Model`] into the OpenAI/Anthropic client model object.
/// `as_1m` advertises the `[1m]` variant id (only valid for 1M-context models).
fn shape_model(model: &Model, as_1m: bool) -> Value {
    let client_id = to_client_model_id(&model.id);
    let claude_model_id = if as_1m {
        format!("{client_id}[1m]")
    } else {
        client_id.clone()
    };

    let mut obj = serde_json::to_value(model).unwrap_or(json!({}));
    if let Some(map) = obj.as_object_mut() {
        map.insert("claude_model_id".to_string(), json!(claude_model_id));
        map.insert("id".to_string(), json!(client_id));
        map.insert("object".to_string(), json!("model"));
        map.insert("type".to_string(), json!("model"));
        map.insert("created".to_string(), json!(0));
        map.insert("created_at".to_string(), json!("1970-01-01T00:00:00.000Z"));
        map.insert("owned_by".to_string(), json!(model.vendor));
        map.insert("display_name".to_string(), json!(model.name));
    }
    obj
}

async fn build_models() -> Result<Value, AppError> {
    if state::with_state(|s| s.models.is_none()) {
        cache_models().await.map_err(AppError::Other)?;
    }

    let models = state::with_state(|s| {
        s.models
            .as_ref()
            .map(|m| m.data.clone())
            .unwrap_or_default()
    });

    let data: Vec<Value> = models
        .into_iter()
        .map(|model| {
            let context_window = model
                .capabilities
                .limits
                .max_context_window_tokens
                .unwrap_or(0);
            let is_1m = context_window >= 1_000_000;
            shape_model(&model, is_1m)
        })
        .collect();

    Ok(json!({
        "object": "list",
        "data": data,
        "has_more": false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_model() -> Model {
        let mut m = Model {
            id: "gpt-5-mini".to_string(),
            name: "GPT-5 Mini".to_string(),
            vendor: "openai".to_string(),
            ..Default::default()
        };
        m.capabilities.limits.max_context_window_tokens = Some(2_000_000);
        m
    }

    #[test]
    fn shape_model_overlays_client_fields() {
        let obj = shape_model(&sample_model(), false);
        assert_eq!(obj["id"], "gpt-5-mini");
        assert_eq!(obj["object"], "model");
        assert_eq!(obj["type"], "model");
        assert_eq!(obj["owned_by"], "openai");
        assert_eq!(obj["display_name"], "GPT-5 Mini");
        // Non-1m: claude_model_id has no suffix.
        assert_eq!(obj["claude_model_id"], "gpt-5-mini");
    }

    #[test]
    fn shape_model_advertises_1m_variant() {
        let obj = shape_model(&sample_model(), true);
        assert_eq!(obj["id"], "gpt-5-mini");
        assert_eq!(obj["claude_model_id"], "gpt-5-mini[1m]");
    }
}
