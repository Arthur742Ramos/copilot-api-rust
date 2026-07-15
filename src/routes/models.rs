//! `/v1/models` endpoint: serves the list of available Copilot models.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::libs::config::{
    are_claude_code_model_discovery_aliases_enabled, get_config, get_model_mappings,
    get_provider_config, resolve_mapped_model,
};
use crate::libs::error::{openai_error_response, AppError};
use crate::libs::models::{is_context_1m_model, strip_context_1m_suffix, to_client_model_id};
use crate::libs::provider_model::{create_claude_code_discovery_alias, parse_provider_model_alias};
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
        add_anthropic_model_metadata(map, model);
    }
    obj
}

fn capability_support(supported: bool) -> Value {
    json!({ "supported": supported })
}

fn add_anthropic_model_metadata(map: &mut Map<String, Value>, model: &Model) {
    if let Some(max_input_tokens) = model.capabilities.limits.max_context_window_tokens {
        map.insert("max_input_tokens".to_string(), json!(max_input_tokens));
    }
    if let Some(max_tokens) = model.capabilities.limits.max_output_tokens {
        map.insert("max_tokens".to_string(), json!(max_tokens));
    }

    let capabilities = map
        .entry("capabilities".to_string())
        .or_insert_with(|| json!({}));
    let Some(capabilities) = capabilities.as_object_mut() else {
        return;
    };

    let efforts = model
        .capabilities
        .supports
        .reasoning_effort
        .as_deref()
        .unwrap_or_default();
    let mut effort = Map::new();
    let mut effort_supported = false;
    for level in ["low", "medium", "high", "xhigh", "max"] {
        let supported = efforts.iter().any(|candidate| candidate == level);
        effort_supported |= supported;
        effort.insert(level.to_string(), capability_support(supported));
    }
    effort.insert("supported".to_string(), json!(effort_supported));
    capabilities.insert("effort".to_string(), Value::Object(effort));

    let adaptive = model.capabilities.supports.adaptive_thinking == Some(true);
    let enabled = model.capabilities.supports.max_thinking_budget.unwrap_or(0) > 0;
    capabilities.insert(
        "thinking".to_string(),
        json!({
            "supported": adaptive || enabled,
            "types": {
                "adaptive": { "supported": adaptive },
                "enabled": { "supported": enabled },
            }
        }),
    );

    if let Some(supported) = model.capabilities.supports.structured_outputs {
        capabilities.insert(
            "structured_outputs".to_string(),
            capability_support(supported),
        );
    }
    if let Some(supported) = model.capabilities.supports.vision {
        capabilities.insert("image_input".to_string(), capability_support(supported));
    }
}

fn is_natively_discoverable_by_claude_code(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.starts_with("claude") || lower.starts_with("anthropic")
}

fn format_context_window(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        if tokens % 1_000_000 == 0 {
            format!("{}M", tokens / 1_000_000)
        } else {
            let formatted = format!("{:.2}", tokens as f64 / 1_000_000.0);
            format!("{}M", formatted.trim_end_matches('0').trim_end_matches('.'))
        }
    } else if tokens >= 1_000 && tokens % 1_000 == 0 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn claude_code_alias_display_name(model: &Model) -> String {
    let name = if model.name.trim().is_empty() {
        model.id.as_str()
    } else {
        model.name.as_str()
    };
    let mut details = vec!["Copilot".to_string()];
    if let Some(context) = model.capabilities.limits.max_context_window_tokens {
        details.push(format!("{} context", format_context_window(context)));
    }
    if let Some(efforts) = model.capabilities.supports.reasoning_effort.as_ref() {
        if !efforts.is_empty() {
            details.push(format!("effort {}", efforts.join("/")));
        }
    }
    format!("{name} ({})", details.join(" | "))
}

fn shape_claude_code_discovery_alias(model: &Model) -> Option<Value> {
    let client_id = to_client_model_id(&model.id);
    if !model.model_picker_enabled || is_natively_discoverable_by_claude_code(&client_id) {
        return None;
    }

    let context_1m = model
        .capabilities
        .limits
        .max_context_window_tokens
        .unwrap_or(0)
        >= 1_000_000;
    let alias = create_claude_code_discovery_alias(&client_id, context_1m);
    let mut shaped = shape_model(model, false);
    if let Some(map) = shaped.as_object_mut() {
        map.insert("id".to_string(), json!(alias));
        map.insert("claude_model_id".to_string(), json!(alias));
        map.insert("mapped_to".to_string(), json!(client_id));
        map.insert("copilot_model_id".to_string(), json!(client_id));
        map.insert(
            "display_name".to_string(),
            json!(claude_code_alias_display_name(model)),
        );
    }
    Some(shaped)
}

fn shape_copilot_models(models: &[Model], include_claude_code_aliases: bool) -> Vec<Value> {
    let mut data: Vec<Value> = models
        .iter()
        .map(|model| {
            let context_window = model
                .capabilities
                .limits
                .max_context_window_tokens
                .unwrap_or(0);
            shape_model(model, context_window >= 1_000_000)
        })
        .collect();

    if include_claude_code_aliases {
        data.extend(models.iter().filter_map(shape_claude_code_discovery_alias));
    }
    data
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

    let mut data = shape_copilot_models(&models, are_claude_code_model_discovery_aliases_enabled());

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

    let first_id = data
        .first()
        .and_then(|model| model.get("id"))
        .and_then(Value::as_str);
    let last_id = data
        .last()
        .and_then(|model| model.get("id"))
        .and_then(Value::as_str);

    Ok(json!({
        "object": "list",
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
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
        m.capabilities.limits.max_output_tokens = Some(128_000);
        m.capabilities.supports.reasoning_effort = Some(vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
            "xhigh".to_string(),
        ]);
        m.capabilities.supports.adaptive_thinking = Some(true);
        m.model_picker_enabled = true;
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
        assert_eq!(obj["max_input_tokens"], 2_000_000);
        assert_eq!(obj["max_tokens"], 128_000);
        assert_eq!(obj["capabilities"]["effort"]["supported"], true);
        assert_eq!(obj["capabilities"]["effort"]["xhigh"]["supported"], true);
        assert_eq!(obj["capabilities"]["effort"]["max"]["supported"], false);
        assert_eq!(obj["capabilities"]["thinking"]["supported"], true);
        assert_eq!(
            obj["capabilities"]["thinking"]["types"]["adaptive"]["supported"],
            true
        );
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

    #[test]
    fn claude_code_alias_is_discoverable_and_describes_capabilities() {
        let obj = shape_claude_code_discovery_alias(&sample_model()).unwrap();
        assert_eq!(obj["id"], "claude-copilot:gpt-5-mini[1m]");
        assert_eq!(obj["mapped_to"], "gpt-5-mini");
        assert_eq!(obj["copilot_model_id"], "gpt-5-mini");
        assert_eq!(
            obj["display_name"],
            "GPT-5 Mini (Copilot | 2M context | effort low/medium/high/xhigh)"
        );
    }

    #[test]
    fn claude_code_alias_skips_native_claude_and_hidden_models() {
        let mut model = sample_model();
        model.id = "claude-sonnet-5".to_string();
        assert!(shape_claude_code_discovery_alias(&model).is_none());

        model.id = "gpt-5-mini".to_string();
        model.model_picker_enabled = false;
        assert!(shape_claude_code_discovery_alias(&model).is_none());
    }

    #[test]
    fn context_window_labels_keep_useful_precision() {
        assert_eq!(format_context_window(1_050_000), "1.05M");
        assert_eq!(format_context_window(1_000_000), "1M");
        assert_eq!(format_context_window(400_000), "400K");
        assert_eq!(format_context_window(123_456), "123456");
    }

    #[test]
    fn copilot_catalog_adds_discovery_aliases_only_when_enabled() {
        let model = sample_model();
        let without_aliases = shape_copilot_models(std::slice::from_ref(&model), false);
        assert_eq!(without_aliases.len(), 1);
        assert_eq!(without_aliases[0]["id"], "gpt-5-mini");

        let with_aliases = shape_copilot_models(&[model], true);
        assert_eq!(with_aliases.len(), 2);
        assert_eq!(with_aliases[0]["id"], "gpt-5-mini");
        assert_eq!(with_aliases[1]["id"], "claude-copilot:gpt-5-mini[1m]");
    }
}
