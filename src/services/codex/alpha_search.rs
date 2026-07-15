//! Codex Alpha Search transport.

use axum::http::HeaderMap;
use bytes::Bytes;

use crate::libs::error::HttpError;
use crate::libs::http::client;
use crate::libs::state;
use crate::services::codex::create_responses::{build_codex_responses_headers, CODEX_API_BASE_URL};

async fn send_alpha_search(
    upstream_client: &reqwest::Client,
    url: &str,
    body: Bytes,
    request_headers: &HeaderMap,
    token: &str,
) -> Result<reqwest::Response, HttpError> {
    let mut headers = build_codex_responses_headers(request_headers, Some(false), token)?;
    // Alpha Search uses the shared Codex auth/request headers, not the
    // Responses experimental beta.
    headers.remove("openai-beta");
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    upstream_client
        .post(url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|error| {
            HttpError::internal(format!("Failed to create codex alpha search: {error}"))
        })
}

pub fn resolve_codex_alpha_search_url(base_url: &str, query: Option<&str>) -> String {
    let normalized = base_url.trim().trim_end_matches('/');
    let base = if normalized.is_empty() {
        CODEX_API_BASE_URL
    } else {
        normalized
    };
    let raw = if base.ends_with("/codex/alpha/search") {
        base.to_string()
    } else if base.ends_with("/codex") {
        format!("{base}/alpha/search")
    } else {
        format!("{base}/codex/alpha/search")
    };
    match url::Url::parse(&raw) {
        Ok(mut url) => {
            url.set_query(query.filter(|value| !value.is_empty()));
            url.to_string()
        }
        Err(_) => raw,
    }
}

pub async fn forward_codex_alpha_search(
    body: Bytes,
    request_headers: &HeaderMap,
    base_url: &str,
    query: Option<&str>,
) -> Result<reqwest::Response, HttpError> {
    let url = resolve_codex_alpha_search_url(base_url, query);
    let custom_base_url =
        !base_url.trim().is_empty() && base_url.trim().trim_end_matches('/') != CODEX_API_BASE_URL;
    if custom_base_url {
        crate::services::providers::provider_proxy::validate_upstream_url(&url)?;
    }
    let upstream_client = if custom_base_url {
        crate::services::providers::provider_proxy::restricted_upstream_client()
    } else {
        client()
    };

    let stale = state::with_state(|state| state.codex_access_token.clone()).unwrap_or_default();
    let response =
        send_alpha_search(upstream_client, &url, body.clone(), request_headers, &stale).await?;
    if response.status().as_u16() != 401 {
        return Ok(response);
    }

    // The 401 is observed before consuming any body or exposing progress, so one
    // credential refresh and replay is safe.
    if crate::libs::token::force_refresh_codex_token(&stale)
        .await
        .is_err()
    {
        return Ok(response);
    }
    metrics::counter!(
        "copilot_token_401_replay_total",
        "endpoint" => "codex_alpha_search"
    )
    .increment(1);
    let fresh = state::with_state(|state| state.codex_access_token.clone()).unwrap_or_default();
    send_alpha_search(upstream_client, &url, body, request_headers, &fresh).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_default_custom_and_query_urls() {
        assert_eq!(
            resolve_codex_alpha_search_url("", None),
            "https://chatgpt.com/backend-api/codex/alpha/search"
        );
        assert_eq!(
            resolve_codex_alpha_search_url("https://example.com/codex", Some("q=rust&n=2")),
            "https://example.com/codex/alpha/search?q=rust&n=2"
        );
        assert_eq!(
            resolve_codex_alpha_search_url(
                "https://example.com/codex/alpha/search",
                Some("q=a%20b")
            ),
            "https://example.com/codex/alpha/search?q=a%20b"
        );
    }
}
