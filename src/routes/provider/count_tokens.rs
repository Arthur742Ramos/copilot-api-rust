//! Port of `src/routes/provider/messages/count-tokens-handler.ts`.
//!
//! Estimation-only token counting for provider-routed models. Builds a fallback
//! `Model` (the provider models are not in the Copilot `/models` list) and runs
//! the GPT tokenizer estimation over the translated OpenAI payload.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::libs::error::AppError;
use crate::libs::provider_model::create_fallback_model;
use crate::libs::provider_resolver::resolve_provider_config;
use crate::libs::tokenizer::get_token_count;
use crate::routes::messages::anthropic_types::AnthropicMessagesPayload;
use crate::routes::messages::non_stream_translation::translate_to_openai;
use crate::routes::messages::preprocess::normalize_system_messages;

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

    let Some(_provider_config) = resolve_provider_config(&provider).await else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "message": format!("Provider '{provider}' not found or disabled"),
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response());
    };

    // The Rust `translate_to_openai` does not yet take translation options
    // (supportPdf / toolContentSupportType); the established provider message
    // flow calls it without options, so we match that here.
    let openai_payload = translate_to_openai(&payload);

    let selected_model = create_fallback_model(&model_id);

    let (input, output) = get_token_count(&openai_payload, &selected_model);
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
    body: Json<serde_json::Value>,
) -> Response {
    let payload: AnthropicMessagesPayload = match serde_json::from_value(body.0) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!("Invalid request payload: {e}"),
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response()
        }
    };
    match handle_provider_count_tokens_for_provider(payload, provider).await {
        Ok(r) => r,
        Err(e) => AppError::into_response(e),
    }
}
