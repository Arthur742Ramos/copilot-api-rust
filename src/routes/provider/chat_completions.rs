use axum::response::Response;

use crate::libs::error::AppError;
use crate::services::copilot::create_chat_completions::ChatCompletionsPayload;

/// Provider-routed chat completions (`<provider>/model` aliases) are part of the
/// third-party provider proxy layer, which is deferred. Until it lands this
/// returns a 400 mirroring the TS "does not support" error shape.
pub async fn handle_provider_chat_completions(
    _payload: ChatCompletionsPayload,
    provider: String,
) -> Result<Response, AppError> {
    Err(AppError::Other(anyhow::anyhow!(
        "Provider '{provider}' routing is not yet implemented in this build"
    )))
}
