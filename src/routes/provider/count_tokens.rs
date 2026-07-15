//! Port of `src/routes/provider/messages/count-tokens-handler.ts`.
//!
//! Estimation-only token counting for provider-routed models. Builds a fallback
//! `Model` (the provider models are not in the Copilot `/models` list) and runs
//! the GPT tokenizer estimation over the translated OpenAI payload.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::libs::error::{anthropic_error_response, AppError};
use crate::libs::provider_model::create_fallback_model;
use crate::libs::provider_resolver::resolve_provider_config;
use crate::libs::tokenizer::get_token_count;
use crate::routes::messages::anthropic_types::AnthropicMessagesPayload;
use crate::routes::messages::non_stream_translation::{
    translate_to_openai_with_options, TranslateToOpenAiOptions,
};
use crate::routes::messages::preprocess::normalize_system_messages;
use crate::routes::messages::request_validation::{
    validate_messages_request_shape, validate_required_model,
};

/// Mirrors `handleProviderCountTokensForProvider`.
#[allow(clippy::result_large_err)]
pub async fn handle_provider_count_tokens_for_provider(
    mut payload: AnthropicMessagesPayload,
    provider: String,
) -> Result<Response, AppError> {
    // normalizeSystemMessages operates on a Value in the Rust port.
    let mut payload_value = serde_json::to_value(&payload)?;
    normalize_system_messages(&mut payload_value);
    payload = serde_json::from_value(payload_value)?;

    let model_id = payload.model.trim().to_string();

    let Some(provider_config) = resolve_provider_config(&provider).await else {
        return Ok(anthropic_error_response(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            format!("Provider '{provider}' not found or disabled"),
        ));
    };

    // Mirror the TS: only the openai-compatible / openai-responses providers
    // thread the translation options from `modelConfig`; the anthropic
    // passthrough uses the defaults.
    let model_config = provider_config
        .models
        .as_ref()
        .and_then(|m| m.get(&model_id))
        .cloned();
    let translation_options = if matches!(
        provider_config.provider_type.as_str(),
        "openai-compatible" | "openai-responses"
    ) {
        TranslateToOpenAiOptions {
            support_pdf: model_config
                .as_ref()
                .and_then(|m| m.support_pdf)
                .unwrap_or(false),
            tool_content_support_type: model_config
                .as_ref()
                .and_then(|m| m.tool_content_support_type.clone())
                .unwrap_or_default(),
        }
    } else {
        TranslateToOpenAiOptions::default()
    };

    let openai_payload = translate_to_openai_with_options(&payload, &translation_options)?;

    let selected_model = create_fallback_model(&model_id);

    // tiktoken BPE is CPU-bound; offload it so it does not stall the Tokio worker
    // thread (and any in-flight streams sharing it). Mirrors count_tokens_handler.
    let (input, output) =
        tokio::task::spawn_blocking(move || get_token_count(&openai_payload, &selected_model))
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("count_tokens task failed: {e}")))?;
    let final_token_count = input + output;

    tracing::debug!(
        provider = %provider,
        model = %payload.model,
        input_tokens = final_token_count,
        "provider.count_tokens.success"
    );

    Ok(Json(json!({ "input_tokens": final_token_count })).into_response())
}

/// Thin axum entrypoint: extracts the `:provider` path param and Anthropic body,
/// then delegates to [`handle_provider_count_tokens_for_provider`]. Mirrors
/// `handleProviderCountTokens`.
pub async fn post_provider_count_tokens(
    axum::extract::Path(provider): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let mut value = match crate::routes::parse_json_body(&body) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = validate_messages_request_shape(&value) {
        return error.into_response();
    }
    if let Err(error) = crate::routes::files::materialize_anthropic_file_sources(&mut value).await {
        return error.into_response();
    }
    if let Err(error) = validate_messages_request_shape(&value) {
        return error.into_response();
    }
    if let Err(error) = validate_required_model(&value) {
        return error.into_response();
    }
    let payload: AnthropicMessagesPayload = match serde_json::from_value(value) {
        Ok(p) => p,
        Err(e) => {
            return AppError::BadRequest(format!("Invalid request payload: {e}")).into_response()
        }
    };
    match handle_provider_count_tokens_for_provider(payload, provider).await {
        Ok(r) => r,
        Err(e) => AppError::into_response(e),
    }
}
