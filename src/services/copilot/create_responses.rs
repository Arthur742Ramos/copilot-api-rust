//! Data model for the Copilot `/responses` API.
//!
//! Ported from services/copilot/create-responses.ts (type/interface
//! declarations). This module is TYPES ONLY: the network/transport
//! implementation (HTTP + pooled websocket streaming) lands in a later phase.
//!
//! Conventions match the rest of the crate:
//! - serde_json has `preserve_order`, so unknown keys captured via
//!   `#[serde(flatten)] extra` preserve their order *relative to each other*.
//!   Note that serde emits all declared struct fields first and the flattened
//!   `extra` map afterwards, so key order is not preserved relative to known
//!   fields — only within `extra`.
//! - Anthropic/Responses payloads are walked as loosely-typed `Value` where the
//!   shape is a union; typed structs are used where a known shape helps.
//! - All optionals use `#[serde(skip_serializing_if = "Option::is_none")]`.

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::libs::api_config::{
    copilot_base_url, copilot_headers, prepare_for_compact, prepare_interaction_headers, set_header,
};
use crate::libs::compact::COMPACT_REQUEST;
use crate::libs::copilot_rate_limit::log_copilot_rate_limits;
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::client;
use crate::libs::state;
use crate::libs::subagent::SubagentMarker;

// ---------------------------------------------------------------------------
// Request payload
// ---------------------------------------------------------------------------

/// Mirrors the TS `ResponsesPayload` interface. `model` is required; every
/// other documented field is optional, and unknown keys flow through `extra`
/// so the body round-trips unchanged to the upstream API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesPayload {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<InputField>,
    /// The `Tool` union is kept as raw `Value`s (FunctionTool / ToolSearchTool /
    /// NamespaceTool / unknown object) so tools pass through untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// NOTE: Unsupported by GitHub Copilot (stripped before sending upstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Round-trips unknown keys (the TS `[key: string]: unknown` index sig).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The `input` field is either a bare prompt string or an array of input items.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputField {
    Text(String),
    Items(Vec<ResponseInputItem>),
}

// ---------------------------------------------------------------------------
// Input items
// ---------------------------------------------------------------------------

/// Mirrors the TS `ResponseInputItem` union. Untagged: serde tries each variant
/// structurally in order, falling back to `Other` for unknown shapes. Each
/// known struct keeps its discriminating fields required so the match is stable.
///
/// Deserialization dispatches on the `type` discriminant (see the manual impl
/// below) rather than relying on untagged structural matching: `Compaction`'s
/// fields are a structural subset of `Reasoning`'s, so untagged matching would
/// mis-route reasoning items. `Serialize` stays untagged (emits the inner shape).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponseInputItem {
    Message(ResponseInputMessage),
    FunctionToolCall(ResponseFunctionToolCallItem),
    FunctionCallOutput(ResponseFunctionCallOutputItem),
    ToolSearchCall(ResponseToolSearchCallItem),
    ToolSearchOutput(ResponseToolSearchOutputItem),
    Reasoning(ResponseInputReasoning),
    Compaction(ResponseInputCompaction),
    CompactionTrigger(ResponseInputCompactionTrigger),
    Other(Value),
}

impl<'de> Deserialize<'de> for ResponseInputItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let tag = value.get("type").and_then(Value::as_str).map(str::to_owned);
        let from = |v: Value| -> Result<Self, D::Error> {
            // input messages may omit `type`; everything else carries it.
            Ok(match tag.as_deref() {
                Some("function_call") => ResponseInputItem::FunctionToolCall(
                    serde_json::from_value(v).map_err(serde::de::Error::custom)?,
                ),
                Some("function_call_output") => ResponseInputItem::FunctionCallOutput(
                    serde_json::from_value(v).map_err(serde::de::Error::custom)?,
                ),
                Some("tool_search_call") => ResponseInputItem::ToolSearchCall(
                    serde_json::from_value(v).map_err(serde::de::Error::custom)?,
                ),
                Some("tool_search_output") => ResponseInputItem::ToolSearchOutput(
                    serde_json::from_value(v).map_err(serde::de::Error::custom)?,
                ),
                Some("reasoning") => ResponseInputItem::Reasoning(
                    serde_json::from_value(v).map_err(serde::de::Error::custom)?,
                ),
                Some("compaction") => ResponseInputItem::Compaction(
                    serde_json::from_value(v).map_err(serde::de::Error::custom)?,
                ),
                Some("compaction_trigger") => ResponseInputItem::CompactionTrigger(
                    serde_json::from_value(v).map_err(serde::de::Error::custom)?,
                ),
                Some("message") | None => ResponseInputItem::Message(
                    serde_json::from_value(v).map_err(serde::de::Error::custom)?,
                ),
                Some(_) => ResponseInputItem::Other(v),
            })
        };
        from(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputMessage {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

/// A message `content` is either bare text or an array of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ResponseInputContent>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFunctionToolCallItem {
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFunctionCallOutputItem {
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub output: FunctionCallOutputContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// `output` is either a bare string or an array of input content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FunctionCallOutputContent {
    Text(String),
    Blocks(Vec<ResponseInputContent>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseToolSearchCallItem {
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseToolSearchOutputItem {
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub tools: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub summary: Vec<ReasoningSummaryText>,
    pub encrypted_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningSummaryText {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputCompaction {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub encrypted_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputCompactionTrigger {
    #[serde(rename = "type")]
    pub item_type: String,
}

/// Mirrors the TS `ResponseInputContent` union.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseInputContent {
    Text(ResponseInputText),
    Image(ResponseInputImage),
    File(ResponseInputFile),
    Other(Value),
}

/// Covers both `input_text` and `output_text` block types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputText {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputImage {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputFile {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// Mirrors the TS `ResponsesResult`. Fields default-lenient so embedded results
/// inside stream events deserialize even when partial; unknown keys round-trip
/// through `extra`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponsesResult {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub output: Vec<ResponseOutputItem>,
    #[serde(default)]
    pub output_text: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
    #[serde(default)]
    pub error: Value,
    #[serde(default)]
    pub incomplete_details: Value,
    #[serde(default)]
    pub instructions: Value,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub temperature: Value,
    #[serde(default)]
    pub tool_choice: Value,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub top_p: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Output items
// ---------------------------------------------------------------------------

/// Mirrors the TS `ResponseOutputItem` union. `Serialize` is untagged;
/// `Deserialize` dispatches on the `type` discriminant (manual impl below)
/// because `Compaction`'s fields are a structural subset of `Reasoning`'s.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponseOutputItem {
    Message(ResponseOutputMessage),
    FunctionCall(ResponseOutputFunctionCall),
    ToolSearchOutput(ResponseOutputToolSearchOutput),
    ToolSearchCall(ResponseOutputToolSearchCall),
    Compaction(ResponseOutputCompaction),
    Reasoning(ResponseOutputReasoning),
    Other(Value),
}

impl<'de> Deserialize<'de> for ResponseOutputItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let tag = value.get("type").and_then(Value::as_str).map(str::to_owned);
        Ok(match tag.as_deref() {
            Some("message") => ResponseOutputItem::Message(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            Some("function_call") => ResponseOutputItem::FunctionCall(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            Some("tool_search_output") => ResponseOutputItem::ToolSearchOutput(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            Some("tool_search_call") => ResponseOutputItem::ToolSearchCall(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            Some("compaction") => ResponseOutputItem::Compaction(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            Some("reasoning") => ResponseOutputItem::Reasoning(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            _ => ResponseOutputItem::Other(value),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub role: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ResponseOutputContentBlock>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputReasoning {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Vec<ResponseReasoningBlock>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseReasoningBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputFunctionCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputToolSearchCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputToolSearchOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub tools: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputCompaction {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub encrypted_content: String,
}

/// Mirrors the TS `ResponseOutputContentBlock` union.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseOutputContentBlock {
    Text(ResponseOutputText),
    Refusal(ResponseOutputRefusal),
    Other(Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputText {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    pub annotations: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputRefusal {
    #[serde(rename = "type")]
    pub block_type: String,
    pub refusal: String,
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseUsage {
    pub input_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    pub total_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<ResponseUsageInputDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<ResponseUsageOutputDetails>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseUsageInputDetails {
    pub cached_tokens: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseUsageOutputDetails {
    pub reasoning_tokens: i64,
}

// ---------------------------------------------------------------------------
// Stream events
// ---------------------------------------------------------------------------

/// Mirrors the TS `ResponseStreamEvent` union. Internally tagged on the `type`
/// string; only the events the handler cares about are modeled, with `Unknown`
/// as the fallback for any other event type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseStreamEvent {
    #[serde(rename = "response.completed")]
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        copilot_quota_snapshots: Option<Value>,
        response: ResponsesResult,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete {
        response: ResponsesResult,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.failed")]
    Failed {
        response: ResponsesResult,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.created")]
    Created {
        response: ResponsesResult,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "error")]
    Error(ResponseErrorEvent),
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        item: ResponseOutputItem,
        #[serde(default)]
        output_index: i64,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        item: ResponseOutputItem,
        #[serde(default)]
        output_index: i64,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        delta: String,
        item_id: String,
        #[serde(default)]
        output_index: i64,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        arguments: String,
        item_id: String,
        name: String,
        #[serde(default)]
        output_index: i64,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        delta: String,
        item_id: String,
        #[serde(default)]
        output_index: i64,
        #[serde(default)]
        sequence_number: i64,
        #[serde(default)]
        summary_index: i64,
    },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {
        item_id: String,
        text: String,
        #[serde(default)]
        output_index: i64,
        #[serde(default)]
        sequence_number: i64,
        #[serde(default)]
        summary_index: i64,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        delta: String,
        item_id: String,
        #[serde(default)]
        content_index: i64,
        #[serde(default)]
        output_index: i64,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        item_id: String,
        text: String,
        #[serde(default)]
        content_index: i64,
        #[serde(default)]
        output_index: i64,
        #[serde(default)]
        sequence_number: i64,
    },
    /// Fallback for any other event type the handler does not special-case.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseErrorEvent {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default)]
    pub sequence_number: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseErrorEventError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseErrorEventError {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: String,
}

// ---------------------------------------------------------------------------
// Transport / outcome
// ---------------------------------------------------------------------------

/// Mirrors the TS `ResponsesTransport` string union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ResponsesTransport {
    #[default]
    #[serde(rename = "http")]
    Http,
    #[serde(rename = "websocket")]
    Websocket,
}

/// A boxed stream of decoded SSE events, produced by either the HTTP transport
/// (parsing the upstream response body) or the pooled websocket transport.
pub type ResponsesEventStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<crate::libs::sse::SseEvent, std::io::Error>> + Send>,
>;

/// Return type of [`create_responses`] / [`create_http_responses`], mirroring
/// the TS `CreateResponsesReturn = ResponsesResult | ResponsesStream`.
///
/// The streaming arm carries a decoded `SseEvent` stream so the same route code
/// drives both the HTTP (SSE-over-body) and websocket transports. It therefore
/// cannot derive `Serialize`/`Deserialize`/`Clone`.
pub enum CreateResponsesReturn {
    /// Non-streaming: the fully-buffered, parsed result.
    Result(Box<ResponsesResult>),
    /// Streaming: decoded SSE events from the chosen transport.
    Stream(ResponsesEventStream),
}

// ---------------------------------------------------------------------------
// Transport (HTTP)
// ---------------------------------------------------------------------------

/// Options for [`create_responses`], mirroring the TS `ResponsesRequestOptions`.
pub struct ResponsesRequestOptions<'a> {
    pub vision: bool,
    /// `"agent"` or `"user"`.
    pub initiator: &'a str,
    pub subagent_marker: Option<&'a SubagentMarker>,
    pub request_id: &'a str,
    pub session_id: Option<&'a str>,
    pub compact_type: Option<i32>,
    pub transport: ResponsesTransport,
}

/// Mirrors `createResponses` in services/copilot/create-responses.ts.
///
/// HTTP transport only. The websocket transport is Phase 5; where the TS code
/// branches to a pooled websocket stream we fall back to HTTP (see below).
pub async fn create_responses(
    payload: &ResponsesPayload,
    options: ResponsesRequestOptions<'_>,
) -> Result<CreateResponsesReturn, HttpError> {
    let st = state::snapshot();
    if st.copilot_token.as_deref().unwrap_or("").is_empty() {
        return Err(HttpError::internal("Copilot token not found"));
    }

    let mut headers: HeaderMap = copilot_headers(&st, Some(options.request_id), options.vision);
    set_header(&mut headers, "x-initiator", options.initiator);

    prepare_interaction_headers(
        options.session_id,
        options.subagent_marker.is_some(),
        &mut headers,
    );

    prepare_for_compact(&mut headers, options.compact_type);

    // service_tier is not supported by github copilot: strip it before sending.
    let mut payload = payload.clone();
    payload.service_tier = None;
    payload.extra.remove("service_tier");

    tracing::info!("<-- model: {}", payload.model);

    let effective_transport = if options.compact_type == Some(COMPACT_REQUEST) {
        ResponsesTransport::Http
    } else {
        options.transport
    };

    if payload.stream == Some(true) && effective_transport == ResponsesTransport::Websocket {
        return create_web_socket_responses(&payload, &st, &headers, &options);
    }

    let stream = payload.stream.unwrap_or(false);
    create_http_responses(&payload, &st, headers, stream).await
}

/// Build and drive the pooled websocket `/responses` transport, returning a
/// decoded SSE event stream identical in shape to the HTTP path. Mirrors
/// `prepareResponsesWebSocketRequest` + `createPooledResponsesWebSocketStream`.
#[allow(clippy::result_large_err)]
fn create_web_socket_responses(
    payload: &ResponsesPayload,
    st: &state::State,
    headers: &HeaderMap,
    options: &ResponsesRequestOptions<'_>,
) -> Result<CreateResponsesReturn, HttpError> {
    use crate::services::responses_websocket::{
        create_pooled_web_socket_stream, create_web_socket_url, PooledWebSocketRequest,
        PooledWebSocketStreamOptions,
    };

    // Headers: the websocket handshake reuses the prepared HTTP headers minus
    // `x-initiator` (the initiator is carried in the payload instead).
    let ws_headers: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| name.as_str() != "x-initiator")
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect();

    // Pool key: token fingerprint | model | request id | subagent key.
    let token_fingerprint = match st.copilot_token.as_deref() {
        Some(token) if !token.is_empty() => {
            let digest = Sha256::digest(token.as_bytes());
            hex::encode(digest)[..16].to_string()
        }
        _ => "missing-token".to_string(),
    };
    let subagent_key = options
        .subagent_marker
        .map(|m| format!("{}:{}:{}", m.session_id, m.agent_id, m.agent_type))
        .unwrap_or_else(|| "main".to_string());
    let pool_key = [
        token_fingerprint,
        payload.model.clone(),
        options.request_id.to_string(),
        subagent_key,
    ]
    .join("|");

    // Payload: response.create envelope with the initiator, minus stream/
    // background/service_tier.
    let mut ws_payload =
        serde_json::to_value(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    if let Some(obj) = ws_payload.as_object_mut() {
        obj.insert("type".to_string(), Value::String("response.create".into()));
        obj.insert(
            "initiator".to_string(),
            Value::String(options.initiator.to_string()),
        );
        obj.remove("stream");
        obj.remove("background");
        obj.remove("service_tier");
    }

    let base = copilot_base_url(st);
    let url = create_web_socket_url(&format!("{}/responses", base.trim_end_matches('/')));

    let request = PooledWebSocketRequest {
        headers: ws_headers,
        payload: ws_payload,
        pool_key,
        url,
    };

    let stream = create_pooled_web_socket_stream(
        request,
        PooledWebSocketStreamOptions {
            create_chunk: ws_chunk_from_data,
            idle_timeout_ms: None,
            read_timeout_ms: None,
            is_terminal_chunk: is_terminal_ws_chunk,
            open_error_message: "Failed to create responses websocket".to_string(),
            stream_error_message: "Responses websocket stream error".to_string(),
            terminal_chunk_missing_message: "Responses websocket ended without a terminal response"
                .to_string(),
            unavailable_error_message: None,
        },
    );

    Ok(CreateResponsesReturn::Stream(Box::pin(stream)))
}

/// Turn one websocket message payload into an `SseEvent`, lifting the event type
/// out of the JSON `type` field so downstream translation sees the same shape as
/// the HTTP SSE transport.
fn ws_chunk_from_data(data: String) -> crate::libs::sse::SseEvent {
    let (event, id) = serde_json::from_str::<Value>(&data)
        .ok()
        .map(|parsed| {
            let event = parsed
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let id = parsed.get("id").and_then(Value::as_str).map(str::to_owned);
            (event, id)
        })
        .unwrap_or((None, None));
    crate::libs::sse::SseEvent { id, event, data }
}

/// Terminal websocket chunk detector: completed / failed / incomplete / error.
fn is_terminal_ws_chunk(chunk: &crate::libs::sse::SseEvent) -> bool {
    matches!(
        chunk.event.as_deref(),
        Some("response.completed")
            | Some("response.failed")
            | Some("response.incomplete")
            | Some("error")
    )
}

/// Mirrors `createHttpResponses`: POST `{copilotBaseUrl}/responses`, log rate
/// limits, error on non-2xx, then either hand back the streaming response or
/// parse the buffered JSON body.
async fn create_http_responses(
    payload: &ResponsesPayload,
    st: &state::State,
    headers: HeaderMap,
    stream: bool,
) -> Result<CreateResponsesReturn, HttpError> {
    let base = copilot_base_url(st);
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    let upstream_start = std::time::Instant::now();
    let request = client()
        .post(format!("{base}/responses"))
        .headers(headers)
        .body(body);
    let response = crate::libs::http::send_with_retry(
        request,
        crate::libs::http::retry_endpoint::RESPONSES,
        crate::libs::http::RetryPolicy::from_env(),
    )
    .await
    .map_err(|e| {
        crate::libs::metrics::record_upstream_request(
            "responses",
            crate::libs::metrics::UpstreamStatus::TransportError,
            upstream_start.elapsed().as_secs_f64(),
        );
        HttpError::internal(format!("Failed to create responses: {e}"))
    })?;
    crate::libs::metrics::record_upstream_request(
        "responses",
        crate::libs::metrics::UpstreamStatus::from_code(response.status().as_u16()),
        upstream_start.elapsed().as_secs_f64(),
    );

    log_copilot_rate_limits(response.headers());

    if !response.status().is_success() {
        tracing::error!("Failed to create responses");
        return Err(http_error_from_response("Failed to create responses", response).await);
    }

    if stream {
        Ok(CreateResponsesReturn::Stream(Box::pin(
            crate::libs::sse::events(response),
        )))
    } else {
        let result = crate::libs::http::read_json_capped::<ResponsesResult>(response)
            .await
            .map_err(|e| HttpError::internal(format!("Failed to parse responses: {e}")))?;
        Ok(CreateResponsesReturn::Result(Box::new(result)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_response_completed_stream_event() {
        let raw = r#"{
            "type": "response.completed",
            "sequence_number": 7,
            "response": {
                "id": "resp_123",
                "object": "response",
                "created_at": 1700000000,
                "model": "gpt-5",
                "status": "completed",
                "output_text": "hello",
                "output": [
                    {
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [
                            { "type": "output_text", "text": "hello", "annotations": [] }
                        ]
                    }
                ],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "total_tokens": 15,
                    "input_tokens_details": { "cached_tokens": 2 },
                    "output_tokens_details": { "reasoning_tokens": 3 }
                },
                "error": null
            }
        }"#;

        let event: ResponseStreamEvent = serde_json::from_str(raw).expect("parse event");
        match event {
            ResponseStreamEvent::Completed {
                response,
                sequence_number,
                ..
            } => {
                assert_eq!(sequence_number, 7);
                assert_eq!(response.id, "resp_123");
                assert_eq!(response.output_text, "hello");
                assert_eq!(response.output.len(), 1);
                let usage = response.usage.expect("usage present");
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.total_tokens, 15);
                assert_eq!(usage.input_tokens_details.unwrap().cached_tokens, 2);
                match &response.output[0] {
                    ResponseOutputItem::Message(msg) => {
                        assert_eq!(msg.role, "assistant");
                        let content = msg.content.as_ref().expect("content");
                        match &content[0] {
                            ResponseOutputContentBlock::Text(t) => assert_eq!(t.text, "hello"),
                            other => panic!("expected output_text, got {other:?}"),
                        }
                    }
                    other => panic!("expected message output item, got {other:?}"),
                }
            }
            other => panic!("expected completed event, got {other:?}"),
        }
    }

    #[test]
    fn roundtrips_responses_result() {
        let raw = r#"{
            "id": "resp_456",
            "object": "response",
            "created_at": 1700000123,
            "model": "gpt-5-codex",
            "status": "completed",
            "output_text": "done",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "do_thing",
                    "arguments": "{}",
                    "status": "completed"
                }
            ],
            "parallel_tool_calls": true,
            "custom_passthrough": { "keep": "me" }
        }"#;

        let result: ResponsesResult = serde_json::from_str(raw).expect("parse result");
        assert_eq!(result.id, "resp_456");
        assert_eq!(result.model, "gpt-5-codex");
        assert_eq!(result.parallel_tool_calls, Some(true));
        assert_eq!(result.output.len(), 1);
        match &result.output[0] {
            ResponseOutputItem::FunctionCall(fc) => {
                assert_eq!(fc.call_id, "call_1");
                assert_eq!(fc.name, "do_thing");
            }
            other => panic!("expected function_call, got {other:?}"),
        }
        // Unknown key round-trips through `extra`.
        assert!(result.extra.contains_key("custom_passthrough"));

        let reser = serde_json::to_value(&result).expect("serialize");
        assert_eq!(reser["custom_passthrough"]["keep"], "me");
    }

    #[test]
    fn transport_serde() {
        assert_eq!(
            serde_json::to_string(&ResponsesTransport::Websocket).unwrap(),
            "\"websocket\""
        );
        let t: ResponsesTransport = serde_json::from_str("\"http\"").unwrap();
        assert_eq!(t, ResponsesTransport::Http);
    }
}
