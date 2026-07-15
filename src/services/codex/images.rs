//! Native Codex Images API forwarding.
//!
//! Unlike the MCP image tool, the public HTTP Images API can proxy Codex's
//! native `/codex/images/*` contract directly. This preserves fields the proxy
//! does not know about, supports multipart edits, and returns the upstream
//! status, headers, and body without translating through Responses SSE.

use axum::http::HeaderMap;
use bytes::Bytes;

use crate::libs::error::HttpError;
use crate::libs::state;
use crate::services::codex::create_responses::{build_codex_responses_headers, CODEX_API_BASE_URL};
use crate::services::providers::provider_proxy::{
    restricted_images_upstream_client, validate_upstream_url, ImagesOperation, IMAGES_TIMEOUT,
};

#[allow(clippy::result_large_err)]
pub fn resolve_codex_images_url(
    base_url: &str,
    operation: ImagesOperation,
    query: Option<&str>,
) -> Result<String, HttpError> {
    let normalized = if base_url.trim().is_empty() {
        CODEX_API_BASE_URL
    } else {
        base_url.trim().trim_end_matches('/')
    };

    let root = if normalized.ends_with("/codex/images/generations")
        || normalized.ends_with("/codex/images/edits")
    {
        normalized.rsplit_once('/').map(|(root, _)| root).unwrap()
    } else if normalized.ends_with("/codex/images") {
        normalized
    } else if normalized.ends_with("/codex") {
        return finalize_url(
            &format!("{normalized}/images/{}", operation.as_str()),
            query,
        );
    } else {
        return finalize_url(
            &format!("{normalized}/codex/images/{}", operation.as_str()),
            query,
        );
    };

    finalize_url(&format!("{root}/{}", operation.as_str()), query)
}

#[allow(clippy::result_large_err)]
fn finalize_url(raw: &str, query: Option<&str>) -> Result<String, HttpError> {
    let mut url = url::Url::parse(raw)
        .map_err(|error| HttpError::internal(format!("Invalid Codex images URL: {error}")))?;
    url.set_query(query.filter(|value| !value.is_empty()));
    Ok(url.to_string())
}

#[allow(clippy::result_large_err)]
fn build_codex_images_headers(
    request_headers: &HeaderMap,
    operation: ImagesOperation,
    access_token: &str,
) -> Result<reqwest::header::HeaderMap, HttpError> {
    let mut headers = build_codex_responses_headers(request_headers, Some(false), access_token)?;
    if operation == ImagesOperation::Edits && !request_headers.contains_key("content-type") {
        headers.remove(reqwest::header::CONTENT_TYPE);
    }
    Ok(headers)
}

pub async fn forward_codex_images(
    body: Bytes,
    request_headers: &HeaderMap,
    base_url: &str,
    access_token: &str,
    operation: ImagesOperation,
    query: Option<&str>,
) -> Result<reqwest::Response, HttpError> {
    let url = resolve_codex_images_url(base_url, operation, query)?;
    validate_upstream_url(&url)?;

    let send = |token: &str| {
        let headers = build_codex_images_headers(request_headers, operation, token);
        let body = body.clone();
        let url = url.clone();
        async move {
            let headers = headers?;
            restricted_images_upstream_client()
                .post(url)
                .headers(headers)
                .body(body)
                .timeout(IMAGES_TIMEOUT)
                .send()
                .await
                .map_err(|error| {
                    HttpError::internal(format!(
                        "Failed to forward Codex image {}: {error}",
                        operation.as_str()
                    ))
                })
        }
    };

    let stale = access_token.to_string();
    let response = send(&stale).await?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(response);
    }

    tracing::warn!("Codex Images upstream 401; refreshing credentials and replaying once");
    if crate::libs::token::force_refresh_codex_token(&stale)
        .await
        .is_err()
    {
        return Ok(response);
    }

    metrics::counter!(
        "copilot_token_401_replay_total",
        "endpoint" => crate::libs::http::retry_endpoint::CODEX
    )
    .increment(1);
    let fresh = state::with_state(|current| current.codex_access_token.clone()).unwrap_or_default();
    send(&fresh).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_default_url_and_preserves_query() {
        assert_eq!(
            resolve_codex_images_url(
                "",
                ImagesOperation::Generations,
                Some("client=codex&format=png")
            )
            .unwrap(),
            "https://chatgpt.com/backend-api/codex/images/generations?client=codex&format=png"
        );
    }

    #[test]
    fn resolves_custom_url_variants() {
        assert_eq!(
            resolve_codex_images_url(
                "https://example.com/backend-api/codex",
                ImagesOperation::Edits,
                None
            )
            .unwrap(),
            "https://example.com/backend-api/codex/images/edits"
        );
        assert_eq!(
            resolve_codex_images_url(
                "https://example.com/backend-api/codex/images/generations",
                ImagesOperation::Edits,
                None
            )
            .unwrap(),
            "https://example.com/backend-api/codex/images/edits"
        );
    }

    #[test]
    #[serial_test::serial]
    fn edit_headers_preserve_multipart_and_do_not_invent_content_type() {
        let previous = state::with_state(|current| current.codex_account_id.clone());
        state::with_state_mut(|current| {
            current.codex_account_id = Some("account-123".to_string());
        });

        let mut request = HeaderMap::new();
        request.insert(
            "content-type",
            "multipart/form-data; boundary=image-boundary"
                .parse()
                .unwrap(),
        );
        let multipart =
            build_codex_images_headers(&request, ImagesOperation::Edits, "token").unwrap();
        assert_eq!(
            multipart["content-type"],
            "multipart/form-data; boundary=image-boundary"
        );

        let without_content_type =
            build_codex_images_headers(&HeaderMap::new(), ImagesOperation::Edits, "token").unwrap();
        assert!(without_content_type.get("content-type").is_none());

        let generation =
            build_codex_images_headers(&HeaderMap::new(), ImagesOperation::Generations, "token")
                .unwrap();
        assert_eq!(generation["content-type"], "application/json");

        state::with_state_mut(|current| {
            current.codex_account_id = previous;
        });
    }
}
