//! Codex `/responses` HTTP transport.
//!
//! Ported from services/codex/create-responses.ts. SCOPE: HTTP transport only.
//! The pooled WebSocket transport (and its pool-key / chunk-translation helpers)
//! is Phase 5; where the TS branches to websocket we take the HTTP path and
//! leave a `// TODO Phase 5 WS` marker.
//!
//! Conventions match the rest of the crate (see create_chat_completions.rs):
//! services return `Result<_, HttpError>`; headers are built into a
//! `reqwest::header::HeaderMap` and handed to `client().post(...).headers(...)`.

use serde_json::Value;

use crate::libs::error::HttpError;
use crate::libs::http::{client, serialize_json_body};
use crate::libs::request_context::request_context_store;
use crate::libs::state;
use crate::services::copilot::create_responses::{
    InputField, MessageContent, ResponseInputContent, ResponseInputItem, ResponsesPayload,
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
/// TODO Phase 5 WS: the TS branches to `forwardCodexResponsesOverWebSocket` when
/// `payload.stream && transport === "websocket"`. That pooled-websocket path is
/// Phase 5; here we always take the HTTP transport.
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
            .post(&url)
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
}
