//! Port of routes/provider/models/route.ts (`providerModelRoutes` GET handler).
//!
//! Resolves the provider config and returns its `/v1/models` list: the hardcoded
//! Codex catalog for the `codex` provider, otherwise a pass-through proxy of the
//! upstream `/v1/models` response.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::libs::error::{openai_error_response, AppError};
use crate::libs::provider_resolver::resolve_provider_config;
use crate::services::codex::get_models::get_codex_models;
use crate::services::providers::provider_proxy::{
    create_provider_proxy_response, forward_provider_models,
};

/// Thin axum entrypoint: extracts the `:provider` path param, then delegates to
/// [`handle_provider_models`].
pub async fn get_provider_models(
    axum::extract::Path(provider): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    match handle_provider_models(headers, provider).await {
        Ok(r) => r,
        Err(e) => e.into_openai_response(),
    }
}

/// Mirrors the `providerModelRoutes.get("/")` handler.
pub async fn handle_provider_models(
    headers: HeaderMap,
    provider: String,
) -> Result<Response, AppError> {
    let Some(provider_config) = resolve_provider_config(&provider).await else {
        return Ok(openai_error_response(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            Some("provider_not_found"),
            format!("Provider '{provider}' not found or disabled"),
        ));
    };

    if provider_config.name == "codex" {
        let models = get_codex_models();
        return Ok(Json(json!({
            "object": "list",
            "data": models.data,
            "has_more": false,
        }))
        .into_response());
    }

    let upstream_response = forward_provider_models(&provider_config, &headers).await?;

    tracing::debug!(
        "provider.models.response: provider={provider} status={}",
        upstream_response.status()
    );

    Ok(create_provider_proxy_response(upstream_response))
}
