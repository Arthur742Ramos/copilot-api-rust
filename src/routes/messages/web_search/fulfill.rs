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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use crate::libs::config::{get_message_api_web_search_model, is_responses_api_web_search_enabled};
use crate::libs::error::AppError;
use crate::libs::models::find_endpoint_model;
use crate::libs::provider_model::{parse_provider_model_alias, ProviderModelAlias};
use crate::libs::subagent::{parse_subagent_marker_from_first_user, SubagentMarker};
use crate::libs::token_usage::{
    create_copilot_token_usage_recorder, normalize_responses_usage, UsageTokens,
};
use crate::libs::utils::{
    generate_request_id_from_payload, get_root_session_id, get_uuid, parse_user_id_metadata,
};
use crate::routes::messages::anthropic_types::{AnthropicMessagesPayload, AnthropicTool};
use crate::routes::messages::preprocess::{get_compact_type, normalize_system_messages};
use crate::routes::messages::responses_translation::{
    optional_nonnull_string_field, parse_and_validate_output_item,
    translate_anthropic_messages_to_responses_payload, validate_created_status,
    validate_output_item_reconciliation, validate_raw_responses_usage, validate_terminal_status,
    validate_typed_output_items_and_usage, OutputValidationPhase, ResponsesTerminalKind,
    ValidatedResponsesUsage,
};
use crate::routes::messages::web_search::backend::{
    build_responses_web_search_tool, extract_web_search_result, WebSearchExtract,
    WebSearchToolConfig,
};
use crate::routes::responses::utils::{
    get_responses_request_options, get_responses_transport_for_model,
};
use crate::services::copilot::create_responses::{
    create_responses, CreateResponsesReturn, ResponseOutputContentBlock, ResponseOutputItem,
    ResponsesBufferedContract, ResponsesPayload, ResponsesRequestOptions, ResponsesResult,
    ResponsesTransport,
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
        let tool_use_id = extract
            .tool_use_id
            .clone()
            .unwrap_or_else(|| format!("srvtoolu_{}", get_uuid(request_id)));
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
#[allow(clippy::result_large_err)]
pub fn prepare_web_search_responses_payload(
    payload: &AnthropicMessagesPayload,
    model: Option<&str>,
    subagent_agent_id: Option<&str>,
) -> Result<ResponsesPayload, AppError> {
    let config = extract_web_search_config(payload);

    let mut switched = payload.clone();
    if let Some(m) = model {
        switched.model = m.to_string();
    }
    switched.tools = Some(Vec::new());
    switched.stream = Some(true);

    let mut responses_payload =
        translate_anthropic_messages_to_responses_payload(&switched, subagent_agent_id)?;
    responses_payload.tools = Some(vec![build_responses_web_search_tool(&config)]);
    responses_payload.tool_choice = None;
    Ok(responses_payload)
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

    let (input_tokens, output_tokens) = match result.usage.as_ref() {
        Some(usage) => (usage.input_tokens, usage.output_tokens),
        None => (0, 0),
    };
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
        stop_reason: Some(
            if result.extra.get("end_turn").and_then(Value::as_bool) == Some(false) {
                "pause_turn"
            } else {
                "end_turn"
            }
            .to_string(),
        ),
        stop_sequence: None,
        usage,
    };

    (extract, response)
}

#[allow(clippy::result_large_err)]
fn representable_web_search_query(raw: &Value) -> Result<String, AppError> {
    let action = raw
        .get("action")
        .and_then(Value::as_object)
        .filter(|action| action.get("type").and_then(Value::as_str) == Some("search"))
        .ok_or_else(|| {
            invalid_web_search_stream(
                "the web-search call action cannot be represented by Anthropic web_search",
            )
        })?;
    let fallback_query = || {
        action
            .get("query")
            .and_then(Value::as_str)
            .filter(|query| !query.is_empty())
            .map(|query| vec![query])
            .unwrap_or_default()
    };
    let queries: Vec<&str> = match action.get("queries") {
        None | Some(Value::Null) => fallback_query(),
        Some(Value::Array(queries)) if queries.is_empty() => fallback_query(),
        Some(Value::Array(queries))
            if queries
                .iter()
                .all(|query| query.as_str().is_some_and(|query| !query.is_empty())) =>
        {
            queries.iter().filter_map(Value::as_str).collect()
        }
        Some(_) => {
            return Err(invalid_web_search_stream(
                "the web-search call queries field was invalid",
            ))
        }
    };
    if queries.len() != 1 {
        return Err(invalid_web_search_stream(
            "the web-search call did not contain exactly one representable query",
        ));
    }
    Ok(queries[0].to_string())
}

#[allow(clippy::result_large_err)]
fn validate_optional_object_field(value: &Value, field: &str) -> Result<(), AppError> {
    if value
        .get(field)
        .is_some_and(|value| !value.is_null() && !value.is_object())
    {
        return Err(invalid_web_search_stream(format_args!(
            "{field} was not an object or null"
        )));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
pub(crate) fn validate_web_search_result(result: &ResponsesResult) -> Result<(), AppError> {
    if result.id.trim().is_empty() || result.status != "completed" {
        return Err(invalid_web_search_stream(
            "the web-search response had invalid identity or completion status",
        ));
    }
    if !result.metadata.is_null() && !result.metadata.is_object() {
        return Err(invalid_web_search_stream(
            "metadata was not an object or null",
        ));
    }
    if !result.incomplete_details.is_null() && !result.incomplete_details.is_object() {
        return Err(invalid_web_search_stream(
            "incomplete_details was not an object or null",
        ));
    }
    validate_typed_output_items_and_usage(result)?;

    let mut web_search_calls = 0_usize;
    for item in &result.output {
        let raw = match item {
            ResponseOutputItem::Message(message) => {
                for block in &message.content {
                    let ResponseOutputContentBlock::Text(text) = block else {
                        return Err(invalid_web_search_stream(
                            "the web-search response contained unsupported message content",
                        ));
                    };
                    if text.block_type != "output_text" {
                        return Err(invalid_web_search_stream(
                            "the web-search response contained unsupported message content",
                        ));
                    }
                    canonical_web_annotations(text.annotations.as_deref())?;
                }
                continue;
            }
            ResponseOutputItem::Other(raw) => raw,
            _ => return Err(invalid_web_search_stream(
                "the web-search response contained an output item unsupported by reconstruction",
            )),
        };
        if raw.get("type").and_then(Value::as_str) != Some("web_search_call") {
            return Err(invalid_web_search_stream(
                "the web-search response contained an unsupported raw output variant",
            ));
        }
        web_search_calls += 1;
        if web_search_calls > 1 {
            return Err(invalid_web_search_stream(
                "multiple web-search calls cannot be represented losslessly",
            ));
        }
        optional_nonnull_string_field(raw, "id").map_err(invalid_web_search_stream)?;
        if optional_nonnull_string_field(raw, "status")
            .map_err(invalid_web_search_stream)?
            .is_some_and(|status| status != "completed")
        {
            return Err(invalid_web_search_stream(
                "the completed web-search call had a non-completed status",
            ));
        }
        representable_web_search_query(raw)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotFieldAuthority {
    RequiredStable,
    OptionalStable,
    RequestedFallback,
    PhaseDiscriminator,
    StructuredOutput,
    StructuredUsage,
    TerminalBoolean,
    IgnoredRaw,
}

#[derive(Debug, Clone, Copy)]
struct SnapshotFieldRule {
    name: &'static str,
    authority: SnapshotFieldAuthority,
}

const WEB_SNAPSHOT_FIELD_RULES: &[SnapshotFieldRule] = &[
    SnapshotFieldRule {
        name: "id",
        authority: SnapshotFieldAuthority::RequiredStable,
    },
    SnapshotFieldRule {
        name: "model",
        authority: SnapshotFieldAuthority::RequestedFallback,
    },
    SnapshotFieldRule {
        name: "object",
        authority: SnapshotFieldAuthority::OptionalStable,
    },
    SnapshotFieldRule {
        name: "status",
        authority: SnapshotFieldAuthority::PhaseDiscriminator,
    },
    SnapshotFieldRule {
        name: "output",
        authority: SnapshotFieldAuthority::StructuredOutput,
    },
    SnapshotFieldRule {
        name: "output_text",
        authority: SnapshotFieldAuthority::OptionalStable,
    },
    SnapshotFieldRule {
        name: "usage",
        authority: SnapshotFieldAuthority::StructuredUsage,
    },
    SnapshotFieldRule {
        name: "metadata",
        authority: SnapshotFieldAuthority::OptionalStable,
    },
    SnapshotFieldRule {
        name: "incomplete_details",
        authority: SnapshotFieldAuthority::OptionalStable,
    },
    SnapshotFieldRule {
        name: "end_turn",
        authority: SnapshotFieldAuthority::TerminalBoolean,
    },
    SnapshotFieldRule {
        name: "created_at",
        authority: SnapshotFieldAuthority::IgnoredRaw,
    },
    SnapshotFieldRule {
        name: "error",
        authority: SnapshotFieldAuthority::IgnoredRaw,
    },
    SnapshotFieldRule {
        name: "instructions",
        authority: SnapshotFieldAuthority::IgnoredRaw,
    },
    SnapshotFieldRule {
        name: "parallel_tool_calls",
        authority: SnapshotFieldAuthority::IgnoredRaw,
    },
    SnapshotFieldRule {
        name: "temperature",
        authority: SnapshotFieldAuthority::IgnoredRaw,
    },
    SnapshotFieldRule {
        name: "tool_choice",
        authority: SnapshotFieldAuthority::IgnoredRaw,
    },
    SnapshotFieldRule {
        name: "tools",
        authority: SnapshotFieldAuthority::IgnoredRaw,
    },
    SnapshotFieldRule {
        name: "top_p",
        authority: SnapshotFieldAuthority::IgnoredRaw,
    },
];

fn snapshot_field_authority(field: &str) -> SnapshotFieldAuthority {
    WEB_SNAPSHOT_FIELD_RULES
        .iter()
        .find(|rule| rule.name == field)
        .map(|rule| rule.authority)
        .unwrap_or(SnapshotFieldAuthority::IgnoredRaw)
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
    done_text: Option<String>,
    item_id: Option<String>,
    output_index: i64,
    text: String,
}

#[derive(Debug, Default)]
struct WebSearchResponsesStreamCollection {
    created_response: Option<Value>,
    output_items_by_index: BTreeMap<i64, CollectedOutputItem>,
    terminal_response: Option<Value>,
    terminal_kind: Option<ResponsesTerminalKind>,
    /// keyed by `"{output_index}:{content_index}"`.
    text_parts_by_key: HashMap<String, CollectedOutputTextPart>,
}

#[derive(Debug, Clone, Default)]
struct CollectedOutputItem {
    added: Option<Value>,
    done: Option<Value>,
}

fn get_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn get_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn event_type(event: &Value) -> Option<&str> {
    event.get("type").and_then(Value::as_str)
}

fn invalid_web_search_stream(message: impl std::fmt::Display) -> AppError {
    AppError::Other(anyhow::anyhow!(
        "Invalid upstream Responses web-search stream: {message}"
    ))
}

#[allow(clippy::result_large_err)]
fn required_nonnegative_event_index(event: &Value, field: &str) -> Result<i64, AppError> {
    event
        .get(field)
        .and_then(Value::as_i64)
        .filter(|index| *index >= 0)
        .ok_or_else(|| {
            invalid_web_search_stream(format_args!(
                "{field} was missing, wrong-typed, or negative"
            ))
        })
}

#[allow(clippy::result_large_err)]
fn optional_event_string<'a>(value: &'a Value, field: &str) -> Result<Option<&'a str>, AppError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid_web_search_stream(format_args!(
            "{field} was not a string or null"
        ))),
    }
}

#[allow(clippy::result_large_err)]
fn validate_web_search_snapshot_fields(
    response: &Value,
    phase: OutputValidationPhase,
) -> Result<(), AppError> {
    if optional_nonnull_string_field(response, "model")
        .map_err(invalid_web_search_stream)?
        .is_some_and(|model| model.trim().is_empty())
    {
        return Err(invalid_web_search_stream(
            "a web-search response snapshot had an empty model",
        ));
    }
    if optional_nonnull_string_field(response, "object")
        .map_err(invalid_web_search_stream)?
        .is_some_and(|object| object != "response")
    {
        return Err(invalid_web_search_stream(
            "a web-search response snapshot had an invalid object",
        ));
    }
    match response.get("output") {
        None | Some(Value::Null) => {}
        Some(Value::Array(output)) => {
            for item in output {
                parse_and_validate_output_item(item, phase).map_err(invalid_web_search_stream)?;
            }
        }
        Some(_) => {
            return Err(invalid_web_search_stream(
                "a web-search response snapshot had non-array output",
            ))
        }
    }
    if response
        .get("output_text")
        .is_some_and(|output_text| !output_text.is_null() && !output_text.is_string())
    {
        return Err(invalid_web_search_stream(
            "a web-search response snapshot had invalid output_text",
        ));
    }
    validate_optional_object_field(response, "metadata")?;
    validate_optional_object_field(response, "incomplete_details")?;
    validate_raw_responses_usage(response).map_err(invalid_web_search_stream)?;
    Ok(())
}

/// `getStreamErrorMessage`.
fn get_stream_error_message(event: &Value) -> Option<String> {
    let from_error = event
        .get("error")
        .and_then(get_record)
        .and_then(|e| get_string(e.get("message")));
    from_error.or_else(|| get_string(event.get("message")))
}

#[allow(clippy::result_large_err)]
fn get_or_create_output_text_part<'a>(
    event: &Value,
    state: &'a mut WebSearchResponsesStreamCollection,
) -> Result<&'a mut CollectedOutputTextPart, AppError> {
    let output_index = required_nonnegative_event_index(event, "output_index")?;
    let content_index = required_nonnegative_event_index(event, "content_index")?;
    let item_id = optional_event_string(event, "item_id")?.map(str::to_string);
    let key = format!("{output_index}:{content_index}");
    let part = state
        .text_parts_by_key
        .entry(key)
        .or_insert_with(|| CollectedOutputTextPart {
            annotations: Vec::new(),
            content_index,
            done_text: None,
            item_id: item_id.clone(),
            output_index,
            text: String::new(),
        });
    if part.item_id.is_some() && item_id.is_some() && part.item_id != item_id {
        return Err(invalid_web_search_stream(
            "a text part changed its output item id",
        ));
    }
    if part.item_id.is_none() {
        part.item_id = item_id;
    }
    let lifecycle = state
        .output_items_by_index
        .get(&output_index)
        .ok_or_else(|| {
            invalid_web_search_stream("a text event referenced an unknown output item")
        })?;
    if lifecycle.done.is_some() {
        return Err(invalid_web_search_stream(
            "text data arrived after output_item.done",
        ));
    }
    if lifecycle
        .added
        .as_ref()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        != Some("message")
    {
        return Err(invalid_web_search_stream(
            "a text event referenced a non-message output item",
        ));
    }
    Ok(part)
}

/// `collectDoneContentPart` for `response.content_part.done`.
#[allow(clippy::result_large_err)]
fn collect_done_content_part(
    event: &Value,
    state: &mut WebSearchResponsesStreamCollection,
) -> Result<(), AppError> {
    let Some(part_record) = event.get("part").and_then(get_record) else {
        return Err(invalid_web_search_stream(
            "response.content_part.done was missing its part object",
        ));
    };
    if part_record.get("type").and_then(Value::as_str) != Some("output_text") {
        return Err(invalid_web_search_stream(
            "response.content_part.done contained an unsupported part type",
        ));
    }
    let text = part_record
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_web_search_stream("an output_text part had invalid text"))?;
    let annotations = match part_record.get("annotations") {
        None => Vec::new(),
        Some(Value::Array(annotations))
            if annotations.iter().all(|annotation| annotation.is_object()) =>
        {
            annotations.clone()
        }
        Some(_) => {
            return Err(invalid_web_search_stream(
                "an output_text part had invalid annotations",
            ))
        }
    };
    let part = get_or_create_output_text_part(event, state)?;
    if part.done_text.as_deref().is_some_and(|done| done != text) {
        return Err(invalid_web_search_stream(
            "response.content_part.done conflicted with output_text.done",
        ));
    }
    part.text = text.to_string();
    part.done_text = Some(text.to_string());
    part.annotations.extend(annotations);
    Ok(())
}

/// `collectWebSearchResponsesStreamEvent`.
#[allow(clippy::result_large_err)]
fn collect_web_search_responses_stream_event(
    event: &Value,
    state: &mut WebSearchResponsesStreamCollection,
) -> Result<(), AppError> {
    let event_type = event_type(event)
        .ok_or_else(|| invalid_web_search_stream("an event had a missing or invalid type"))?;
    if state.terminal_response.is_some() {
        return Err(invalid_web_search_stream(
            "an event arrived after the terminal Responses event",
        ));
    }
    if state.created_response.is_none() && !matches!(event_type, "response.created" | "error") {
        return Err(invalid_web_search_stream(
            "an event arrived before response.created",
        ));
    }
    match event_type {
        "response.created" => {
            if state.created_response.is_some() {
                return Err(invalid_web_search_stream(
                    "more than one response.created event was emitted",
                ));
            }
            let response = event
                .get("response")
                .filter(|response| response.is_object())
                .ok_or_else(|| {
                    invalid_web_search_stream("response.created was missing its response object")
                })?;
            response
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    invalid_web_search_stream("response.created had an invalid response id")
                })?;
            validate_created_status(response).map_err(invalid_web_search_stream)?;
            validate_web_search_snapshot_fields(response, OutputValidationPhase::Added)?;
            state.created_response = Some(response.clone());
        }
        "response.completed" | "response.incomplete" | "response.failed" => {
            if state.terminal_response.is_some() {
                return Err(invalid_web_search_stream(
                    "more than one terminal event was emitted",
                ));
            }
            let response = event
                .get("response")
                .filter(|response| response.is_object())
                .ok_or_else(|| {
                    invalid_web_search_stream("a terminal event had an invalid response object")
                })?;
            let terminal_kind = ResponsesTerminalKind::from_event_type(event_type)
                .expect("matched terminal event type");
            validate_terminal_status(response, terminal_kind).map_err(invalid_web_search_stream)?;
            if terminal_kind == ResponsesTerminalKind::Completed {
                validate_web_search_snapshot_fields(response, OutputValidationPhase::Done)?;
            }
            state.terminal_response = Some(response.clone());
            state.terminal_kind = Some(terminal_kind);
        }
        "response.output_item.added" | "response.output_item.done" => {
            let output_index = required_nonnegative_event_index(event, "output_index")?;
            let item = event
                .get("item")
                .filter(|item| item.is_object())
                .ok_or_else(|| {
                    invalid_web_search_stream("an output item event had an invalid item object")
                })?;
            let phase = if event_type == "response.output_item.added" {
                OutputValidationPhase::Added
            } else {
                OutputValidationPhase::Done
            };
            parse_and_validate_output_item(item, phase).map_err(invalid_web_search_stream)?;
            let lifecycle = state.output_items_by_index.entry(output_index).or_default();
            if phase == OutputValidationPhase::Added {
                if lifecycle.added.as_ref().is_some_and(|added| added != item)
                    || lifecycle.done.is_some()
                {
                    return Err(invalid_web_search_stream(
                        "an output index was reused by a conflicting added item",
                    ));
                }
                lifecycle.added = Some(item.clone());
            } else {
                if lifecycle.done.as_ref().is_some_and(|done| done != item) {
                    return Err(invalid_web_search_stream(
                        "an output index was reused by a conflicting done item",
                    ));
                }
                if let Some(added) = lifecycle.added.as_ref() {
                    validate_output_item_reconciliation(added, item)
                        .map_err(invalid_web_search_stream)?;
                }
                lifecycle.done = Some(item.clone());
            }
        }
        "response.output_text.delta" => {
            let delta = event.get("delta").and_then(Value::as_str).ok_or_else(|| {
                invalid_web_search_stream("response.output_text.delta had an invalid delta")
            })?;
            let part = get_or_create_output_text_part(event, state)?;
            if part.done_text.is_some() {
                return Err(invalid_web_search_stream(
                    "response.output_text.delta arrived after output_text.done",
                ));
            }
            part.text.push_str(delta);
        }
        "response.output_text.done" => {
            let text = event.get("text").and_then(Value::as_str).ok_or_else(|| {
                invalid_web_search_stream("response.output_text.done had invalid text")
            })?;
            let part = get_or_create_output_text_part(event, state)?;
            if part.done_text.as_deref().is_some_and(|done| done != text) {
                return Err(invalid_web_search_stream(
                    "conflicting response.output_text.done events were emitted",
                ));
            }
            if !text.starts_with(&part.text) {
                return Err(invalid_web_search_stream(
                    "response.output_text.done conflicted with streamed text",
                ));
            }
            part.text = text.to_string();
            part.done_text = Some(text.to_string());
        }
        "response.output_text.annotation.added" => {
            let annotation = event
                .get("annotation")
                .filter(|annotation| annotation.is_object())
                .ok_or_else(|| {
                    invalid_web_search_stream(
                        "response.output_text.annotation.added had invalid annotation",
                    )
                })?
                .clone();
            get_or_create_output_text_part(event, state)?
                .annotations
                .push(annotation);
        }
        "response.content_part.done" => collect_done_content_part(event, state)?,
        "error" => {
            // The caller turns the provider message into an AppError after this
            // collector validates the event envelope.
            if event
                .get("message")
                .is_some_and(|message| !message.is_string())
                || event.get("error").is_some_and(|error| !error.is_object())
            {
                return Err(invalid_web_search_stream(
                    "an error event had malformed error fields",
                ));
            }
        }
        _ => {}
    }
    Ok(())
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
    for annotation in collected {
        if !annotations.contains(annotation) {
            annotations.push(annotation.clone());
        }
    }
    annotations
}

/// `mergeOutputItemWithCollectedText`.
#[allow(clippy::result_large_err)]
fn merge_output_item_with_collected_text(
    output_index: i64,
    item: &Value,
    state: &WebSearchResponsesStreamCollection,
) -> Result<Value, AppError> {
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return Ok(item.clone());
    }
    let collected = collected_text_parts(output_index, state);
    if collected.is_empty() {
        return Ok(item.clone());
    }

    let mut item = item.clone();
    let mut content: Vec<Value> = item
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid_web_search_stream("a completed message had invalid content"))?;

    // `content_index` is upstream-controlled. Never fill arbitrary sparse gaps:
    // a malicious or buggy upstream could otherwise drive unbounded allocation.
    for part in &collected {
        let idx = part.content_index as usize;
        if idx > content.len() {
            return Err(invalid_web_search_stream(
                "collected text used a sparse content index",
            ));
        }
        let existing = content.get(idx).cloned();
        let existing_annotations = existing
            .as_ref()
            .and_then(|existing| existing.get("annotations"))
            .cloned();
        let (mut block, text) = match existing {
            Some(existing) => {
                let block = existing.as_object().cloned().ok_or_else(|| {
                    invalid_web_search_stream("a completed message block was not an object")
                })?;
                if block.get("type").and_then(Value::as_str) != Some("output_text") {
                    return Err(invalid_web_search_stream(
                        "collected output text conflicted with a non-text message block",
                    ));
                }
                let authoritative = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        invalid_web_search_stream("a completed output_text block had invalid text")
                    })?
                    .to_string();
                if part
                    .done_text
                    .as_deref()
                    .is_some_and(|done| done != authoritative)
                    || (part.done_text.is_none() && !authoritative.starts_with(&part.text))
                {
                    return Err(invalid_web_search_stream(
                        "completed message text conflicted with streamed text",
                    ));
                }
                (block, authoritative)
            }
            None => {
                let text = part.done_text.clone().ok_or_else(|| {
                    invalid_web_search_stream(
                        "a missing completed message block had no authoritative done text",
                    )
                })?;
                (Map::new(), text)
            }
        };
        block.insert("type".to_string(), Value::String("output_text".to_string()));
        block.insert("text".to_string(), Value::String(text));
        block.insert(
            "annotations".to_string(),
            Value::Array(merge_annotations(
                existing_annotations.as_ref(),
                &part.annotations,
            )),
        );
        if idx < content.len() {
            content[idx] = Value::Object(block);
        } else {
            content.push(Value::Object(block));
        }
    }

    if let Some(obj) = item.as_object_mut() {
        obj.insert("content".to_string(), Value::Array(content));
    }
    Ok(item)
}

/// `buildCollectedWebSearchOutput`: complete output items sorted by their
/// source index with collected text merged in.
#[allow(clippy::result_large_err)]
fn build_collected_web_search_output(
    state: &WebSearchResponsesStreamCollection,
) -> Result<Vec<Value>, AppError> {
    for part in state.text_parts_by_key.values() {
        let lifecycle = state
            .output_items_by_index
            .get(&part.output_index)
            .ok_or_else(|| {
                invalid_web_search_stream("text events referenced an unknown output index")
            })?;
        let done = lifecycle.done.as_ref().ok_or_else(|| {
            invalid_web_search_stream("text events referenced an incomplete output item")
        })?;
        if done.get("type").and_then(Value::as_str) != Some("message") {
            return Err(invalid_web_search_stream(
                "text events referenced a non-message output item",
            ));
        }
        if let (Some(event_id), Some(item_id)) = (
            part.item_id.as_deref().filter(|id| !id.is_empty()),
            done.get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty()),
        ) {
            if event_id != item_id {
                return Err(invalid_web_search_stream(
                    "text events changed their output item id",
                ));
            }
        }
    }

    let mut output = Vec::with_capacity(state.output_items_by_index.len());
    for (expected, (output_index, lifecycle)) in state.output_items_by_index.iter().enumerate() {
        if i64::try_from(expected).ok() != Some(*output_index) {
            return Err(invalid_web_search_stream(
                "output item indices were sparse or out of order",
            ));
        }
        let done = lifecycle.done.as_ref().ok_or_else(|| {
            invalid_web_search_stream(
                "the terminal arrived before an output item emitted output_item.done",
            )
        })?;
        output.push(merge_output_item_with_collected_text(
            *output_index,
            done,
            state,
        )?);
    }
    Ok(output)
}

#[allow(clippy::result_large_err)]
fn build_lifecycle_initial_output(
    state: &WebSearchResponsesStreamCollection,
) -> Result<Option<Vec<Value>>, AppError> {
    if state.output_items_by_index.is_empty() {
        return Ok(None);
    }
    let mut output = Vec::with_capacity(state.output_items_by_index.len());
    for (expected, (output_index, lifecycle)) in state.output_items_by_index.iter().enumerate() {
        if i64::try_from(expected).ok() != Some(*output_index) {
            return Err(invalid_web_search_stream(
                "output item indices were sparse or out of order",
            ));
        }
        let item = lifecycle
            .added
            .as_ref()
            .or(lifecycle.done.as_ref())
            .ok_or_else(|| invalid_web_search_stream("an output lifecycle had no item snapshot"))?;
        output.push(item.clone());
    }
    Ok(Some(output))
}

#[allow(clippy::result_large_err)]
fn validate_reconciled_response_identity(
    created: &Map<String, Value>,
    terminal: &Map<String, Value>,
) -> Result<(), AppError> {
    let created_id = created
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| invalid_web_search_stream("response.created had an invalid id"))?;
    let terminal_id = terminal
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| invalid_web_search_stream("the terminal response had an invalid id"))?;
    if created_id != terminal_id {
        return Err(invalid_web_search_stream(
            "the terminal response id conflicted with response.created",
        ));
    }
    for field in ["model", "object"] {
        let created_value = created.get(field).filter(|value| !value.is_null());
        let terminal_value = terminal.get(field).filter(|value| !value.is_null());
        if let (Some(created_value), Some(terminal_value)) = (created_value, terminal_value) {
            if created_value != terminal_value {
                return Err(invalid_web_search_stream(format_args!(
                    "the terminal response {field} conflicted with response.created"
                )));
            }
        }
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn validated_snapshot_output(
    snapshot: &Map<String, Value>,
    phase: OutputValidationPhase,
) -> Result<Option<Vec<Value>>, AppError> {
    let Some(output) = snapshot.get("output").filter(|output| !output.is_null()) else {
        return Ok(None);
    };
    let output = output
        .as_array()
        .ok_or_else(|| invalid_web_search_stream("terminal output was not an array"))?;
    for item in output {
        parse_and_validate_output_item(item, phase).map_err(invalid_web_search_stream)?;
    }
    Ok(Some(output.clone()))
}

#[allow(clippy::result_large_err)]
fn canonical_web_annotations(
    annotations: Option<&[Value]>,
) -> Result<Option<Vec<Value>>, AppError> {
    let Some(annotations) = annotations else {
        return Ok(None);
    };
    let mut seen_urls = HashSet::new();
    let mut canonical = Vec::new();
    for annotation in annotations {
        let annotation = annotation.as_object().ok_or_else(|| {
            invalid_web_search_stream("web-search message annotation was not an object")
        })?;
        let annotation_type = match annotation.get("type") {
            None | Some(Value::Null) => continue,
            Some(Value::String(annotation_type)) => annotation_type.as_str(),
            Some(_) => {
                return Err(invalid_web_search_stream(
                    "web-search message annotation type was not a string or null",
                ))
            }
        };
        if annotation_type != "url_citation" {
            continue;
        }
        let url = match annotation.get("url") {
            Some(Value::String(url)) if !url.is_empty() => url.as_str(),
            _ => {
                return Err(invalid_web_search_stream(
                    "web-search URL citation had a missing, empty, or non-string URL",
                ))
            }
        };
        if !seen_urls.insert(url.to_string()) {
            continue;
        }
        let title = match annotation.get("title") {
            None | Some(Value::Null) => url,
            Some(Value::String(title)) => title.as_str(),
            Some(_) => {
                return Err(invalid_web_search_stream(
                    "web-search URL citation title was not a string or null",
                ))
            }
        };
        canonical.push(json!({"type":"url_citation","url":url,"title":title}));
    }
    Ok((!canonical.is_empty()).then_some(canonical))
}

#[derive(Debug, Clone)]
enum WebOutputAssertion {
    Message {
        id: Option<String>,
        status: Option<String>,
        role: String,
        content: Vec<WebTextAssertion>,
    },
    WebSearchCall {
        id: Option<String>,
        status: Option<String>,
        query: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WebTextAssertion {
    block_type: String,
    text: String,
    annotations: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemFieldAuthority {
    RequiredStable,
    OptionalStable,
    ProgressStatus,
    ProgressiveContent,
    OptionalAction,
    IgnoredRaw,
}

const MESSAGE_OUTPUT_FIELD_AUTHORITY: &[(&str, ItemFieldAuthority)] = &[
    ("type", ItemFieldAuthority::RequiredStable),
    ("id", ItemFieldAuthority::OptionalStable),
    ("role", ItemFieldAuthority::RequiredStable),
    ("status", ItemFieldAuthority::ProgressStatus),
    ("content", ItemFieldAuthority::ProgressiveContent),
    ("phase", ItemFieldAuthority::IgnoredRaw),
    (
        "internal_chat_message_metadata_passthrough",
        ItemFieldAuthority::IgnoredRaw,
    ),
];

const WEB_SEARCH_OUTPUT_FIELD_AUTHORITY: &[(&str, ItemFieldAuthority)] = &[
    ("type", ItemFieldAuthority::RequiredStable),
    ("id", ItemFieldAuthority::OptionalStable),
    ("status", ItemFieldAuthority::ProgressStatus),
    ("action", ItemFieldAuthority::OptionalAction),
];

#[allow(clippy::result_large_err)]
fn parse_web_output_assertion(
    value: &Value,
    phase: OutputValidationPhase,
) -> Result<WebOutputAssertion, AppError> {
    match parse_and_validate_output_item(value, phase).map_err(invalid_web_search_stream)? {
        ResponseOutputItem::Message(message) => {
            let _authority = MESSAGE_OUTPUT_FIELD_AUTHORITY;
            let mut content = Vec::with_capacity(message.content.len());
            let raw_content = value
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    invalid_web_search_stream("web-search message content was invalid")
                })?;
            for (index, block) in message.content.into_iter().enumerate() {
                let ResponseOutputContentBlock::Text(text) = block else {
                    return Err(invalid_web_search_stream(
                        "web-search message content was unsupported",
                    ));
                };
                let raw_annotations = raw_content
                    .get(index)
                    .and_then(|block| block.get("annotations"));
                let annotations = match raw_annotations {
                    None | Some(Value::Null) => None,
                    Some(Value::Array(annotations)) => {
                        canonical_web_annotations(Some(annotations.as_slice()))?
                    }
                    Some(_) => {
                        return Err(invalid_web_search_stream(
                            "web-search message annotations were not an array or null",
                        ))
                    }
                };
                content.push(WebTextAssertion {
                    block_type: text.block_type,
                    text: text.text,
                    annotations,
                });
            }
            Ok(WebOutputAssertion::Message {
                id: message.id,
                status: message.status,
                role: message.role,
                content,
            })
        }
        ResponseOutputItem::Other(raw)
            if raw.get("type").and_then(Value::as_str) == Some("web_search_call") =>
        {
            let _authority = WEB_SEARCH_OUTPUT_FIELD_AUTHORITY;
            let id = optional_nonnull_string_field(&raw, "id")
                .map_err(invalid_web_search_stream)?
                .map(str::to_string);
            let status = optional_nonnull_string_field(&raw, "status")
                .map_err(invalid_web_search_stream)?
                .map(str::to_string);
            let query = match raw.get("action") {
                None | Some(Value::Null) => None,
                _ => Some(representable_web_search_query(&raw)?),
            };
            Ok(WebOutputAssertion::WebSearchCall { id, status, query })
        }
        _ => Err(invalid_web_search_stream(
            "web-search output contained an unsupported item",
        )),
    }
}

#[allow(clippy::result_large_err)]
fn merge_optional_item_field(
    field: &str,
    current: Option<String>,
    incoming: Option<String>,
) -> Result<Option<String>, AppError> {
    match (current, incoming) {
        (Some(current), Some(incoming)) if current != incoming => Err(invalid_web_search_stream(
            format_args!("output item {field} conflicted"),
        )),
        (Some(value), _) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

#[allow(clippy::result_large_err)]
fn merge_item_status(
    current: Option<String>,
    incoming: Option<String>,
    incoming_phase: OutputValidationPhase,
) -> Result<Option<String>, AppError> {
    match (current, incoming) {
        (None, incoming) => Ok(incoming),
        (Some(current), None)
            if current == "in_progress" && incoming_phase == OutputValidationPhase::Done =>
        {
            Ok(None)
        }
        (Some(current), None) => Ok(Some(current)),
        (Some(current), Some(incoming)) if current == incoming => Ok(Some(current)),
        (Some(current), Some(incoming))
            if current == "in_progress"
                && matches!(incoming.as_str(), "completed" | "incomplete") =>
        {
            Ok(Some(incoming))
        }
        _ => Err(invalid_web_search_stream(
            "output item status assertions conflicted",
        )),
    }
}

#[allow(clippy::result_large_err)]
fn merge_web_output_assertion(
    current: WebOutputAssertion,
    incoming: WebOutputAssertion,
    incoming_phase: OutputValidationPhase,
) -> Result<WebOutputAssertion, AppError> {
    match (current, incoming) {
        (
            WebOutputAssertion::Message {
                id,
                status,
                role,
                content,
            },
            WebOutputAssertion::Message {
                id: incoming_id,
                status: incoming_status,
                role: incoming_role,
                content: incoming_content,
            },
        ) => {
            if role != incoming_role {
                return Err(invalid_web_search_stream(
                    "web-search message role conflicted",
                ));
            }
            let content = if content == incoming_content {
                content
            } else if content.is_empty() {
                incoming_content
            } else if incoming_content.is_empty() && incoming_phase == OutputValidationPhase::Added
            {
                content
            } else {
                if content.len() != incoming_content.len() {
                    return Err(invalid_web_search_stream(
                        "web-search message content lengths conflicted",
                    ));
                }
                let mut merged = Vec::with_capacity(content.len());
                for (current, incoming) in content.into_iter().zip(incoming_content) {
                    if current.block_type != incoming.block_type {
                        return Err(invalid_web_search_stream(
                            "web-search message content type conflicted",
                        ));
                    }
                    let text = if current.text == incoming.text {
                        current.text
                    } else if current.text.is_empty() {
                        incoming.text
                    } else if incoming.text.is_empty()
                        && incoming_phase == OutputValidationPhase::Added
                    {
                        current.text
                    } else {
                        return Err(invalid_web_search_stream(
                            "web-search message text conflicted",
                        ));
                    };
                    let annotations = match (current.annotations, incoming.annotations) {
                        (Some(current), Some(incoming)) if current != incoming => {
                            return Err(invalid_web_search_stream(
                                "web-search message annotations conflicted",
                            ))
                        }
                        (Some(value), _) | (None, Some(value)) => Some(value),
                        (None, None) => None,
                    };
                    merged.push(WebTextAssertion {
                        block_type: current.block_type,
                        text,
                        annotations,
                    });
                }
                merged
            };
            Ok(WebOutputAssertion::Message {
                id: merge_optional_item_field("id", id, incoming_id)?,
                status: merge_item_status(status, incoming_status, incoming_phase)?,
                role,
                content,
            })
        }
        (
            WebOutputAssertion::WebSearchCall { id, status, query },
            WebOutputAssertion::WebSearchCall {
                id: incoming_id,
                status: incoming_status,
                query: incoming_query,
            },
        ) => Ok(WebOutputAssertion::WebSearchCall {
            id: merge_optional_item_field("id", id, incoming_id)?,
            status: merge_item_status(status, incoming_status, incoming_phase)?,
            query: merge_optional_item_field("query", query, incoming_query)?,
        }),
        _ => Err(invalid_web_search_stream(
            "web-search output item type conflicted",
        )),
    }
}

fn web_output_assertion_value(assertion: WebOutputAssertion) -> Value {
    match assertion {
        WebOutputAssertion::Message {
            id,
            status,
            role,
            content,
        } => {
            let mut value = Map::new();
            value.insert("type".to_string(), Value::String("message".to_string()));
            if let Some(id) = id {
                value.insert("id".to_string(), Value::String(id));
            }
            value.insert("role".to_string(), Value::String(role));
            if let Some(status) = status {
                value.insert("status".to_string(), Value::String(status));
            }
            value.insert(
                "content".to_string(),
                Value::Array(
                    content
                        .into_iter()
                        .map(|content| {
                            let mut block = Map::new();
                            block.insert("type".to_string(), Value::String(content.block_type));
                            block.insert("text".to_string(), Value::String(content.text));
                            if let Some(annotations) = content.annotations {
                                block.insert("annotations".to_string(), Value::Array(annotations));
                            }
                            Value::Object(block)
                        })
                        .collect(),
                ),
            );
            Value::Object(value)
        }
        WebOutputAssertion::WebSearchCall { id, status, query } => {
            let mut value = Map::new();
            value.insert(
                "type".to_string(),
                Value::String("web_search_call".to_string()),
            );
            if let Some(id) = id {
                value.insert("id".to_string(), Value::String(id));
            }
            if let Some(status) = status {
                value.insert("status".to_string(), Value::String(status));
            }
            if let Some(query) = query {
                value.insert("action".to_string(), json!({"type":"search","query":query}));
            }
            Value::Object(value)
        }
    }
}

#[allow(clippy::result_large_err)]
fn merge_web_output_snapshot(
    current: &mut Option<Vec<WebOutputAssertion>>,
    incoming: Option<Vec<Value>>,
    phase: OutputValidationPhase,
    empty_is_assertion: bool,
) -> Result<(), AppError> {
    let Some(incoming) = incoming else {
        return Ok(());
    };
    if incoming.is_empty() && !empty_is_assertion {
        return Ok(());
    }
    let incoming: Vec<WebOutputAssertion> = incoming
        .iter()
        .map(|item| parse_web_output_assertion(item, phase))
        .collect::<Result<_, _>>()?;
    let Some(existing) = current.as_mut() else {
        *current = Some(incoming);
        return Ok(());
    };
    if existing.len() != incoming.len() {
        return Err(invalid_web_search_stream(
            "web-search output snapshot lengths conflicted",
        ));
    }
    for (existing, incoming) in existing.iter_mut().zip(incoming) {
        *existing = merge_web_output_assertion(existing.clone(), incoming, phase)?;
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn validated_snapshot_usage(
    snapshot: &Map<String, Value>,
) -> Result<Option<ValidatedResponsesUsage>, AppError> {
    let Some(usage) = snapshot.get("usage").filter(|usage| !usage.is_null()) else {
        return Ok(None);
    };
    let response = json!({"usage":usage});
    let validated = validate_raw_responses_usage(&response).map_err(invalid_web_search_stream)?;
    Ok(Some(validated))
}

#[allow(clippy::result_large_err)]
fn reconcile_snapshot_value(
    created: &Map<String, Value>,
    terminal: &Map<String, Value>,
    field: &str,
) -> Result<Option<Value>, AppError> {
    let created_value = created.get(field).filter(|value| !value.is_null());
    let terminal_value = terminal.get(field).filter(|value| !value.is_null());
    match (created_value, terminal_value) {
        (Some(created), Some(terminal)) if created != terminal => Err(invalid_web_search_stream(
            format_args!("terminal {field} conflicted with response.created"),
        )),
        (Some(created), _) => Ok(Some(created.clone())),
        (None, Some(terminal)) => Ok(Some(terminal.clone())),
        (None, None) => Ok(None),
    }
}

#[allow(clippy::result_large_err)]
fn reconcile_snapshot_usage(
    created: &Map<String, Value>,
    terminal: &Map<String, Value>,
) -> Result<Option<Value>, AppError> {
    let created_usage = validated_snapshot_usage(created)?;
    let terminal_usage = validated_snapshot_usage(terminal)?;
    match (created_usage, terminal_usage) {
        (Some(created), Some(terminal)) => {
            if created.input_tokens != terminal.input_tokens
                || created.output_tokens != terminal.output_tokens
            {
                return Err(invalid_web_search_stream(
                    "terminal required usage counters conflicted with response.created",
                ));
            }
            let merge_optional = |field: &str, created: Option<i64>, terminal: Option<i64>| match (
                created, terminal,
            ) {
                (Some(created), Some(terminal)) if created != terminal => {
                    Err(invalid_web_search_stream(format_args!(
                        "terminal usage {field} conflicted with response.created"
                    )))
                }
                (Some(value), _) | (None, Some(value)) => Ok(Some(value)),
                (None, None) => Ok(None),
            };
            let cached_tokens = merge_optional(
                "cached_tokens",
                created.cached_input_tokens,
                terminal.cached_input_tokens,
            )?;
            let reasoning_tokens = merge_optional(
                "reasoning_tokens",
                created.reasoning_output_tokens,
                terminal.reasoning_output_tokens,
            )?;
            Ok(Some(merged_usage_value(
                created.input_tokens,
                created.output_tokens,
                cached_tokens,
                reasoning_tokens,
            )))
        }
        (Some(usage), None) | (None, Some(usage)) => Ok(Some(merged_usage_value(
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.reasoning_output_tokens,
        ))),
        (None, None) => Ok(None),
    }
}

fn merged_usage_value(
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
) -> Value {
    let mut usage = Map::new();
    usage.insert("input_tokens".to_string(), Value::from(input_tokens));
    if let Some(cached_tokens) = cached_tokens {
        usage.insert(
            "input_tokens_details".to_string(),
            json!({"cached_tokens":cached_tokens}),
        );
    }
    usage.insert("output_tokens".to_string(), Value::from(output_tokens));
    if let Some(reasoning_tokens) = reasoning_tokens {
        usage.insert(
            "output_tokens_details".to_string(),
            json!({"reasoning_tokens":reasoning_tokens}),
        );
    }
    usage.insert(
        "total_tokens".to_string(),
        Value::from(input_tokens + output_tokens),
    );
    Value::Object(usage)
}

#[allow(clippy::result_large_err)]
fn reconcile_web_search_output(
    created: Option<Vec<Value>>,
    terminal: Option<Vec<Value>>,
    lifecycle_initial: Option<Vec<Value>>,
    lifecycle_final: Option<Vec<Value>>,
) -> Result<Vec<Value>, AppError> {
    let mut merged = None;
    merge_web_output_snapshot(&mut merged, created, OutputValidationPhase::Added, false)?;
    merge_web_output_snapshot(
        &mut merged,
        lifecycle_initial,
        OutputValidationPhase::Added,
        true,
    )?;
    merge_web_output_snapshot(
        &mut merged,
        lifecycle_final,
        OutputValidationPhase::Done,
        true,
    )?;
    merge_web_output_snapshot(&mut merged, terminal, OutputValidationPhase::Done, true)?;
    Ok(merged
        .unwrap_or_default()
        .into_iter()
        .map(web_output_assertion_value)
        .collect())
}

/// Merge a full `response.created` object with a source-valid partial terminal.
/// Required fields come from the created response; terminal identity and any
/// authoritative fields are validated before overlaying.
#[allow(clippy::result_large_err)]
fn build_web_search_responses_stream_result(
    state: &WebSearchResponsesStreamCollection,
    requested_model: Option<&str>,
) -> Result<ResponsesResult, AppError> {
    let created = state
        .created_response
        .as_ref()
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_web_search_stream("the stream had no response.created object"))?;
    let terminal = state
        .terminal_response
        .as_ref()
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_web_search_stream("the stream had no terminal response object"))?;
    let terminal_kind = state
        .terminal_kind
        .ok_or_else(|| invalid_web_search_stream("the stream had no terminal event type"))?;
    validate_reconciled_response_identity(created, terminal)?;
    validate_terminal_status(&Value::Object(terminal.clone()), terminal_kind)
        .map_err(invalid_web_search_stream)?;

    match terminal_kind {
        ResponsesTerminalKind::Failed => {
            let message = terminal
                .get("error")
                .and_then(Value::as_object)
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .unwrap_or("The web-search Responses request failed.");
            return Err(invalid_web_search_stream(message));
        }
        ResponsesTerminalKind::Incomplete => {
            let reason = terminal
                .get("incomplete_details")
                .and_then(Value::as_object)
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
                .unwrap_or("unknown");
            return Err(invalid_web_search_stream(format_args!(
                "the web-search response was incomplete ({reason})"
            )));
        }
        ResponsesTerminalKind::Completed => {}
    }

    let created_output = validated_snapshot_output(created, OutputValidationPhase::Added)?;
    let terminal_output = validated_snapshot_output(terminal, OutputValidationPhase::Done)?;
    let lifecycle_initial = build_lifecycle_initial_output(state)?;
    let lifecycle_final = if state.output_items_by_index.is_empty() {
        None
    } else {
        Some(build_collected_web_search_output(state)?)
    };
    let output = reconcile_web_search_output(
        created_output,
        terminal_output,
        lifecycle_initial,
        lifecycle_final,
    )?;
    let usage = reconcile_snapshot_usage(created, terminal)?;
    let metadata = reconcile_snapshot_value(created, terminal, "metadata")?;
    let incomplete_details = reconcile_snapshot_value(created, terminal, "incomplete_details")?;
    let output_text = reconcile_snapshot_value(created, terminal, "output_text")?;
    let end_turn = reconcile_snapshot_value(created, terminal, "end_turn")?;
    if end_turn
        .as_ref()
        .is_some_and(|end_turn| !end_turn.is_boolean())
    {
        return Err(invalid_web_search_stream(
            "web-search end_turn assertion was not boolean or null",
        ));
    }
    let model = match reconcile_snapshot_value(created, terminal, "model")? {
        Some(model) => Some(model),
        None => requested_model
            .filter(|model| !model.trim().is_empty())
            .map(|model| Value::String(model.to_string())),
    };
    if model.is_none() {
        return Err(invalid_web_search_stream(
            "web-search response snapshots omitted model without requested model context",
        ));
    }
    let object = reconcile_snapshot_value(created, terminal, "object")?;

    let mut response = created.clone();
    for (key, value) in terminal {
        if snapshot_field_authority(key) == SnapshotFieldAuthority::IgnoredRaw
            && !value.is_null()
            && response.get(key).is_none_or(Value::is_null)
        {
            response.insert(key.clone(), value.clone());
        }
    }
    for (field, value) in [
        ("model", model),
        ("object", object),
        ("output_text", output_text),
        ("usage", usage),
        ("metadata", metadata),
        ("incomplete_details", incomplete_details),
        ("end_turn", end_turn),
    ] {
        match value {
            Some(value) => {
                response.insert(field.to_string(), value);
            }
            None => {
                response.remove(field);
            }
        }
    }
    response.insert(
        "status".to_string(),
        Value::String(terminal_kind.expected_status().to_string()),
    );
    response.insert("output".to_string(), Value::Array(output));

    let result: ResponsesResult = serde_json::from_value(Value::Object(response))
        .map_err(|error| invalid_web_search_stream(format_args!("merged response: {error}")))?;
    validate_web_search_result(&result)?;
    Ok(result)
}

/// `collectWebSearchResponsesStreamResult`: drive the upstream SSE stream to a
/// single buffered `ResponsesResult`.
pub async fn collect_web_search_responses_stream_result(
    upstream: crate::services::copilot::create_responses::ResponsesEventStream,
    error_message_prefix: &str,
    requested_model: Option<&str>,
) -> Result<ResponsesResult, AppError> {
    let mut state = WebSearchResponsesStreamCollection::default();
    let stream = upstream;
    futures_util::pin_mut!(stream);

    while let Some(item) = stream.next().await {
        let event = item
            .map_err(|e| AppError::Other(anyhow::anyhow!("Web search stream read error: {e}")))?;

        if event.event.as_deref() == Some("ping") {
            continue;
        }
        if event.data == "[DONE]" {
            break;
        }
        if event.data.is_empty() {
            continue;
        }

        let parsed: Value = serde_json::from_str(&event.data).map_err(|_| {
            invalid_web_search_stream("the provider emitted a malformed JSON event")
        })?;

        collect_web_search_responses_stream_event(&parsed, &mut state)?;

        if event_type(&parsed) == Some("error") {
            let message = get_stream_error_message(&parsed)
                .unwrap_or_else(|| format!("{error_message_prefix} failed"));
            return Err(AppError::Other(anyhow::anyhow!(message)));
        }
    }

    if state.terminal_response.is_some() {
        build_web_search_responses_stream_result(&state, requested_model)
    } else {
        Err(AppError::Other(anyhow::anyhow!(
            "{error_message_prefix} ended without a terminal event"
        )))
    }
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
            // This early-return branch invokes Copilot's native Responses API,
            // so it must honor the Copilot-specific premium-interaction gate.
            // Provider-routed web search above intentionally does not consume
            // the Copilot account's quota.
            if let Err(error) = crate::libs::premium_interactions::check_premium_interactions() {
                return Some(Err(error.into()));
            }
            let subagent_marker = parse_subagent_marker_from_first_user(&payload_value);
            let mut session_id = get_root_session_id(&payload_value, headers);
            let messages = payload_value
                .get("messages")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let request_id = generate_request_id_from_payload(messages, session_id.as_deref());
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
    )?;

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
        responses_payload,
        ResponsesRequestOptions {
            vision,
            initiator,
            subagent_marker: options.subagent_marker.as_ref(),
            request_id: &options.request_id,
            session_id: options.session_id.as_deref(),
            compact_type,
            transport,
            buffered_contract: ResponsesBufferedContract::Regular,
        },
    )
    .await?;

    let result = match upstream {
        CreateResponsesReturn::Stream(response) => {
            collect_web_search_responses_stream_result(
                response,
                "Web search responses stream",
                Some(&options.web_search_model),
            )
            .await?
        }
        CreateResponsesReturn::Result(result) => result.parsed,
        CreateResponsesReturn::CompactResult(_) => {
            return Err(crate::libs::error::HttpError::internal(
                "Web-search Responses flow unexpectedly returned a compact result",
            )
            .into())
        }
    };
    validate_web_search_result(&result)?;

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
        result
            .usage
            .as_ref()
            .and_then(|u| serde_json::to_value(u).ok())
            .as_ref(),
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
            let text = block
                .get("text")
                .cloned()
                .unwrap_or(Value::String(String::new()));
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
        assert_eq!(events[4]["content_block"]["type"], "web_search_tool_result");
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
            prepare_web_search_responses_payload(&payload, Some("gpt-5-mini"), None).unwrap();
        assert_eq!(responses_payload.model, "gpt-5-mini");
        let tools = responses_payload.tools.expect("tools present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "web_search");
        assert_eq!(tools[0]["filters"]["allowed_domains"][0], "a.com");
        assert!(responses_payload.tool_choice.is_none());
    }
}
