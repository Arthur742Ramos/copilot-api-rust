//! Codex `/responses` HTTP and pooled upstream WebSocket transports.
//!
//! Ported from services/codex/create-responses.ts. Streaming requests select the
//! upstream WebSocket when enabled, with handshake-only fallback to HTTP. Unary
//! and compaction requests remain HTTP.
//!
//! Conventions match the rest of the crate (see create_chat_completions.rs):
//! services return `Result<_, HttpError>`; headers are built into a
//! `reqwest::header::HeaderMap` and handed to `client().post(...).headers(...)`.

use futures_util::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::libs::error::HttpError;
use crate::libs::http::{client, serialize_json_body};
use crate::libs::request_context::request_context_store;
use crate::libs::state;
use crate::services::copilot::create_responses::{
    InputField, MessageContent, ResponseInputContent, ResponseInputItem, ResponsesEventStream,
    ResponsesPayload,
};

/// Mirrors the TS `CODEX_API_BASE_URL`.
pub const CODEX_API_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// Mirrors `STRIPPED_CODEX_REQUEST_HEADERS`. All entries are lowercase; lookups
/// lowercase the incoming header name first.
const STRIPPED_CODEX_REQUEST_HEADERS: &[&str] = &[
    "accept-encoding",
    "authorization",
    "cdn-loop",
    "connection",
    "content-length",
    // `cookie` is not in the TS strip-set, but forwarding browser cookies to a
    // third-party upstream is an unnecessary data-leak risk, so we drop it.
    "cookie",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "true-client-ip",
    "upgrade",
    "x-api-key",
    "x-forwarded-for",
    "x-forwarded-proto",
];

/// Mirrors the TS `requireCodexAuthContext`'s account-id half: pulls the Codex
/// account id from global state, erroring (TS throws) when it is missing. The
/// access token is threaded in explicitly by the caller (see
/// [`build_codex_responses_headers`]) so the 401-replay path can stamp the exact
/// token its refresh decision is made against, rather than re-snapshotting state.
// HttpError is the crate-wide service error; the small Ok payload here makes
// clippy flag the large Err, but boxing would diverge from every other service.
#[allow(clippy::result_large_err)]
fn require_codex_account_id() -> Result<String, HttpError> {
    state::snapshot()
        .codex_account_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HttpError::internal("Codex account id is not loaded"))
}

/// Mirrors the TS `resolveCodexResponsesUrl`.
pub fn resolve_codex_responses_url(base_url: &str) -> String {
    let normalized = base_url.trim().trim_end_matches('/');
    if normalized.is_empty() {
        return format!("{CODEX_API_BASE_URL}/codex/responses");
    }
    if normalized.ends_with("/codex/responses") {
        return normalized.to_string();
    }
    if normalized.ends_with("/codex") {
        return format!("{normalized}/responses");
    }
    format!("{normalized}/codex/responses")
}

pub fn resolve_codex_compact_url(base_url: &str) -> String {
    let normalized = base_url.trim().trim_end_matches('/');
    if normalized.ends_with("/codex/responses/compact") {
        return normalized.to_string();
    }
    format!("{}/compact", resolve_codex_responses_url(normalized))
}

pub enum CodexResponsesReturn {
    Http(reqwest::Response),
    Stream(ResponsesEventStream),
}

struct CodexWebsocketOutcome {
    finished: bool,
}

impl Drop for CodexWebsocketOutcome {
    fn drop(&mut self) {
        if !self.finished {
            metrics::counter!(
                "copilot_responses_websocket_cancel_total",
                "provider" => "codex"
            )
            .increment(1);
        }
    }
}

fn private_provider_override_enabled() -> bool {
    std::env::var("COPILOT_API_ALLOW_PRIVATE_PROVIDERS")
        .map(|value| value == "1")
        .unwrap_or(false)
}

fn websocket_allowed_for_base_url(base_url: &str, allow_unpinned_custom_url: bool) -> bool {
    let normalized = base_url.trim().trim_end_matches('/');
    normalized.is_empty() || normalized == CODEX_API_BASE_URL || allow_unpinned_custom_url
}

fn use_codex_websocket() -> bool {
    crate::libs::config::is_responses_api_web_socket_enabled()
        && !crate::libs::http::proxy_from_env_enabled()
}

fn websocket_chunk(data: String) -> crate::libs::sse::SseEvent {
    let parsed = serde_json::from_str::<Value>(&data).ok();
    let event = parsed
        .as_ref()
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let id = parsed
        .as_ref()
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    crate::libs::sse::SseEvent { id, event, data }
}

fn websocket_terminal(chunk: &crate::libs::sse::SseEvent) -> bool {
    serde_json::from_str::<Value>(&chunk.data)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|event_type| {
            matches!(
                event_type.as_str(),
                "response.completed" | "response.failed" | "response.incomplete" | "error"
            )
        })
}

fn codex_websocket_pool_key(
    url: &str,
    model: &str,
    token: &str,
    account_id: &str,
    headers: &[(String, String)],
) -> String {
    let mut stable_headers = headers
        .iter()
        .filter(|(name, _)| !name.to_ascii_lowercase().contains("trace"))
        .cloned()
        .collect::<Vec<_>>();
    stable_headers.sort();
    let header_bytes = serde_json::to_vec(&stable_headers).unwrap_or_default();

    let mut digest = Sha256::new();
    digest.update(b"codex-responses-websocket-v1\0");
    digest.update(url.as_bytes());
    digest.update(b"\0");
    digest.update(model.as_bytes());
    digest.update(b"\0");
    digest.update(token.as_bytes());
    digest.update(b"\0");
    digest.update(account_id.as_bytes());
    digest.update(b"\0");
    digest.update(header_bytes);
    format!("codex:{}", hex::encode(digest.finalize()))
}

#[allow(clippy::result_large_err)]
pub async fn forward_codex_responses_websocket(
    payload: ResponsesPayload,
    request_headers: &axum::http::HeaderMap,
    base_url: &str,
) -> Result<ResponsesEventStream, HttpError> {
    forward_codex_responses_websocket_inner(
        payload,
        request_headers,
        base_url,
        private_provider_override_enabled(),
    )
    .await
}

#[allow(clippy::result_large_err)]
async fn forward_codex_responses_websocket_inner(
    mut payload: ResponsesPayload,
    request_headers: &axum::http::HeaderMap,
    base_url: &str,
    allow_unpinned_custom_url: bool,
) -> Result<ResponsesEventStream, HttpError> {
    use crate::services::responses_websocket::{
        create_pooled_web_socket_stream, create_web_socket_url, PooledWebSocketRequest,
        PooledWebSocketStreamOptions,
    };

    if !websocket_allowed_for_base_url(base_url, allow_unpinned_custom_url) {
        return Err(HttpError::internal(
            "Custom Codex base URLs use HTTP because WebSocket DNS pinning is unavailable",
        ));
    }

    normalize_codex_responses_payload(&mut payload);
    let access_token =
        state::with_state(|state| state.codex_access_token.clone()).unwrap_or_default();
    let account_id = state::with_state(|state| state.codex_account_id.clone()).unwrap_or_default();
    let mut ws_headers =
        build_codex_responses_headers(request_headers, Some(false), &access_token)?;
    ws_headers.insert(
        reqwest::header::HeaderName::from_static("openai-beta"),
        reqwest::header::HeaderValue::from_static("responses_websockets=2026-02-06"),
    );
    ws_headers.remove(reqwest::header::ACCEPT);
    ws_headers.remove(reqwest::header::CONTENT_TYPE);
    let headers = ws_headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect::<Vec<_>>();

    let http_url = resolve_codex_responses_url(base_url);
    if !base_url.trim().is_empty() && !allow_unpinned_custom_url {
        crate::services::providers::provider_proxy::validate_upstream_url(&http_url)?;
    }
    let url = create_web_socket_url(&http_url);
    let pool_key =
        codex_websocket_pool_key(&url, &payload.model, &access_token, &account_id, &headers);

    let mut websocket_payload =
        serde_json::to_value(&payload).map_err(|error| HttpError::internal(error.to_string()))?;
    if let Some(object) = websocket_payload.as_object_mut() {
        object.insert(
            "type".to_string(),
            Value::String("response.create".to_string()),
        );
        object.remove("stream");
    }

    let source = create_pooled_web_socket_stream(
        PooledWebSocketRequest {
            headers,
            payload: websocket_payload,
            pool_key,
            url,
        },
        PooledWebSocketStreamOptions {
            create_chunk: websocket_chunk,
            idle_timeout_ms: None,
            connect_timeout: crate::libs::http::UPSTREAM_CONNECT_TIMEOUT,
            read_timeout: crate::libs::http::upstream_read_timeout(),
            is_terminal_chunk: websocket_terminal,
            open_error_message: "Failed to create codex responses websocket".to_string(),
            stream_error_message: "Codex responses websocket stream error".to_string(),
            terminal_chunk_missing_message:
                "Codex responses websocket ended without a terminal response".to_string(),
            unavailable_error_message: None,
        },
    )
    .await
    .map_err(|error| HttpError::internal(error.to_string()))?;

    let stream = async_stream::stream! {
        let mut outcome_guard = CodexWebsocketOutcome { finished: false };
        futures_util::pin_mut!(source);
        let mut terminal_recorded = false;
        while let Some(item) = source.next().await {
            match &item {
                Ok(chunk) if websocket_terminal(chunk) && !terminal_recorded => {
                    terminal_recorded = true;
                    let terminal = chunk.event.as_deref().unwrap_or("unknown");
                    let outcome = match terminal {
                        "response.completed" => "completed",
                        "response.failed" => "failed",
                        "response.incomplete" => "incomplete",
                        "error" => "error",
                        _ => "unknown",
                    };
                    metrics::counter!(
                        "copilot_responses_websocket_terminal_total",
                        "provider" => "codex",
                        "outcome" => outcome
                    )
                    .increment(1);
                    outcome_guard.finished = true;
                }
                Err(_) => {
                    metrics::counter!(
                        "copilot_responses_websocket_stream_error_total",
                        "provider" => "codex"
                    )
                    .increment(1);
                    outcome_guard.finished = true;
                }
                _ => {}
            }
            yield item;
        }
    };
    Ok(Box::pin(stream))
}

pub async fn forward_codex_responses_selected(
    payload: ResponsesPayload,
    request_headers: &axum::http::HeaderMap,
    base_url: &str,
) -> Result<CodexResponsesReturn, HttpError> {
    if payload.stream == Some(true) && use_codex_websocket() {
        metrics::counter!(
            "copilot_responses_websocket_attempt_total",
            "provider" => "codex"
        )
        .increment(1);
        match forward_codex_responses_websocket(payload.clone(), request_headers, base_url).await {
            Ok(stream) => return Ok(CodexResponsesReturn::Stream(stream)),
            Err(error) => {
                // `create_pooled_web_socket_stream` returns only after its
                // handshake and before sending response.create. Replaying over
                // HTTP is safe at this boundary and nowhere later.
                metrics::counter!(
                    "copilot_responses_websocket_fallback_total",
                    "provider" => "codex"
                )
                .increment(1);
                tracing::warn!(
                    error = %error,
                    "Codex Responses websocket unavailable before request send; falling back to HTTP"
                );
            }
        }
    }
    forward_codex_responses(payload, request_headers, base_url)
        .await
        .map(CodexResponsesReturn::Http)
}

fn set_req_header(map: &mut reqwest::header::HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (
        reqwest::header::HeaderName::from_bytes(name.as_bytes()),
        reqwest::header::HeaderValue::from_str(value),
    ) {
        map.insert(n, v);
    }
}

/// Mirrors the TS `buildCodexResponsesHeaders`. Copies through the inbound
/// request headers minus the strip-set / `*trace*` / `cf-*` rules, then layers
/// the Codex auth + defaults and the opencode originator override.
///
/// `access_token` is threaded in by the caller (rather than re-read from state)
/// so the 401-replay path stamps the exact token its refresh decision is made
/// against; an empty token is rejected, matching the old state-loaded check.
///
/// NOTE: the TS port takes no provider config — Codex auth comes from global
/// state (oauth2), so this intentionally omits the `ResolvedProviderConfig`
/// parameter the orchestrator sketched.
#[allow(clippy::result_large_err)]
pub fn build_codex_responses_headers(
    request_headers: &axum::http::HeaderMap,
    stream: Option<bool>,
    access_token: &str,
) -> Result<reqwest::header::HeaderMap, HttpError> {
    if access_token.is_empty() {
        return Err(HttpError::internal("Codex access token is not loaded"));
    }
    let account_id = require_codex_account_id()?;
    let mut headers = reqwest::header::HeaderMap::new();

    for (name, value) in request_headers.iter() {
        let name_lower = name.as_str().to_ascii_lowercase();
        if STRIPPED_CODEX_REQUEST_HEADERS.contains(&name_lower.as_str()) {
            continue;
        }
        if name_lower.contains("trace") {
            continue;
        }
        if name_lower.starts_with("cf-") {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.insert(n, v);
        }
    }

    if !headers.contains_key("accept") {
        set_req_header(
            &mut headers,
            "accept",
            if stream.unwrap_or(false) {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
    }

    set_req_header(
        &mut headers,
        "authorization",
        &format!("Bearer {access_token}"),
    );
    set_req_header(&mut headers, "chatgpt-account-id", &account_id);
    if !headers.contains_key("content-type") {
        set_req_header(&mut headers, "content-type", "application/json");
    }
    if !headers.contains_key("openai-beta") {
        set_req_header(&mut headers, "OpenAI-Beta", "responses=experimental");
    }
    if !headers.contains_key("originator") {
        set_req_header(&mut headers, "originator", "copilot-api");
    }
    if !headers.contains_key("user-agent") {
        set_req_header(&mut headers, "user-agent", "copilot-api");
    }

    let ua_is_opencode = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.starts_with("opencode"))
        .unwrap_or(false);
    if ua_is_opencode {
        set_req_header(&mut headers, "originator", "opencode");
        if let Some(store) = request_context_store() {
            if let Some(session_id) = store.session_affinity {
                set_req_header(&mut headers, "session-id", &session_id);
            }
        }
    }

    Ok(headers)
}

/// Mirrors the TS `normalizeCodexResponsesPayload`. Forces `store: false`,
/// strips `temperature` / `top_p` / `max_output_tokens` / `metadata`, then folds
/// the first up-to-three leading system messages into `instructions`.
pub fn normalize_codex_responses_payload(payload: &mut ResponsesPayload) {
    payload.store = Some(false);
    payload.temperature = None;
    payload.top_p = None;
    payload.max_output_tokens = None;
    payload.metadata = None;

    // Bail when instructions are already set, or `input` is not an array.
    let has_instructions = payload
        .instructions
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if has_instructions {
        return;
    }
    let items = match payload.input.take() {
        Some(InputField::Items(items)) => items,
        // Not an array (bare-text prompt or absent): restore and return.
        other => {
            payload.input = other;
            return;
        }
    };

    let original_len = items.len();
    let mut instructions: Vec<String> = Vec::new();
    let mut message_count = 0;
    let mut remaining: Vec<ResponseInputItem> = Vec::new();

    for item in items {
        let keep = match &item {
            ResponseInputItem::Message(message) => {
                message_count += 1;
                if message.role != "system" || message_count > 3 {
                    true
                } else {
                    match get_text_content(message.content.as_ref()) {
                        // `undefined` text content -> keep the message untouched.
                        None => true,
                        Some(system_prompt) => {
                            if !system_prompt.trim().is_empty() {
                                instructions.push(system_prompt);
                            }
                            false
                        }
                    }
                }
            }
            _ => true,
        };
        if keep {
            remaining.push(item);
        }
    }

    if remaining.len() == original_len {
        // Nothing folded; restore the input array unchanged.
        payload.input = Some(InputField::Items(remaining));
        return;
    }

    if !instructions.is_empty() {
        // Codex expects system prompts in instructions instead of input messages.
        payload.instructions = Some(instructions.join("\n\n"));
    }

    if !remaining.is_empty() {
        payload.input = Some(InputField::Items(remaining));
    } else {
        payload.input = None;
    }
}

/// Extracts the text content of a message for system-prompt folding. Returns
/// `None` (the TS `undefined`) to signal "leave this message in place" — this
/// includes a missing `content` field, so a content-less system message is kept
/// untouched rather than folded-and-dropped with an empty prompt.
fn get_text_content(content: Option<&MessageContent>) -> Option<String> {
    match content {
        // `typeof content === "string"`.
        Some(MessageContent::Text(s)) => Some(s.clone()),
        // No `content` field: keep the message untouched rather than deleting it.
        None => None,
        Some(MessageContent::Blocks(blocks)) => {
            let mut text_blocks: Vec<String> = Vec::new();
            for block in blocks {
                match get_text_block(block) {
                    None => return None,
                    Some(text) => {
                        if !text.is_empty() {
                            text_blocks.push(text);
                        }
                    }
                }
            }
            Some(text_blocks.join("\n\n"))
        }
    }
}

/// Mirrors the TS `getTextBlock`. Returns `None` for `undefined` (a non-text or
/// malformed block), `Some(text)` for a valid `input_text` / `output_text`.
fn get_text_block(block: &ResponseInputContent) -> Option<String> {
    match block {
        ResponseInputContent::Text(text) => {
            if text.block_type == "input_text" || text.block_type == "output_text" {
                Some(text.text.clone())
            } else {
                None
            }
        }
        // Image / file blocks carry a non-text `type` -> `undefined`.
        ResponseInputContent::Image(_) | ResponseInputContent::File(_) => None,
        ResponseInputContent::Other(value) => {
            if !value.is_object() {
                return None;
            }
            if let Some(type_value) = value.get("type") {
                match type_value.as_str() {
                    Some("input_text") | Some("output_text") => {}
                    _ => return None,
                }
            }
            match value.get("text") {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            }
        }
    }
}

/// Mirrors the HTTP arm of the TS `forwardCodexResponses`: normalizes the
/// payload, builds headers, and POSTs to the Codex responses endpoint. Returns
/// the raw response; the caller checks `status` and forwards/streams the body.
///
pub async fn forward_codex_responses(
    mut payload: ResponsesPayload,
    request_headers: &axum::http::HeaderMap,
    base_url: &str,
) -> Result<reqwest::Response, HttpError> {
    tracing::info!("<-- model: {}", payload.model);

    normalize_codex_responses_payload(&mut payload);

    let url = resolve_codex_responses_url(base_url);
    let custom_base_url = !base_url.trim().is_empty();
    // SSRF: when the operator has configured a custom Codex base URL it is
    // runtime-settable and therefore untrusted; validate it before forwarding.
    // An empty `base_url` falls back to the fixed CODEX_API_BASE_URL (ChatGPT's
    // own endpoint), which we trust and skip.
    if custom_base_url {
        crate::services::providers::provider_proxy::validate_upstream_url(&url)?;
    }
    let stream = payload.stream;
    let body = serialize_json_body(&payload).map_err(|e| {
        HttpError::internal(format!("Failed to serialize codex responses payload: {e}"))
    })?;
    drop(payload);

    send_codex_request(body, request_headers, &url, stream, custom_base_url).await
}

/// Forward the unary Codex `/responses/compact` contract without applying the
/// generation-only payload normalization used by [`forward_codex_responses`].
pub async fn forward_codex_compact(
    payload: &ResponsesPayload,
    request_headers: &axum::http::HeaderMap,
    base_url: &str,
) -> Result<reqwest::Response, HttpError> {
    let url = resolve_codex_compact_url(base_url);
    let custom_base_url = !base_url.trim().is_empty();
    if custom_base_url {
        crate::services::providers::provider_proxy::validate_upstream_url(&url)?;
    }
    let body = serialize_json_body(payload).map_err(|e| {
        HttpError::internal(format!("Failed to serialize codex compact payload: {e}"))
    })?;
    send_codex_request(body, request_headers, &url, Some(false), custom_base_url).await
}

async fn send_codex_request(
    body: bytes::Bytes,
    request_headers: &axum::http::HeaderMap,
    url: &str,
    stream: Option<bool>,
    custom_base_url: bool,
) -> Result<reqwest::Response, HttpError> {
    // Inline 401 recovery: a stale/revoked Codex oauth token self-heals on the
    // request that hit it. Codex auth is a DISTINCT path from the Copilot token
    // (oauth2 access token), so it force-refreshes the Codex credential. The 401
    // is read off the status line before any body streams, so replaying once is
    // safe (no partial, already-billed generation is dropped). The access token is
    // read ONCE here and threaded into the header builder so the failing request
    // provably carries the exact token `force_refresh_codex_token` is told is
    // stale — a background-loop rotation between this read and header construction
    // can no longer make the refresh decision skip and replay a still-bad token.
    let stale = state::with_state(|s| s.codex_access_token.clone()).unwrap_or_default();
    let upstream_client = if custom_base_url {
        // User-controlled targets must neither follow redirects nor connect to
        // private DNS answers; both protections live on the restricted client.
        crate::services::providers::provider_proxy::restricted_upstream_client()
    } else {
        client()
    };
    let send = |headers: reqwest::header::HeaderMap| {
        let request = upstream_client
            .post(url)
            .headers(headers)
            .body(body.clone());
        crate::libs::http::send_with_retry(
            request,
            crate::libs::http::retry_endpoint::CODEX,
            crate::libs::http::RetryPolicy::billable_generation(),
        )
    };

    let headers = build_codex_responses_headers(request_headers, stream, &stale)?;
    let response = send(headers)
        .await
        .map_err(|e| HttpError::internal(format!("Failed to create codex responses: {e}")))?;

    if response.status().as_u16() != 401 {
        return Ok(response);
    }

    tracing::warn!("codex upstream 401; attempting inline credential refresh + single replay");
    // Refresh against the EXACT token the failing request carried.
    if crate::libs::token::force_refresh_codex_token(&stale)
        .await
        .is_err()
    {
        // Refresh failed: surface the original 401 unchanged.
        return Ok(response);
    }

    metrics::counter!("copilot_token_401_replay_total", "endpoint" => crate::libs::http::retry_endpoint::CODEX)
        .increment(1);
    // Replay once with the freshly-installed token re-read from state.
    let fresh = state::with_state(|s| s.codex_access_token.clone()).unwrap_or_default();
    let headers = build_codex_responses_headers(request_headers, stream, &fresh)?;
    let response = send(headers)
        .await
        .map_err(|e| HttpError::internal(format!("Failed to create codex responses: {e}")))?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_responses_url_variants() {
        assert_eq!(
            resolve_codex_responses_url(""),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_responses_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_responses_url("https://example.com/codex/"),
            "https://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_responses_url("https://example.com/codex/responses"),
            "https://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_compact_url("https://example.com"),
            "https://example.com/codex/responses/compact"
        );
        assert_eq!(
            resolve_codex_compact_url("https://example.com/codex/responses/compact"),
            "https://example.com/codex/responses/compact"
        );
    }

    #[test]
    fn normalize_folds_system_messages_into_instructions() {
        let raw = r#"{
            "model": "gpt-5.5",
            "temperature": 0.7,
            "top_p": 0.9,
            "max_output_tokens": 1024,
            "metadata": { "foo": "bar" },
            "input": [
                { "type": "message", "role": "system",
                  "content": [ { "type": "input_text", "text": "be helpful" } ] },
                { "id": "msg_1", "type": "message", "role": "user",
                  "content": [
                    { "type": "input_text", "text": "hello", "future_content_field": true }
                  ],
                  "internal_chat_message_metadata_passthrough": {
                    "turn_id": "turn_1"
                  } }
            ]
        }"#;
        let mut payload: ResponsesPayload = serde_json::from_str(raw).expect("parse payload");

        normalize_codex_responses_payload(&mut payload);

        assert_eq!(payload.store, Some(false));
        assert!(payload.temperature.is_none());
        assert!(payload.top_p.is_none());
        assert!(payload.max_output_tokens.is_none());
        assert!(payload.metadata.is_none());
        assert_eq!(payload.instructions.as_deref(), Some("be helpful"));

        match &payload.input {
            Some(InputField::Items(ref items)) => {
                assert_eq!(items.len(), 1);
                match &items[0] {
                    ResponseInputItem::Message(m) => assert_eq!(m.role, "user"),
                    other => panic!("expected user message, got {other:?}"),
                }
            }
            other => panic!("expected remaining input items, got {other:?}"),
        }

        let normalized = serde_json::to_value(&payload).expect("serialize normalized payload");
        assert_eq!(normalized["input"][0]["id"], "msg_1");
        assert_eq!(
            normalized["input"][0]["internal_chat_message_metadata_passthrough"]["turn_id"],
            "turn_1"
        );
        assert_eq!(
            normalized["input"][0]["content"][0]["future_content_field"],
            true
        );
    }

    #[test]
    fn normalize_keeps_existing_instructions() {
        let raw = r#"{
            "model": "gpt-5.5",
            "instructions": "already set",
            "input": [
                { "type": "message", "role": "system",
                  "content": [ { "type": "input_text", "text": "ignored" } ] }
            ]
        }"#;
        let mut payload: ResponsesPayload = serde_json::from_str(raw).expect("parse payload");

        normalize_codex_responses_payload(&mut payload);

        assert_eq!(payload.instructions.as_deref(), Some("already set"));
        // input untouched (system message still present).
        match payload.input {
            Some(InputField::Items(ref items)) => assert_eq!(items.len(), 1),
            other => panic!("expected input items, got {other:?}"),
        }
    }

    #[test]
    fn normalize_drops_input_when_only_system_messages() {
        let raw = r#"{
            "model": "gpt-5.5",
            "input": [
                { "type": "message", "role": "system",
                  "content": "only system" }
            ]
        }"#;
        let mut payload: ResponsesPayload = serde_json::from_str(raw).expect("parse payload");

        normalize_codex_responses_payload(&mut payload);

        assert_eq!(payload.instructions.as_deref(), Some("only system"));
        assert!(payload.input.is_none());
    }

    #[test]
    fn build_headers_applies_strip_set_and_auth() {
        // Seed the Codex account id in global state; the access token is threaded
        // in explicitly (mirroring the 401-replay call site) rather than read here.
        state::with_state_mut(|s| {
            s.codex_account_id = Some("acct-123".to_string());
        });

        let mut request_headers = axum::http::HeaderMap::new();
        let insert = |map: &mut axum::http::HeaderMap, name: &str, value: &str| {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        };
        // Stripped by name.
        insert(&mut request_headers, "host", "evil.example");
        insert(&mut request_headers, "authorization", "Bearer client-token");
        insert(&mut request_headers, "content-length", "42");
        // Stripped by *trace* / cf- rules.
        insert(&mut request_headers, "x-trace-id", "trace-xyz");
        insert(&mut request_headers, "cf-ray", "ray-1");
        // Kept through.
        insert(&mut request_headers, "x-custom", "keep-me");

        let headers = build_codex_responses_headers(&request_headers, Some(true), "tok-abc")
            .expect("build headers");

        // Strip-set / trace / cf- removed.
        assert!(!headers.contains_key("host"));
        assert!(!headers.contains_key("content-length"));
        assert!(!headers.contains_key("x-trace-id"));
        assert!(!headers.contains_key("cf-ray"));
        // Passthrough kept.
        assert_eq!(headers.get("x-custom").unwrap(), "keep-me");
        // Auth overrides the inbound authorization header with the threaded token.
        assert_eq!(headers.get("authorization").unwrap(), "Bearer tok-abc");
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acct-123");
        // Stream -> SSE accept; defaults layered on.
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(
            headers.get("openai-beta").unwrap(),
            "responses=experimental"
        );
        assert_eq!(headers.get("originator").unwrap(), "copilot-api");
        assert_eq!(headers.get("user-agent").unwrap(), "copilot-api");
    }

    #[test]
    fn build_headers_rejects_empty_access_token() {
        state::with_state_mut(|s| {
            s.codex_account_id = Some("acct-123".to_string());
        });
        let request_headers = axum::http::HeaderMap::new();
        // An empty threaded token is rejected, matching the old "token not loaded"
        // guard now that the access token no longer comes from state here.
        assert!(build_codex_responses_headers(&request_headers, Some(false), "").is_err());
    }

    #[test]
    fn websocket_pool_key_is_stable_opaque_and_auth_scoped() {
        let headers = vec![
            ("user-agent".to_string(), "fixture".to_string()),
            ("x-trace-id".to_string(), "ignored".to_string()),
        ];
        let first =
            codex_websocket_pool_key("wss://example.test", "gpt", "token-a", "account", &headers);
        let reordered = vec![
            ("x-trace-id".to_string(), "different".to_string()),
            ("user-agent".to_string(), "fixture".to_string()),
        ];
        assert_eq!(
            first,
            codex_websocket_pool_key(
                "wss://example.test",
                "gpt",
                "token-a",
                "account",
                &reordered
            )
        );
        assert_ne!(
            first,
            codex_websocket_pool_key("wss://example.test", "gpt", "token-b", "account", &headers)
        );
        assert!(!first.contains("token-a"));
        assert!(!first.contains("account"));
    }

    #[test]
    fn codex_websocket_terminal_set_is_exact() {
        for terminal in [
            "response.completed",
            "response.failed",
            "response.incomplete",
            "error",
        ] {
            assert!(websocket_terminal(&websocket_chunk(
                serde_json::json!({"type": terminal}).to_string()
            )));
        }
        assert!(!websocket_terminal(&websocket_chunk(
            serde_json::json!({"type":"response.output_text.delta"}).to_string()
        )));
        assert!(!websocket_terminal(&websocket_chunk("[DONE]".to_string())));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn codex_websocket_sends_beta_envelope_and_receives_terminal() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::accept_hdr_async;
        use tokio_tungstenite::tungstenite::handshake::server::Request as WsRequest;
        use tokio_tungstenite::tungstenite::Message;

        state::with_state_mut(|state| {
            state.codex_access_token = Some("fixture-access".to_string());
            state.codex_account_id = Some("fixture-account".to_string());
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(tcp, |request: &WsRequest, response| {
                assert_eq!(
                    request.headers()["openai-beta"],
                    "responses_websockets=2026-02-06"
                );
                assert_eq!(request.headers()["chatgpt-account-id"], "fixture-account");
                assert!(!request.headers().contains_key("content-type"));
                Ok(response)
            })
            .await
            .unwrap();
            let request = websocket.next().await.unwrap().unwrap();
            let Message::Text(request) = request else {
                panic!("expected response.create text frame")
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["type"], "response.create");
            assert_eq!(request["model"], "gpt-fixture");
            assert!(request.get("stream").is_none());
            websocket
                .send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"resp_fixture"}}"#.to_string(),
                ))
                .await
                .unwrap();
        });

        let payload: ResponsesPayload =
            serde_json::from_value(serde_json::json!({"model":"gpt-fixture","stream":true}))
                .unwrap();
        let mut stream = forward_codex_responses_websocket_inner(
            payload,
            &axum::http::HeaderMap::new(),
            &format!("http://{address}"),
            true,
        )
        .await
        .unwrap();
        let terminal = stream.next().await.unwrap().unwrap();
        assert_eq!(terminal.event.as_deref(), Some("response.completed"));
        assert!(stream.next().await.is_none());
        server.await.unwrap();
        state::with_state_mut(|state| {
            state.codex_access_token = None;
            state.codex_account_id = None;
        });
    }
}
