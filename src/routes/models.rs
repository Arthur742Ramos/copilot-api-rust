//! `/v1/models` endpoint: serves the list of available Copilot models.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::libs::config::{
    get_config, get_model_mappings, get_provider_config, resolve_mapped_model,
};
use crate::libs::error::{openai_error_response, AppError};
use crate::libs::models::{is_context_1m_model, strip_context_1m_suffix, to_client_model_id};
use crate::libs::provider_model::parse_provider_model_alias;
use crate::libs::state;
use crate::libs::utils::cache_models;
use crate::services::copilot::get_models::Model;

/// GET /models — mirrors routes/models/route.ts.
pub async fn get_models_route() -> Response {
    match build_models().await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_openai_response(),
    }
}

/// GET /models/:id — retrieve a single model object (OpenAI Models API). Honors
/// the non-standard `[1m]` 1M-context variant suffix, and 404s an unknown id.
pub async fn get_model_route(Path(requested): Path<String>) -> Response {
    let resolved = resolve_mapped_model(&requested);
    if let Some(model) = provider_model_record(&resolved, Some(&requested)) {
        return Json(model).into_response();
    }

    if state::with_state(|s| s.models.is_none()) {
        if state::with_state(|s| s.provider_only.is_some()) {
            return model_not_found(&requested);
        }
        if let Err(error) = cache_models().await {
            return AppError::Other(error).into_openai_response();
        }
    }

    // Clone the `Arc<ModelsResponse>` (a refcount bump), not the model `Vec`.
    let models = state::with_state(|s| s.models.clone());
    let models = match models {
        Some(m) => m,
        None => return model_not_found(&requested),
    };

    // Normalize the requested id the same way the list route advertises ids, so
    // a date-suffixed / raw upstream id resolves like every other endpoint.
    let want_1m = is_context_1m_model(&resolved);
    let base_requested = to_client_model_id(strip_context_1m_suffix(&resolved));

    let found = models
        .data
        .iter()
        .find(|model| to_client_model_id(&model.id) == base_requested);

    match found {
        Some(model) => {
            // Only advertise the [1m] variant when the model is actually
            // 1M-context-capable, matching the list route's own gating.
            let is_1m_capable = model
                .capabilities
                .limits
                .max_context_window_tokens
                .unwrap_or(0)
                >= 1_000_000;
            let shaped = shape_model(model, want_1m && is_1m_capable);
            if requested == resolved {
                Json(shaped).into_response()
            } else {
                Json(shape_mapped_model(shaped, &requested, &resolved)).into_response()
            }
        }
        None => model_not_found(&requested),
    }
}

fn model_not_found(id: &str) -> Response {
    openai_error_response(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        Some("model_not_found"),
        format!("The model `{id}` does not exist."),
    )
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

fn shape_mapped_model(mut model: Value, alias: &str, target: &str) -> Value {
    if let Some(map) = model.as_object_mut() {
        map.insert("id".to_string(), json!(alias));
        map.insert("claude_model_id".to_string(), json!(alias));
        map.insert("mapped_to".to_string(), json!(target));
    }
    model
}

fn provider_model_record(model_id: &str, alias: Option<&str>) -> Option<Value> {
    let parsed = parse_provider_model_alias(model_id)?;
    let provider = get_provider_config(&parsed.provider)?;
    if let Some(models) = provider.models.as_ref() {
        if !models.contains_key(&parsed.model) {
            return None;
        }
    }
    let id = alias.unwrap_or(model_id);
    Some(json!({
        "id": id,
        "object": "model",
        "type": "model",
        "created": 0,
        "created_at": "1970-01-01T00:00:00.000Z",
        "owned_by": parsed.provider,
        "display_name": parsed.model,
        "claude_model_id": id,
        "mapped_to": model_id,
    }))
}

async fn build_models() -> Result<Value, AppError> {
    let config = get_config();
    // Normal Copilot startup primes this cache before serving. In provider-only
    // mode it is intentionally absent; do not trigger unrelated GitHub
    // initialization there. Outside provider-only mode, a missing cache must be
    // populated even when mappings exist because they may target Copilot models.
    let (models_missing, provider_only) =
        state::with_state(|state| (state.models.is_none(), state.provider_only.is_some()));
    if models_missing && !provider_only {
        cache_models().await.map_err(AppError::Other)?;
    }

    let models = state::with_state(|s| {
        s.models
            .as_ref()
            .map(|m| m.data.clone())
            .unwrap_or_default()
    });

    let mut data: Vec<Value> = models
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

    if let Some(providers) = config.providers.as_ref() {
        for provider_name in providers.keys() {
            let Some(provider) = get_provider_config(provider_name) else {
                continue;
            };
            if let Some(provider_models) = provider.models.as_ref() {
                for model_name in provider_models.keys() {
                    let id = format!("{provider_name}/{model_name}");
                    if !data.iter().any(|model| model["id"] == id) {
                        data.push(json!({
                            "id": id,
                            "object": "model",
                            "type": "model",
                            "created": 0,
                            "created_at": "1970-01-01T00:00:00.000Z",
                            "owned_by": provider_name,
                            "display_name": model_name,
                            "claude_model_id": id,
                        }));
                    }
                }
            }
        }
    }

    for (alias, target) in get_model_mappings() {
        if data.iter().any(|model| model["id"] == alias) {
            continue;
        }
        let mapped = data
            .iter()
            .find(|model| model["id"] == target)
            .cloned()
            .map(|model| shape_mapped_model(model, &alias, &target))
            .or_else(|| provider_model_record(&target, Some(&alias)));
        if let Some(mapped) = mapped {
            data.push(mapped);
        } else {
            tracing::warn!(
                alias,
                target,
                "Omitting model mapping whose target is not available"
            );
        }
    }

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

    #[test]
    fn mapped_model_keeps_openai_and_anthropic_aliases_aligned() {
        let obj = shape_mapped_model(
            shape_model(&sample_model(), false),
            "coding-default",
            "gpt-5-mini",
        );
        assert_eq!(obj["id"], "coding-default");
        assert_eq!(obj["claude_model_id"], "coding-default");
        assert_eq!(obj["mapped_to"], "gpt-5-mini");
    }
}
