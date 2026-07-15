//! Provider-scoped Alpha Search route.

use std::time::Instant;

use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;

use crate::libs::error::{http_error_from_response, openai_error_response, AppError};
use crate::libs::metrics::{record_provider_upstream_request, UpstreamStatus};
use crate::libs::provider_capabilities::{supports, ProviderCapability};
use crate::libs::provider_resolver::resolve_provider_config;
use crate::routes::parse_json_body;
use crate::services::codex::alpha_search::forward_codex_alpha_search;
use crate::services::providers::provider_proxy::{
    create_provider_proxy_response, forward_provider_alpha_search,
};

pub async fn post_provider_alpha_search(
    Path(provider): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let value = match parse_json_body(&body) {
        Ok(value) if value.is_object() => value,
        Ok(_) => {
            return openai_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                Some("invalid_json"),
                "request: must be a JSON object",
            )
        }
        Err(error) => return error.into_openai_response(),
    };
    let body = match serde_json::to_vec(&value) {
        Ok(body) => Bytes::from(body),
        Err(error) => return AppError::Other(error.into()).into_openai_response(),
    };

    let config = match resolve_provider_config(&provider).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            return openai_error_response(
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                Some("provider_not_found"),
                format!("Provider '{provider}' not found or disabled"),
            )
        }
        Err(error) => return AppError::Other(error).into_openai_response(),
    };
    if !supports(&config, ProviderCapability::AlphaSearch) {
        return openai_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("unsupported_provider_capability"),
            format!("Provider '{provider}' does not support Alpha Search"),
        );
    }

    let started = Instant::now();
    let upstream = if config.name == "codex" {
        forward_codex_alpha_search(body, &headers, &config.base_url, uri.query()).await
    } else {
        forward_provider_alpha_search(&config, body, &headers, uri.query()).await
    };
    let upstream = match upstream {
        Ok(response) => response,
        Err(error) => {
            record_provider_upstream_request(
                "alpha_search",
                UpstreamStatus::TransportError,
                started.elapsed().as_secs_f64(),
            );
            return AppError::Http(error).into_openai_response();
        }
    };
    record_provider_upstream_request(
        "alpha_search",
        UpstreamStatus::from_code(upstream.status().as_u16()),
        started.elapsed().as_secs_f64(),
    );
    if !upstream.status().is_success() {
        return AppError::Http(
            http_error_from_response(
                format!("Failed to create {provider} alpha search"),
                upstream,
            )
            .await,
        )
        .into_openai_response();
    }
    create_provider_proxy_response(upstream)
}
