//! Port of services/providers/provider-proxy.ts: forwards Anthropic /
//! OpenAI-compatible requests to a configured upstream provider and proxies the
//! response back to the client unchanged.

use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::libs::config::ResolvedProviderConfig;
use crate::libs::error::HttpError;
use crate::libs::http::client;
use crate::routes::messages::anthropic_types::AnthropicMessagesPayload;
use crate::services::copilot::create_chat_completions::ChatCompletionsPayload;
use crate::services::copilot::create_responses::ResponsesPayload;

/// Request headers copied through to the upstream for every provider type.
const SHARED_FORWARDABLE_HEADERS: [&str; 2] = ["accept", "user-agent"];

/// Additional request headers copied through only for `anthropic` providers.
const ANTHROPIC_FORWARDABLE_HEADERS: [&str; 2] = ["anthropic-version", "anthropic-beta"];

/// Hop-by-hop / encoding headers stripped from the upstream response before it
/// is forwarded to the client.
const STRIPPED_RESPONSE_HEADERS: [&str; 10] = [
    "connection",
    "content-encoding",
    "content-length",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Build the headers sent to the upstream provider. Mirrors
/// `buildProviderUpstreamHeaders`.
pub fn build_provider_upstream_headers(
    cfg: &ResolvedProviderConfig,
    request_headers: &HeaderMap,
) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderName, HeaderValue};

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept", HeaderValue::from_static("application/json"));

    if cfg.auth_type == "x-api-key" {
        if let Ok(v) = HeaderValue::from_str(&cfg.api_key) {
            headers.insert("x-api-key", v);
        }
    } else if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", cfg.api_key)) {
        headers.insert("authorization", v);
    }

    let mut copy_header = |name: &str| {
        if let Some(value) = request_headers.get(name) {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                headers.insert(n, v);
            }
        }
    };

    for name in SHARED_FORWARDABLE_HEADERS {
        copy_header(name);
    }

    if cfg.provider_type != "anthropic" {
        return headers;
    }

    for name in ANTHROPIC_FORWARDABLE_HEADERS {
        copy_header(name);
    }

    headers
}

/// Pass-through proxy response: copy status and headers (minus the stripped
/// set) and stream the upstream body to the client. Mirrors
/// `createProviderProxyResponse`.
pub fn create_provider_proxy_response(upstream: reqwest::Response) -> Response {
    use axum::http::{HeaderName, HeaderValue, StatusCode};

    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if STRIPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            // `append` (not `insert`) so multi-value headers such as
            // `set-cookie` are forwarded in full rather than collapsed to one.
            headers.append(n, v);
        }
    }

    let body = Body::from_stream(upstream.bytes_stream());

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// POST {base_url}/v1/messages. Returns the raw upstream response; the caller
/// inspects the status. Mirrors `forwardProviderMessages`.
pub async fn forward_provider_messages(
    cfg: &ResolvedProviderConfig,
    payload: &AnthropicMessagesPayload,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, HttpError> {
    tracing::info!("<-- model: {}", payload.model);
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    client()
        .post(format!("{}/v1/messages", cfg.base_url))
        .headers(build_provider_upstream_headers(cfg, request_headers))
        .body(body)
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to forward provider messages: {e}")))
}

/// POST {base_url}/v1/chat/completions. Mirrors
/// `forwardProviderChatCompletions`.
pub async fn forward_provider_chat_completions(
    cfg: &ResolvedProviderConfig,
    payload: &ChatCompletionsPayload,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, HttpError> {
    tracing::info!("<-- model: {}", payload.model);
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    client()
        .post(format!("{}/v1/chat/completions", cfg.base_url))
        .headers(build_provider_upstream_headers(cfg, request_headers))
        .body(body)
        .send()
        .await
        .map_err(|e| {
            HttpError::internal(format!("Failed to forward provider chat completions: {e}"))
        })
}

/// POST {base_url}/v1/responses. Mirrors `forwardProviderResponses`.
pub async fn forward_provider_responses(
    cfg: &ResolvedProviderConfig,
    payload: &ResponsesPayload,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, HttpError> {
    tracing::info!("<-- model: {}", payload.model);
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    client()
        .post(format!("{}/v1/responses", cfg.base_url))
        .headers(build_provider_upstream_headers(cfg, request_headers))
        .body(body)
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to forward provider responses: {e}")))
}

/// GET {base_url}/v1/models (no body, no model log). Mirrors
/// `forwardProviderModels`.
pub async fn forward_provider_models(
    cfg: &ResolvedProviderConfig,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, HttpError> {
    client()
        .get(format!("{}/v1/models", cfg.base_url))
        .headers(build_provider_upstream_headers(cfg, request_headers))
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to forward provider models: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg(provider_type: &str, auth_type: &str, api_key: &str) -> ResolvedProviderConfig {
        ResolvedProviderConfig {
            name: "test".to_string(),
            provider_type: provider_type.to_string(),
            base_url: "https://example.com".to_string(),
            api_key: api_key.to_string(),
            auth_type: auth_type.to_string(),
            models: Some(BTreeMap::new()),
            adjust_input_tokens: None,
        }
    }

    #[test]
    fn x_api_key_auth() {
        let headers = build_provider_upstream_headers(
            &cfg("anthropic", "x-api-key", "secret"),
            &HeaderMap::new(),
        );
        assert_eq!(headers.get("x-api-key").unwrap(), "secret");
        assert!(headers.get("authorization").is_none());
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("accept").unwrap(), "application/json");
    }

    #[test]
    fn bearer_auth() {
        let headers = build_provider_upstream_headers(
            &cfg("openai-compatible", "authorization", "secret"),
            &HeaderMap::new(),
        );
        assert_eq!(headers.get("authorization").unwrap(), "Bearer secret");
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn shared_headers_forwarded_and_accept_overwritten() {
        let mut req = HeaderMap::new();
        req.insert("accept", "text/event-stream".parse().unwrap());
        req.insert("user-agent", "agent/1.0".parse().unwrap());
        let headers =
            build_provider_upstream_headers(&cfg("openai-compatible", "authorization", "k"), &req);
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
        assert_eq!(headers.get("user-agent").unwrap(), "agent/1.0");
    }

    #[test]
    fn anthropic_headers_only_for_anthropic() {
        let mut req = HeaderMap::new();
        req.insert("anthropic-version", "2023-06-01".parse().unwrap());
        req.insert("anthropic-beta", "beta-feature".parse().unwrap());

        let anthropic = build_provider_upstream_headers(&cfg("anthropic", "x-api-key", "k"), &req);
        assert_eq!(anthropic.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(anthropic.get("anthropic-beta").unwrap(), "beta-feature");

        let openai =
            build_provider_upstream_headers(&cfg("openai-compatible", "authorization", "k"), &req);
        assert!(openai.get("anthropic-version").is_none());
        assert!(openai.get("anthropic-beta").is_none());
    }
}
