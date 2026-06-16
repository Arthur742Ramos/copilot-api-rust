//! Port of `src/routes/messages/count-tokens-handler.ts`.
//!
//! Token counting for the Anthropic `/v1/messages/count_tokens` endpoint. When
//! an Anthropic API key is configured and the model is a Claude model, the count
//! is forwarded to Anthropic's free `/v1/messages/count_tokens` endpoint for an
//! exact result. Otherwise it falls back to GPT tokenizer estimation.

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::libs::config::{
    get_anthropic_api_key, get_claude_token_multiplier, resolve_mapped_model,
};
use crate::libs::error::AppError;
use crate::libs::http::client;
use crate::libs::models::find_endpoint_model;
use crate::libs::provider_model::{create_fallback_model, parse_provider_model_alias};
use crate::libs::tokenizer::get_token_count;
use crate::routes::messages::anthropic_types::AnthropicMessagesPayload;
use crate::routes::messages::non_stream_translation::translate_to_openai;
use crate::routes::messages::preprocess::normalize_system_messages;
use crate::routes::provider::count_tokens::handle_provider_count_tokens_for_provider;
use crate::services::copilot::get_models::Model;

/// Result of [`resolve_count_tokens_model`], mirroring the TS object.
pub struct ResolvedCountTokensModel {
    pub fallback: bool,
    pub model: Model,
}

/// Mirrors `resolveCountTokensModel`.
pub fn resolve_count_tokens_model(model_id: &str) -> ResolvedCountTokensModel {
    if let Some(model) = find_endpoint_model(model_id) {
        return ResolvedCountTokensModel {
            fallback: false,
            model,
        };
    }
    ResolvedCountTokensModel {
        fallback: true,
        model: create_fallback_model(model_id.trim()),
    }
}

/// Mirrors `countTokensViaAnthropic`: forward to Anthropic's real
/// `/v1/messages/count_tokens` endpoint. Returns `Some(response)` on success, or
/// `None` to fall through to estimation.
async fn count_tokens_via_anthropic(payload: &AnthropicMessagesPayload) -> Option<Response> {
    if !payload.model.starts_with("claude") {
        return None;
    }

    let api_key = get_anthropic_api_key()?;

    // Copilot uses dotted names (claude-opus-4.6) but Anthropic requires dashes
    // (claude-opus-4-6).
    let model = payload.model.replace('.', "-");

    let mut body = serde_json::to_value(payload).ok()?;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(model));
    }

    let res = client()
        .post("https://api.anthropic.com/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "token-counting-2024-11-01")
        .json(&body)
        .send()
        .await
        .ok()?;

    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        tracing::warn!(
            "Anthropic count_tokens failed: {status} {text} - falling back to estimation"
        );
        return None;
    }

    let result: Value = res.json().await.ok()?;
    let input_tokens = result
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    tracing::info!("Token count (Anthropic API): {input_tokens}");
    Some(Json(json!({ "input_tokens": input_tokens })).into_response())
}

/// Mirrors `handleCountTokens`.
#[allow(clippy::result_large_err)]
pub async fn handle_count_tokens(body: Value, headers: HeaderMap) -> Result<Response, AppError> {
    let mut anthropic_payload: AnthropicMessagesPayload = match serde_json::from_value(body) {
        Ok(payload) => payload,
        Err(e) => {
            // An invalid client payload is a 400, not a 500 (AppError::Other
            // renders as INTERNAL_SERVER_ERROR). Matches the provider handler.
            return Ok((
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!("Invalid request payload: {e}"),
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response());
        }
    };

    anthropic_payload.model = resolve_mapped_model(&anthropic_payload.model);

    // normalizeSystemMessages operates on a Value in the Rust port.
    let mut payload_value = serde_json::to_value(&anthropic_payload)?;
    normalize_system_messages(&mut payload_value);
    anthropic_payload = serde_json::from_value(payload_value)?;

    // `<provider>/model` alias -> delegate to the provider count-tokens handler.
    if let Some(alias) = parse_provider_model_alias(&anthropic_payload.model) {
        anthropic_payload.model = alias.model;
        return handle_provider_count_tokens_for_provider(anthropic_payload, alias.provider).await;
    }

    // Try Anthropic's real endpoint first (Claude models only).
    if let Some(response) = count_tokens_via_anthropic(&anthropic_payload).await {
        return Ok(response);
    }

    // Fallback: GPT tokenizer estimation (also used for non-Claude models).
    let anthropic_beta = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let openai_payload = translate_to_openai(&anthropic_payload);

    let requested_model = anthropic_payload.model.clone();
    let resolve = resolve_count_tokens_model(&requested_model);

    let selected_model = resolve.model;
    anthropic_payload.model = selected_model.id.clone();

    if resolve.fallback {
        tracing::warn!("Model '{requested_model}' not found, using o200k_base fallback tokenizer");
    }

    let (mut input, output) = get_token_count(&openai_payload, &selected_model);

    if let Some(tools) = anthropic_payload.tools.as_ref() {
        if !tools.is_empty() {
            let mut add_tool_system_prompt_count = false;
            if anthropic_beta.is_some() {
                let tools_length = tools.len();
                add_tool_system_prompt_count = !tools.iter().any(|tool| {
                    let name = tool.name.as_deref().unwrap_or("");
                    name.starts_with("mcp__") || (name == "Skill" && tools_length == 1)
                });
            }
            if add_tool_system_prompt_count {
                if anthropic_payload.model.starts_with("claude") {
                    // https://docs.anthropic.com/en/docs/agents-and-tools/tool-use/overview#pricing
                    input += 346;
                } else if anthropic_payload.model.starts_with("grok") {
                    input += 120;
                }
            }
        }
    }

    let mut final_token_count = input + output;
    if anthropic_payload.model.starts_with("claude") {
        final_token_count =
            (final_token_count as f64 * get_claude_token_multiplier()).round() as i64;
    }

    tracing::info!("Token count: {final_token_count}");

    Ok(Json(json!({ "input_tokens": final_token_count })).into_response())
}
