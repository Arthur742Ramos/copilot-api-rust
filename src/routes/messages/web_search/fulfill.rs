//! Web-search detection + fulfillment for `/v1/messages`.
//!
//! Ported from `src/routes/messages/web-search/fulfill.ts`. The Anthropic
//! Messages API exposes a server-side `web_search` tool. GitHub Copilot does not
//! support it natively on the Messages path, so a web-search-only request is
//! switched to a Responses-capable GPT model (`messageApiWebSearchModel`), run
//! through Copilot's native `/responses` `web_search`, and the result is
//! reconstructed into native Anthropic `server_tool_use` +
//! `web_search_tool_result` blocks.
//!
//! Crate conventions:
//! - The inbound payload is the typed [`AnthropicMessagesPayload`]; the in-place
//!   preprocess helpers operate on `serde_json::Value`, so we round-trip through
//!   `Value` exactly where the TS code does.
//! - `serde_json` has `preserve_order`, so object key insertion order is
//!   preserved and matters for byte-stable output.
//! - Streaming replays the reconstructed response as a synthetic Anthropic SSE
//!   stream built with `async_stream::stream!` (the TRANSLATING-stream idiom).
//! - To avoid a route<->handler cycle, [`try_handle_web_search`] takes the
//!   provider-forward callback as a generic and calls
//!   [`crate::services::copilot::create_responses::create_responses`] directly.

use std::collections::HashMap;
use std::future::Future;

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use crate::libs::config::{
    get_message_api_web_search_model, is_responses_api_web_search_enabled,
};
use crate::libs::error::AppError;
use crate::libs::models::find_endpoint_model;
use crate::libs::provider_model::{parse_provider_model_alias, ProviderModelAlias};
use crate::libs::sse::events;
use crate::libs::subagent::{parse_subagent_marker_from_first_user, SubagentMarker};
use crate::libs::token_usage::{
    create_copilot_token_usage_recorder, normalize_responses_usage, UsageTokens,
};
use crate::libs::utils::{
    generate_request_id_from_payload, get_root_session_id, get_uuid, parse_user_id_metadata,
};
use crate::routes::messages::anthropic_types::{AnthropicMessagesPayload, AnthropicTool};
use crate::routes::messages::preprocess::{get_compact_type, normalize_system_messages};
use crate::routes::messages::responses_translation::translate_anthropic_messages_to_responses_payload;
use crate::routes::messages::web_search::backend::{
    build_responses_web_search_tool, extract_web_search_result, WebSearchExtract, WebSearchToolConfig,
};
use crate::routes::responses::utils::{
    get_responses_request_options, get_responses_transport_for_model,
};
use crate::services::copilot::create_responses::{
    create_responses, CreateResponsesReturn, ResponsesPayload, ResponsesRequestOptions,
    ResponsesResult, ResponsesTransport,
};

const LOG_TAG: &str = "messages-web-search";

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// `isWebSearchServerTool`: a `type` starting with `web_search` and no
/// `input_schema` (server tools omit the schema; custom tools carry it).
fn is_web_search_server_tool(tool: &AnthropicTool) -> bool {
    tool.kind
        .as_deref()
        .map(|t| t.starts_with("web_search"))
        .unwrap_or(false)
        && tool.input_schema.is_none()
}

/// True when the payload carries an Anthropic server-side `web_search` tool.
pub fn has_web_search_server_tool(payload: &AnthropicMessagesPayload) -> bool {
    payload
        .tools
        .as_ref()
        .map(|tools| tools.iter().any(is_web_search_server_tool))
        .unwrap_or(false)
}

/// True when `web_search` is the ONLY tool in the request. Mixing `web_search`
/// with other tools is intentionally unsupported, so only these requests are
/// switched to the web-search model.
pub fn is_web_search_only_request(payload: &AnthropicMessagesPayload) -> bool {
    payload
        .tools
        .as_ref()
        .map(|tools| !tools.is_empty() && tools.iter().all(is_web_search_server_tool))
        .unwrap_or(false)
}

/// Removes `web_search` server tools (used for unsupported mixed-tool requests).
pub fn strip_web_search_server_tool(payload: &mut AnthropicMessagesPayload) {
    if let Some(tools) = payload.tools.as_mut() {
        tools.retain(|tool| !is_web_search_server_tool(tool));
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Decides how a web-search request should be handled. Pure so the routing is
/// unit-testable. Assumes the caller already confirmed a `web_search` tool
/// exists.
///
/// - `Provider`: `messageApiWebSearchModel` is a `provider/model` alias whose
///   message API supports websearch natively — pass the tool straight through.
/// - `Responses`: a Copilot GPT model — run it via the `/responses` web_search.
/// - `Strip`: mixing with other tools, no model configured, or web search off —
///   drop the tool and continue normally.
pub enum WebSearchRoute {
    Provider { alias: ProviderModelAlias },
    Responses { model: String },
    Strip,
}

/// Options controlling [`resolve_web_search_route`].
pub struct ResolveWebSearchRouteOptions {
    pub web_search_model: Option<String>,
    pub responses_web_search_enabled: bool,
}

/// `resolveWebSearchRoute`.
pub fn resolve_web_search_route(
    payload: &AnthropicMessagesPayload,
    options: ResolveWebSearchRouteOptions,
) -> WebSearchRoute {
    let web_search_model = match options.web_search_model {
        Some(m) if !m.is_empty() => m,
        _ => return WebSearchRoute::Strip,
    };
    if !is_web_search_only_request(payload) {
        return WebSearchRoute::Strip;
    }
    if let Some(alias) = parse_provider_model_alias(&web_search_model) {
        return WebSearchRoute::Provider { alias };
    }
    if options.responses_web_search_enabled {
        return WebSearchRoute::Responses {
            model: web_search_model,
        };
    }
    WebSearchRoute::Strip
}

/// `extractWebSearchConfig`: pull the Anthropic-side tool config from the first
/// `web_search` server tool.
pub fn extract_web_search_config(payload: &AnthropicMessagesPayload) -> WebSearchToolConfig {
    let tool = payload
        .tools
        .as_ref()
        .and_then(|tools| tools.iter().find(|t| is_web_search_server_tool(t)));

    let extra_array = |key: &str| -> Option<Vec<String>> {
        tool.and_then(|t| t.extra.get(key))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
    };

    WebSearchToolConfig {
        allowed_domains: extra_array("allowed_domains"),
        blocked_domains: extra_array("blocked_domains"),
        user_location: tool.and_then(|t| t.extra.get("user_location")).cloned(),
    }
}

// ---------------------------------------------------------------------------
// Reconstruction
// ---------------------------------------------------------------------------

/// `ReconstructedWebSearchResponse` — a native Anthropic assistant `message`
/// rebuilt from the GPT web-search result. `content` holds polymorphic blocks
/// (`text` | `server_tool_use` | `web_search_tool_result`) as raw `Value`s, and
/// `usage` is the bespoke shape carrying `server_tool_use.web_search_requests`.
#[derive(Debug, Clone)]
pub struct ReconstructedWebSearchResponse {
    pub id: String,
    pub model: String,
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Value,
}

impl ReconstructedWebSearchResponse {
    /// Serialize to the wire `message` object (key order matches the TS literal).
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".to_string(), Value::String(self.id.clone()));
        map.insert("type".to_string(), Value::String("message".to_string()));
        map.insert("role".to_string(), Value::String("assistant".to_string()));
        map.insert("content".to_string(), Value::Array(self.content.clone()));
        map.insert("model".to_string(), Value::String(self.model.clone()));
        map.insert(
            "stop_reason".to_string(),
            self.stop_reason
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        map.insert(
            "stop_sequence".to_string(),
            self.stop_sequence
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        map.insert("usage".to_string(), self.usage.clone());
        Value::Object(map)
    }
}

/// `buildWebSearchResultBlock`: one `web_search_tool_result` block whose content
/// is the deduped `web_search_result` items.
fn build_web_search_result_block(tool_use_id: &str, extract: &WebSearchExtract) -> Value {
    let items: Vec<Value> = extract
        .sources
        .iter()
        .map(|source| {
            json!({
                "type": "web_search_result",
                "url": source.url,
                "title": source.title,
                "page_age": source.page_age.clone().map(Value::String).unwrap_or(Value::Null),
                "encrypted_content": "",
            })
        })
        .collect();
    json!({
        "type": "web_search_tool_result",
        "tool_use_id": tool_use_id,
        "content": items,
    })
}

/// `buildResponseContent`: one `server_tool_use` + `web_search_tool_result` pair
/// (when there are sources or a query), then the grounded answer `text` block.
fn build_response_content(request_id: &str, extract: &WebSearchExtract) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    let query = extract.queries.first().cloned().unwrap_or_default();
    if !extract.sources.is_empty() || !query.is_empty() {
        let tool_use_id = format!("srvtoolu_{}", get_uuid(request_id));
        blocks.push(json!({
            "type": "server_tool_use",
            "id": tool_use_id,
            "name": "web_search",
            "input": { "query": query },
        }));
        blocks.push(build_web_search_result_block(&tool_use_id, extract));
    }
    blocks.push(json!({ "type": "text", "text": extract.answer_text }));
    blocks
}

/// `prepareWebSearchResponsesPayload`: switch to the GPT web-search model, drop
/// the Anthropic server tool, translate to a Responses payload, and attach the
/// Responses `web_search` tool.
pub fn prepare_web_search_responses_payload(
    payload: &AnthropicMessagesPayload,
    model: Option<&str>,
    subagent_agent_id: Option<&str>,
) -> ResponsesPayload {
    let config = extract_web_search_config(payload);

    let mut switched = payload.clone();
    if let Some(m) = model {
        switched.model = m.to_string();
    }
    switched.tools = Some(Vec::new());
    switched.stream = Some(true);

    let mut responses_payload =
        translate_anthropic_messages_to_responses_payload(&switched, subagent_agent_id);
    responses_payload.tools = Some(vec![build_responses_web_search_tool(&config)]);
    responses_payload.tool_choice = None;
    responses_payload
}

/// `reconstructWebSearchResponse`: extract + assemble the native Anthropic
/// response from the GPT `/responses` result.
pub fn reconstruct_web_search_response(
    payload: &AnthropicMessagesPayload,
    result: &ResponsesResult,
    request_id: &str,
) -> (WebSearchExtract, ReconstructedWebSearchResponse) {
    let extract = extract_web_search_result(result);

    let id = if result.id.is_empty() {
        get_uuid(request_id)
    } else {
        result.id.clone()
    };

    let input_tokens = result.usage.as_ref().map(|u| u.input_tokens).unwrap_or(0);
    let output_tokens = result
        .usage
        .as_ref()
        .and_then(|u| u.output_tokens)
        .unwrap_or(0);
    let web_search_requests = (extract.queries.len() as i64).max(1);

    let usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "server_tool_use": {
            "web_search_requests": web_search_requests,
        },
    });

    let response = ReconstructedWebSearchResponse {
        id,
        model: payload.model.clone(),
        content: build_response_content(request_id, &extract),
        stop_reason: Some("end_turn".to_string()),
        stop_sequence: None,
        usage,
    };

    (extract, response)
}

// ---------------------------------------------------------------------------
// Streaming collection
//
// When `create_responses` returns a streaming upstream, we collect the SSE
// events into a single `ResponsesResult` (mirroring the TS async-iterator
// collector). The events are read loosely as `Value` because the collector
// merges partial `output_text` deltas back into the terminal response's output
// items, matching the TS field-by-field access.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct CollectedOutputTextPart {
    annotations: Vec<Value>,
    content_index: i64,
    item_id: Option<String>,
    output_index: i64,
    text: String,
}

#[derive(Debug, Default)]
struct WebSearchResponsesStreamCollection {
    created_response: Option<Value>,
    output_items_by_index: HashMap<i64, Value>,
    terminal_response: Option<Value>,
    /// keyed by `"{output_index}:{content_index}"`.
    text_parts_by_key: HashMap<String, CollectedOutputTextPart>,
}

fn get_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn get_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn get_number(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64)
}

fn event_type(event: &Value) -> Option<&str> {
    event.get("type").and_then(Value::as_str)
}

/// `getResponsesResult`: an event's `response` object as a (lenient) result.
fn parse_responses_result(value: Option<&Value>) -> Option<Value> {
    value.filter(|v| v.is_object()).cloned()
}

/// `isResponsesTerminalEvent`.
fn is_responses_terminal_event(event: &Value) -> bool {
    matches!(
        event_type(event),
        Some("response.completed") | Some("response.failed") | Some("response.incomplete")
    ) && event.get("response").map(Value::is_object).unwrap_or(false)
}

/// `getStreamErrorMessage`.
fn get_stream_error_message(event: &Value) -> Option<String> {
    let from_error = event
        .get("error")
        .and_then(get_record)
        .and_then(|e| get_string(e.get("message")));
    from_error.or_else(|| get_string(event.get("message")))
}

fn get_or_create_output_text_part<'a>(
    event: &Value,
    state: &'a mut WebSearchResponsesStreamCollection,
) -> Option<&'a mut CollectedOutputTextPart> {
    let output_index = get_number(event.get("output_index"))?;
    let content_index = get_number(event.get("content_index"))?;
    let key = format!("{output_index}:{content_index}");
    let part = state
        .text_parts_by_key
        .entry(key)
        .or_insert_with(|| CollectedOutputTextPart {
            annotations: Vec::new(),
            content_index,
            item_id: get_string(event.get("item_id")),
            output_index,
            text: String::new(),
        });
    Some(part)
}

/// `collectDoneContentPart` for `response.content_part.done`.
fn collect_done_content_part(event: &Value, state: &mut WebSearchResponsesStreamCollection) {
    let part_record = event.get("part").and_then(get_record).cloned();
    let part_record = match part_record {
        Some(p) if p.get("type").and_then(Value::as_str) == Some("output_text") => p,
        _ => return,
    };

    let text = get_string(part_record.get("text"));
    let annotations: Vec<Value> = part_record
        .get("annotations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if let Some(part) = get_or_create_output_text_part(event, state) {
        if let Some(text) = text {
            part.text = text;
        }
        if !annotations.is_empty() {
            part.annotations.extend(annotations);
        }
    }
}

/// `collectWebSearchResponsesStreamEvent`.
fn collect_web_search_responses_stream_event(
    event: &Value,
    state: &mut WebSearchResponsesStreamCollection,
) {
    match event_type(event) {
        Some("response.created") => {
            state.created_response = parse_responses_result(event.get("response"));
        }
        _ if is_responses_terminal_event(event) => {
            state.terminal_response = event.get("response").cloned();
        }
        Some("response.output_item.added") | Some("response.output_item.done") => {
            let output_index = get_number(event.get("output_index"));
            let item = event.get("item").filter(|v| v.is_object()).cloned();
            if let (Some(idx), Some(item)) = (output_index, item) {
                state.output_items_by_index.insert(idx, item);
            }
        }
        Some("response.output_text.delta") => {
            let delta = get_string(event.get("delta"));
            if let Some(part) = get_or_create_output_text_part(event, state) {
                if let Some(delta) = delta {
                    part.text.push_str(&delta);
                }
            }
        }
        Some("response.output_text.done") => {
            let text = get_string(event.get("text"));
            if let Some(part) = get_or_create_output_text_part(event, state) {
                if let Some(text) = text {
                    part.text = text;
                }
            }
        }
        Some("response.output_text.annotation.added") => {
            let annotation = event.get("annotation").cloned();
            if let Some(part) = get_or_create_output_text_part(event, state) {
                if let Some(annotation) = annotation {
                    part.annotations.push(annotation);
                }
            }
        }
        Some("response.content_part.done") => {
            collect_done_content_part(event, state);
        }
        _ => {}
    }
}

fn collected_text_parts(
    output_index: i64,
    state: &WebSearchResponsesStreamCollection,
) -> Vec<CollectedOutputTextPart> {
    let mut parts: Vec<CollectedOutputTextPart> = state
        .text_parts_by_key
        .values()
        .filter(|p| p.output_index == output_index)
        .cloned()
        .collect();
    parts.sort_by(|a, b| {
        a.content_index.cmp(&b.content_index).then_with(|| {
            a.item_id
                .clone()
                .unwrap_or_default()
                .cmp(&b.item_id.clone().unwrap_or_default())
        })
    });
    parts
}

fn merge_annotations(existing: Option<&Value>, collected: &[Value]) -> Vec<Value> {
    let mut annotations: Vec<Value> = existing
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    annotations.extend(collected.iter().cloned());
    annotations
}

/// `mergeOutputItemWithCollectedText`.
fn merge_output_item_with_collected_text(
    output_index: i64,
    item: &Value,
    state: &WebSearchResponsesStreamCollection,
) -> Value {
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return item.clone();
    }
    let collected = collected_text_parts(output_index, state);
    if collected.is_empty() {
        return item.clone();
    }

    let mut item = item.clone();
    let mut content: Vec<Value> = item
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for part in &collected {
        let idx = part.content_index as usize;
        let existing = content.get(idx).cloned();
        let existing_annotations = existing.as_ref().and_then(|e| e.get("annotations"));
        let mut block = existing
            .clone()
            .and_then(|e| e.as_object().cloned())
            .unwrap_or_default();
        block.insert("type".to_string(), Value::String("output_text".to_string()));
        block.insert("text".to_string(), Value::String(part.text.clone()));
        block.insert(
            "annotations".to_string(),
            Value::Array(merge_annotations(existing_annotations, &part.annotations)),
        );
        if idx < content.len() {
            content[idx] = Value::Object(block);
        } else {
            // Grow the array to the target index (matching JS sparse assignment,
            // filling gaps with null) so positional cites land correctly.
            while content.len() < idx {
                content.push(Value::Null);
            }
            content.push(Value::Object(block));
        }
    }

    if let Some(obj) = item.as_object_mut() {
        obj.insert("content".to_string(), Value::Array(content));
    }
    item
}

/// `buildCollectedWebSearchOutput`: output items sorted by index with collected
/// text merged in.
fn build_collected_web_search_output(state: &WebSearchResponsesStreamCollection) -> Vec<Value> {
    let mut entries: Vec<(i64, Value)> = state
        .output_items_by_index
        .iter()
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    entries.sort_by_key(|(idx, _)| *idx);
    entries
        .into_iter()
        .map(|(idx, item)| merge_output_item_with_collected_text(idx, &item, state))
        .collect()
}

/// `buildWebSearchResponsesStreamResult`: the terminal (or created) response with
/// its `output` replaced by the collected output when non-empty.
#[allow(clippy::result_large_err)]
fn build_web_search_responses_stream_result(
    state: &WebSearchResponsesStreamCollection,
) -> Result<ResponsesResult, AppError> {
    let response = state
        .terminal_response
        .clone()
        .or_else(|| state.created_response.clone())
        .ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "Web search responses stream ended without a response"
            ))
        })?;

    let output = build_collected_web_search_output(state);
    let mut response = response;
    if !output.is_empty() {
        if let Some(obj) = response.as_object_mut() {
            obj.insert("output".to_string(), Value::Array(output));
        }
    }

    serde_json::from_value(response)
        .map_err(|e| AppError::Other(anyhow::anyhow!("Failed to parse web search result: {e}")))
}

/// `collectWebSearchResponsesStreamResult`: drive the upstream SSE stream to a
/// single buffered `ResponsesResult`.
pub async fn collect_web_search_responses_stream_result(
    upstream: reqwest::Response,
    error_message_prefix: &str,
) -> Result<ResponsesResult, AppError> {
    let mut state = WebSearchResponsesStreamCollection::default();
    let stream = events(upstream);
    futures_util::pin_mut!(stream);

    while let Some(item) = stream.next().await {
        let event = item
            .map_err(|e| AppError::Other(anyhow::anyhow!("Web search stream read error: {e}")))?;

        if event.event.as_deref() == Some("ping") {
            continue;
        }
        if event.data.is_empty() || event.data == "[DONE]" {
            continue;
        }

        let parsed: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        collect_web_search_responses_stream_event(&parsed, &mut state);

        if event_type(&parsed) == Some("error") {
            let message = get_stream_error_message(&parsed)
                .unwrap_or_else(|| format!("{error_message_prefix} failed"));
            return Err(AppError::Other(anyhow::anyhow!(message)));
        }

        if is_responses_terminal_event(&parsed) {
            return build_web_search_responses_stream_result(&state);
        }
    }

    Err(AppError::Other(anyhow::anyhow!(
        "{error_message_prefix} ended without a terminal event"
    )))
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Options for [`handle_web_search_via_responses`], mirroring `WebSearchFlowOptions`.
pub struct WebSearchFlowOptions {
    pub subagent_marker: Option<SubagentMarker>,
    /// GPT (Responses-capable) model the web-search request is switched to.
    pub web_search_model: String,
    pub request_id: String,
    pub session_id: Option<String>,
    pub compact_type: i32,
}

/// Entry point for web-search detection and routing on `/v1/messages`.
///
/// Called after model mapping but before provider alias resolution and
/// preprocessing. Returns:
/// - `None` when no `web_search` tool is present, or the tool was stripped
///   (caller continues normal flow).
/// - `Some(Ok(response))` when web search is handled (provider reroute or
///   responses-native).
/// - `Some(Err(_))` when handling failed.
///
/// `forward_to_provider` is injected to avoid a route<->handler import cycle: it
/// is invoked for the `provider` route with the (mutated) payload and provider
/// name. `headers` are the inbound request headers (used to resolve the root
/// session id).
pub async fn try_handle_web_search<F, Fut>(
    payload: &mut AnthropicMessagesPayload,
    headers: &HeaderMap,
    forward_to_provider: F,
) -> Option<Result<Response, AppError>>
where
    F: FnOnce(AnthropicMessagesPayload, String) -> Fut,
    Fut: Future<Output = Result<Response, AppError>>,
{
    if !has_web_search_server_tool(payload) {
        return None;
    }

    // The preprocess helpers operate in-place on a `Value`; round-trip so the
    // normalization matches the TS exactly, then read the result back.
    let mut payload_value = serde_json::to_value(&*payload).ok()?;
    normalize_system_messages(&mut payload_value);
    if let Ok(updated) = serde_json::from_value::<AnthropicMessagesPayload>(payload_value.clone()) {
        *payload = updated;
    }

    let route = resolve_web_search_route(
        payload,
        ResolveWebSearchRouteOptions {
            web_search_model: get_message_api_web_search_model(),
            responses_web_search_enabled: is_responses_api_web_search_enabled(),
        },
    );

    match route {
        WebSearchRoute::Provider { alias } => {
            payload.model = alias.model.clone();
            Some(forward_to_provider(payload.clone(), alias.provider).await)
        }
        WebSearchRoute::Responses { model } => {
            let subagent_marker = parse_subagent_marker_from_first_user(&payload_value);
            let mut session_id = get_root_session_id(&payload_value, headers);
            let messages = payload_value
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let request_id =
                generate_request_id_from_payload(&messages, session_id.as_deref());
            if session_id.is_none() {
                session_id = Some(get_uuid(&request_id));
            }
            let compact_type = get_compact_type(&payload_value);
            Some(
                handle_web_search_via_responses(
                    payload,
                    WebSearchFlowOptions {
                        subagent_marker,
                        web_search_model: model,
                        request_id,
                        session_id,
                        compact_type,
                    },
                )
                .await,
            )
        }
        WebSearchRoute::Strip => {
            strip_web_search_server_tool(payload);
            None
        }
    }
}

/// Handles a web-search-only Claude (Messages API) request by switching it to a
/// Responses-capable GPT model (`web_search_model`), running Copilot's native
/// `/responses` `web_search` in a single call, and reconstructing native
/// Anthropic `server_tool_use` + `web_search_tool_result` blocks. Streaming and
/// non-streaming are both supported (streaming replays the result as a synthetic
/// SSE stream).
pub async fn handle_web_search_via_responses(
    payload: &AnthropicMessagesPayload,
    options: WebSearchFlowOptions,
) -> Result<Response, AppError> {
    let wants_stream = payload.stream.unwrap_or(false);

    // Switch to the GPT web-search model and drop the Anthropic server tool so
    // the standard Anthropic -> Responses translation does not choke on it; the
    // Responses `web_search` tool is attached to the translated payload instead.
    let responses_payload = prepare_web_search_responses_payload(
        payload,
        Some(&options.web_search_model),
        options
            .subagent_marker
            .as_ref()
            .map(|m| m.agent_id.as_str()),
    );

    let selected_model = find_endpoint_model(&options.web_search_model);
    let (vision, initiator) = get_responses_request_options(&responses_payload);
    let compact_type = if options.compact_type != 0 {
        Some(options.compact_type)
    } else {
        None
    };
    let transport = get_responses_transport_for_model(selected_model.as_ref(), compact_type)
        .unwrap_or(ResponsesTransport::Http);

    crate::libs::logger::debug_json(
        LOG_TAG,
        &format!(
            "Switching web search request to model: {}",
            options.web_search_model
        ),
        &serde_json::to_value(&responses_payload).unwrap_or(Value::Null),
    );

    let upstream = create_responses(
        &responses_payload,
        ResponsesRequestOptions {
            vision,
            initiator,
            subagent_marker: options.subagent_marker.as_ref(),
            request_id: &options.request_id,
            session_id: options.session_id.as_deref(),
            compact_type,
            transport,
        },
    )
    .await?;

    let result = match upstream {
        CreateResponsesReturn::Stream(response) => {
            collect_web_search_responses_stream_result(response, "Web search responses stream")
                .await?
        }
        CreateResponsesReturn::Result(result) => *result,
    };

    let (extract, response) =
        reconstruct_web_search_response(payload, &result, &options.request_id);

    crate::libs::logger::debug_json(
        LOG_TAG,
        &format!(
            "Web search via responses: {} quer(y/ies), {} source(s)",
            extract.queries.len(),
            extract.sources.len()
        ),
        &serde_json::to_value(&result).unwrap_or(Value::Null),
    );

    let recorder = create_copilot_token_usage_recorder(
        "responses",
        options.web_search_model.clone(),
        options.session_id.clone(),
    );
    // The session id from the payload metadata overrides the recorder default.
    let session_from_metadata =
        parse_user_id_metadata(payload.metadata.as_ref().and_then(|m| m.user_id.as_deref()))
            .session_id;
    let recorder = TokenUsageRecorderWithSession {
        recorder,
        session_id: session_from_metadata,
    };
    recorder.record(normalize_responses_usage(
        result.usage.as_ref().and_then(|u| serde_json::to_value(u).ok()).as_ref(),
    ));

    if !wants_stream {
        return Ok(Json(response.to_json()).into_response());
    }

    Ok(synthetic_stream_response(&response))
}

/// Small wrapper applying the `metadata.user_id` session id to a recorder, since
/// `create_copilot_token_usage_recorder` only sets `fallback_session_id`.
struct TokenUsageRecorderWithSession {
    recorder: crate::libs::token_usage::TokenUsageRecorder,
    session_id: Option<String>,
}

impl TokenUsageRecorderWithSession {
    fn record(self, usage: UsageTokens) {
        let mut recorder = self.recorder;
        recorder.session_id = self.session_id;
        recorder.record(usage);
    }
}

// ---------------------------------------------------------------------------
// Synthetic SSE replay
// ---------------------------------------------------------------------------

/// `blockToStreamEvents`: expand a reconstructed content block into the
/// Anthropic `content_block_start` / `content_block_delta` / `content_block_stop`
/// event sequence.
pub fn block_to_stream_events(block: &Value, index: i64) -> Vec<Value> {
    let start = |content_block: Value| -> Value {
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": content_block,
        })
    };
    let stop = json!({ "type": "content_block_stop", "index": index });

    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = block.get("text").cloned().unwrap_or(Value::String(String::new()));
            vec![
                start(json!({ "type": "text", "text": "" })),
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "text_delta", "text": text },
                }),
                stop,
            ]
        }
        Some("server_tool_use") => {
            let id = block.get("id").cloned().unwrap_or(Value::Null);
            let name = block.get("name").cloned().unwrap_or(Value::Null);
            let input = block.get("input").cloned().unwrap_or(json!({}));
            let partial_json = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
            vec![
                start(json!({
                    "type": "server_tool_use",
                    "id": id,
                    "name": name,
                    "input": {},
                })),
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": partial_json,
                    },
                }),
                stop,
            ]
        }
        // web_search_tool_result + any other block: full block in the start event.
        _ => vec![start(block.clone()), stop],
    }
}

/// `buildSyntheticStreamEvents`: a full Anthropic message stream
/// (`message_start` -> per-block events -> `message_delta` -> `message_stop`)
/// replaying the reconstructed response.
pub fn build_synthetic_stream_events(response: &ReconstructedWebSearchResponse) -> Vec<Value> {
    let mut events: Vec<Value> = Vec::new();

    // message_start carries usage with output_tokens forced to 0.
    let mut start_usage = response.usage.clone();
    if let Some(obj) = start_usage.as_object_mut() {
        obj.insert("output_tokens".to_string(), Value::from(0));
    }
    events.push(json!({
        "type": "message_start",
        "message": {
            "id": response.id,
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": response.model,
            "stop_reason": null,
            "stop_sequence": null,
            "usage": start_usage,
        },
    }));

    for (index, block) in response.content.iter().enumerate() {
        events.extend(block_to_stream_events(block, index as i64));
    }

    let output_tokens = response
        .usage
        .get("output_tokens")
        .cloned()
        .unwrap_or(Value::from(0));
    events.push(json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": response.stop_reason,
            "stop_sequence": response.stop_sequence,
        },
        "usage": { "output_tokens": output_tokens },
    }));
    events.push(json!({ "type": "message_stop" }));

    events
}

/// Build the synthetic SSE response that replays the reconstructed message.
fn synthetic_stream_response(response: &ReconstructedWebSearchResponse) -> Response {
    let events = build_synthetic_stream_events(response);

    let body_stream = async_stream::stream! {
        for event in events {
            let event_type = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message")
                .to_string();
            let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            let frame = format!("event: {event_type}\ndata: {data}\n\n");
            yield Ok::<Bytes, std::io::Error>(Bytes::from(frame));
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_from(value: Value) -> AnthropicMessagesPayload {
        serde_json::from_value(value).expect("parse payload")
    }

    fn web_search_payload(tools: Value) -> AnthropicMessagesPayload {
        payload_from(json!({
            "model": "claude-3-5-sonnet",
            "messages": [{ "role": "user", "content": "search the web" }],
            "max_tokens": 1024,
            "tools": tools,
        }))
    }

    #[test]
    fn detects_web_search_server_tool() {
        let payload = web_search_payload(json!([{ "type": "web_search_20250305" }]));
        assert!(has_web_search_server_tool(&payload));
        assert!(is_web_search_only_request(&payload));
    }

    #[test]
    fn custom_tool_with_input_schema_is_not_web_search() {
        // A tool whose type starts with web_search but carries an input_schema is
        // a custom tool, not the server tool.
        let payload = web_search_payload(json!([
            { "type": "web_search_custom", "input_schema": { "type": "object" } }
        ]));
        assert!(!has_web_search_server_tool(&payload));
    }

    #[test]
    fn mixed_tools_are_not_web_search_only() {
        let payload = web_search_payload(json!([
            { "type": "web_search_20250305" },
            { "name": "calculator", "input_schema": { "type": "object" } }
        ]));
        assert!(has_web_search_server_tool(&payload));
        assert!(!is_web_search_only_request(&payload));
    }

    #[test]
    fn strip_removes_only_web_search_tools() {
        let mut payload = web_search_payload(json!([
            { "type": "web_search_20250305" },
            { "name": "calculator", "input_schema": { "type": "object" } }
        ]));
        strip_web_search_server_tool(&mut payload);
        let tools = payload.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_deref(), Some("calculator"));
    }

    #[test]
    fn route_strip_without_model() {
        let payload = web_search_payload(json!([{ "type": "web_search_20250305" }]));
        let route = resolve_web_search_route(
            &payload,
            ResolveWebSearchRouteOptions {
                web_search_model: None,
                responses_web_search_enabled: true,
            },
        );
        assert!(matches!(route, WebSearchRoute::Strip));
    }

    #[test]
    fn route_provider_for_alias_model() {
        let payload = web_search_payload(json!([{ "type": "web_search_20250305" }]));
        let route = resolve_web_search_route(
            &payload,
            ResolveWebSearchRouteOptions {
                web_search_model: Some("acme/web-model".to_string()),
                responses_web_search_enabled: true,
            },
        );
        match route {
            WebSearchRoute::Provider { alias } => {
                assert_eq!(alias.provider, "acme");
                assert_eq!(alias.model, "web-model");
            }
            _ => panic!("expected provider route"),
        }
    }

    #[test]
    fn route_responses_for_plain_model() {
        let payload = web_search_payload(json!([{ "type": "web_search_20250305" }]));
        let route = resolve_web_search_route(
            &payload,
            ResolveWebSearchRouteOptions {
                web_search_model: Some("gpt-5-mini".to_string()),
                responses_web_search_enabled: true,
            },
        );
        match route {
            WebSearchRoute::Responses { model } => assert_eq!(model, "gpt-5-mini"),
            _ => panic!("expected responses route"),
        }
    }

    #[test]
    fn route_strip_when_responses_disabled() {
        let payload = web_search_payload(json!([{ "type": "web_search_20250305" }]));
        let route = resolve_web_search_route(
            &payload,
            ResolveWebSearchRouteOptions {
                web_search_model: Some("gpt-5-mini".to_string()),
                responses_web_search_enabled: false,
            },
        );
        assert!(matches!(route, WebSearchRoute::Strip));
    }

    #[test]
    fn route_strip_for_mixed_tools_even_with_model() {
        let payload = web_search_payload(json!([
            { "type": "web_search_20250305" },
            { "name": "calculator", "input_schema": { "type": "object" } }
        ]));
        let route = resolve_web_search_route(
            &payload,
            ResolveWebSearchRouteOptions {
                web_search_model: Some("gpt-5-mini".to_string()),
                responses_web_search_enabled: true,
            },
        );
        assert!(matches!(route, WebSearchRoute::Strip));
    }

    #[test]
    fn extract_config_reads_domains_and_location() {
        let payload = web_search_payload(json!([{
            "type": "web_search_20250305",
            "allowed_domains": ["a.com", "b.com"],
            "blocked_domains": ["c.com"],
            "user_location": { "type": "approximate", "country": "US" }
        }]));
        let config = extract_web_search_config(&payload);
        assert_eq!(
            config.allowed_domains,
            Some(vec!["a.com".to_string(), "b.com".to_string()])
        );
        assert_eq!(config.blocked_domains, Some(vec!["c.com".to_string()]));
        assert_eq!(
            config.user_location,
            Some(json!({ "type": "approximate", "country": "US" }))
        );
    }

    fn result_from(value: Value) -> ResponsesResult {
        serde_json::from_value(value).expect("parse result")
    }

    #[test]
    fn reconstruct_builds_server_tool_use_and_result_blocks() {
        let result = result_from(json!({
            "id": "resp_1",
            "object": "response",
            "created_at": 1,
            "model": "gpt-5-mini",
            "status": "completed",
            "output_text": "",
            "usage": { "input_tokens": 12, "output_tokens": 8, "total_tokens": 20 },
            "output": [
                { "type": "web_search_call", "action": { "query": "rust async" } },
                {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": "Answer.",
                        "annotations": [
                            { "type": "url_citation", "url": "https://x.com", "title": "X" }
                        ]
                    }]
                }
            ]
        }));
        let payload = web_search_payload(json!([{ "type": "web_search_20250305" }]));
        let (extract, response) = reconstruct_web_search_response(&payload, &result, "req-1");

        assert_eq!(extract.queries, vec!["rust async"]);
        assert_eq!(extract.sources.len(), 1);
        assert_eq!(response.id, "resp_1");
        assert_eq!(response.model, "claude-3-5-sonnet");
        assert_eq!(response.stop_reason.as_deref(), Some("end_turn"));

        // content: server_tool_use, web_search_tool_result, text
        assert_eq!(response.content.len(), 3);
        assert_eq!(response.content[0]["type"], "server_tool_use");
        assert_eq!(response.content[0]["name"], "web_search");
        assert_eq!(response.content[0]["input"]["query"], "rust async");
        assert_eq!(response.content[1]["type"], "web_search_tool_result");
        assert_eq!(response.content[1]["content"][0]["url"], "https://x.com");
        assert_eq!(response.content[2]["type"], "text");
        assert_eq!(response.content[2]["text"], "Answer.");

        // usage carries the bespoke server_tool_use.web_search_requests counter.
        assert_eq!(response.usage["input_tokens"], 12);
        assert_eq!(response.usage["output_tokens"], 8);
        assert_eq!(response.usage["server_tool_use"]["web_search_requests"], 1);
    }

    #[test]
    fn reconstruct_text_only_when_no_sources_or_query() {
        let result = result_from(json!({
            "id": "",
            "object": "response",
            "created_at": 1,
            "model": "gpt-5-mini",
            "status": "completed",
            "output_text": "Just an answer.",
            "output": []
        }));
        let payload = web_search_payload(json!([{ "type": "web_search_20250305" }]));
        let (_extract, response) = reconstruct_web_search_response(&payload, &result, "req-2");

        // No server_tool_use pair; just the text block.
        assert_eq!(response.content.len(), 1);
        assert_eq!(response.content[0]["type"], "text");
        assert_eq!(response.content[0]["text"], "Just an answer.");
        // Empty result id falls back to a deterministic uuid from the request id.
        assert_eq!(response.id, get_uuid("req-2"));
        // web_search_requests is max(queries.len(), 1) == 1.
        assert_eq!(response.usage["server_tool_use"]["web_search_requests"], 1);
    }

    #[test]
    fn synthetic_events_replay_full_message_stream() {
        let response = ReconstructedWebSearchResponse {
            id: "msg_x".to_string(),
            model: "claude-3-5-sonnet".to_string(),
            content: vec![
                json!({ "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": { "query": "q" } }),
                json!({ "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [] }),
                json!({ "type": "text", "text": "Hello" }),
            ],
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: json!({
                "input_tokens": 5,
                "output_tokens": 9,
                "server_tool_use": { "web_search_requests": 1 }
            }),
        };

        let events = build_synthetic_stream_events(&response);
        // message_start (1)
        // + server_tool_use block (3: start, delta, stop)
        // + web_search_tool_result block (2: start, stop)
        // + text block (3: start, delta, stop)
        // + message_delta (1) + message_stop (1) = 11
        assert_eq!(events.len(), 11);
        assert_eq!(events[0]["type"], "message_start");
        // message_start usage zeroes output_tokens.
        assert_eq!(events[0]["message"]["usage"]["output_tokens"], 0);
        assert_eq!(events[0]["message"]["usage"]["input_tokens"], 5);

        // server_tool_use block: start has empty input, delta carries partial_json.
        assert_eq!(events[1]["type"], "content_block_start");
        assert_eq!(events[1]["content_block"]["type"], "server_tool_use");
        assert_eq!(events[1]["content_block"]["input"], json!({}));
        assert_eq!(events[2]["type"], "content_block_delta");
        assert_eq!(events[2]["delta"]["type"], "input_json_delta");
        assert_eq!(events[2]["delta"]["partial_json"], "{\"query\":\"q\"}");
        assert_eq!(events[3]["type"], "content_block_stop");

        // web_search_tool_result block: full block delivered in start, then stop.
        assert_eq!(events[4]["type"], "content_block_start");
        assert_eq!(
            events[4]["content_block"]["type"],
            "web_search_tool_result"
        );
        assert_eq!(events[5]["type"], "content_block_stop");

        // text block: start empty, delta carries text.
        assert_eq!(events[6]["content_block"]["type"], "text");
        assert_eq!(events[6]["content_block"]["text"], "");
        assert_eq!(events[7]["delta"]["type"], "text_delta");
        assert_eq!(events[7]["delta"]["text"], "Hello");
        assert_eq!(events[8]["type"], "content_block_stop");

        // message_delta + message_stop with final output_tokens.
        assert_eq!(events[9]["type"], "message_delta");
        assert_eq!(events[9]["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[9]["usage"]["output_tokens"], 9);
        assert_eq!(events[10]["type"], "message_stop");
    }

    #[test]
    fn prepare_payload_drops_anthropic_tool_and_attaches_responses_tool() {
        let payload = web_search_payload(json!([{
            "type": "web_search_20250305",
            "allowed_domains": ["a.com"]
        }]));
        let responses_payload =
            prepare_web_search_responses_payload(&payload, Some("gpt-5-mini"), None);
        assert_eq!(responses_payload.model, "gpt-5-mini");
        let tools = responses_payload.tools.expect("tools present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "web_search");
        assert_eq!(tools[0]["filters"]["allowed_domains"][0], "a.com");
        assert!(responses_payload.tool_choice.is_none());
    }
}
