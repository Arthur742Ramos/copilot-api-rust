//! Data model for the Copilot `/responses` API.
//!
//! Ported from services/copilot/create-responses.ts, including the HTTP and
//! pooled WebSocket transports.
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

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::libs::api_config::{
    copilot_base_url, copilot_headers, prepare_for_compact, prepare_interaction_headers, set_header,
};
use crate::libs::compact::COMPACT_REQUEST;
use crate::libs::copilot_rate_limit::log_copilot_rate_limits;
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::{client, serialize_json_body};
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
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

/// Mirrors the Responses input union and the Codex 0.144.1 `ResponseItem`
/// continuation schema at commit 44918ea10c0f99151c6710411b4322c2f5c96bea.
/// Variants this proxy actively inspects are typed; every other item remains a
/// lossless `Other(Value)`. Fields that Codex declares optional must remain
/// optional here even when a particular upstream usually supplies them.
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
                Some("compaction" | "compaction_summary") => {
                    let mut item: ResponseInputCompaction =
                        serde_json::from_value(v).map_err(serde::de::Error::custom)?;
                    // Codex accepts the legacy `compaction_summary` alias but
                    // serializes the canonical `compaction` discriminant.
                    item.item_type = "compaction".to_string();
                    ResponseInputItem::Compaction(item)
                }
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
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFunctionCallOutputItem {
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub output: FunctionCallOutputContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseToolSearchOutputItem {
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub tools: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub summary: Vec<ReasoningSummaryText>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningSummaryText {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputCompaction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub encrypted_content: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputCompactionTrigger {
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Mirrors the TS `ResponseInputContent` union.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponseInputContent {
    Text(ResponseInputText),
    Image(ResponseInputImage),
    File(ResponseInputFile),
    Other(Value),
}

impl<'de> Deserialize<'de> for ResponseInputContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text") => serde_json::from_value(value)
                .map(ResponseInputContent::Text)
                .map_err(serde::de::Error::custom),
            Some("input_image") => serde_json::from_value(value)
                .map(ResponseInputContent::Image)
                .map_err(serde::de::Error::custom),
            Some("input_file") => serde_json::from_value(value)
                .map(ResponseInputContent::File)
                .map_err(serde::de::Error::custom),
            _ => Ok(ResponseInputContent::Other(value)),
        }
    }
}

/// Covers both `input_text` and `output_text` block types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputText {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInputImage {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// Deserialize helper used by the separate GitHub token/usage compatibility
/// payloads, whose upstream contract historically returns nullable counters.
///
/// Responses output deliberately does not use this helper: coercing malformed
/// known output fields to empty strings, arrays, or zeroes can turn an invalid
/// provider response into a successful empty Anthropic turn.
pub(crate) fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Mirrors a complete Responses result. Known required output/usage fields are
/// strict; unknown keys and unknown output variants round-trip through raw maps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponsesResult {
    pub id: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub object: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub created_at: Value,
    pub model: String,
    pub output: Vec<ResponseOutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_text: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub error: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub incomplete_details: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub instructions: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub parallel_tool_calls: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub temperature: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub tool_choice: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub tools: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
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
    CustomToolCall(ResponseOutputCustomToolCall),
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
        let tag = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("output item type must be a string"))?
            .to_string();
        Ok(match tag.as_str() {
            "message" => ResponseOutputItem::Message(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            "function_call" => ResponseOutputItem::FunctionCall(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            "custom_tool_call" => ResponseOutputItem::CustomToolCall(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            "tool_search_output" => ResponseOutputItem::ToolSearchOutput(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            "tool_search_call" => ResponseOutputItem::ToolSearchCall(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            "compaction" | "compaction_summary" => {
                let mut item: ResponseOutputCompaction =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                item.item_type = "compaction".to_string();
                ResponseOutputItem::Compaction(item)
            }
            "reasoning" => ResponseOutputItem::Reasoning(
                serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ),
            _ => ResponseOutputItem::Other(value),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub content: Vec<ResponseOutputContentBlock>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub summary: Vec<ResponseReasoningBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ResponseReasoningBlock>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseReasoningBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputCustomToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub call_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputToolSearchCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub arguments: Value,
    pub execution: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputToolSearchOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub tools: Vec<Value>,
    pub execution: String,
    pub status: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputCompaction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub encrypted_content: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Mirrors the TS `ResponseOutputContentBlock` union.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponseOutputContentBlock {
    Text(ResponseOutputText),
    Refusal(ResponseOutputRefusal),
    Other(Value),
}

impl<'de> Deserialize<'de> for ResponseOutputContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value.get("type").and_then(Value::as_str) {
            Some("output_text" | "input_text") => serde_json::from_value(value)
                .map(ResponseOutputContentBlock::Text)
                .map_err(serde::de::Error::custom),
            Some("refusal") => serde_json::from_value(value)
                .map(ResponseOutputContentBlock::Refusal)
                .map_err(serde::de::Error::custom),
            _ => Ok(ResponseOutputContentBlock::Other(value)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputText {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutputRefusal {
    #[serde(rename = "type")]
    pub block_type: String,
    pub refusal: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<ResponseUsageInputDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<ResponseUsageOutputDetails>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseUsageInputDetails {
    pub cached_tokens: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseUsageOutputDetails {
    pub reasoning_tokens: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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
        /// Terminal response objects are source-valid partials. Callers merge
        /// them with `response.created` before parsing a complete result.
        response: Value,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete {
        response: Value,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.failed")]
    Failed {
        response: Value,
        #[serde(default)]
        sequence_number: i64,
    },
    #[serde(rename = "response.created")]
    Created {
        response: Value,
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

pub struct ResponsesBufferedResult {
    pub parsed: ResponsesResult,
    pub raw: Bytes,
    pub headers: HeaderMap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesCompactResult {
    pub output: Vec<ResponseOutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub struct ResponsesCompactBufferedResult {
    pub parsed: ResponsesCompactResult,
    pub raw: Bytes,
    pub headers: HeaderMap,
}

/// Buffered JSON contract selected by the public route.
///
/// Regular Responses require the complete response identity/status shape. The
/// compact endpoint deliberately uses the smaller Codex output-only contract,
/// whose valid result has no response id, model, or status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesBufferedContract {
    Regular,
    Compact,
}

impl ResponsesBufferedContract {
    pub const fn metrics_endpoint(self) -> &'static str {
        match self {
            Self::Regular => "responses",
            Self::Compact => "responses_compact",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Regular => "Responses",
            Self::Compact => "compact Responses",
        }
    }
}

pub enum ResponsesBufferedBody {
    Regular(Box<ResponsesBufferedResult>),
    Compact(Box<ResponsesCompactBufferedResult>),
}

fn safe_buffered_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut safe = HeaderMap::new();
    for name in ["x-request-id", "openai-request-id", "x-codex-turn-state"] {
        if let Some(value) = headers.get(name) {
            if let Ok(name) = axum::http::HeaderName::from_bytes(name.as_bytes()) {
                safe.insert(name, value.clone());
            }
        }
    }
    safe
}

fn validate_buffered_usage(usage: Option<&ResponseUsage>) -> Result<(), String> {
    let Some(usage) = usage else {
        return Ok(());
    };
    if usage.input_tokens < 0
        || usage.output_tokens < 0
        || usage.input_tokens.checked_add(usage.output_tokens) != Some(usage.total_tokens)
        || usage.input_tokens_details.as_ref().is_some_and(|details| {
            details.cached_tokens < 0 || details.cached_tokens > usage.input_tokens
        })
        || usage.output_tokens_details.as_ref().is_some_and(|details| {
            details.reasoning_tokens < 0 || details.reasoning_tokens > usage.output_tokens
        })
    {
        return Err("Responses usage counters were inconsistent".to_string());
    }
    Ok(())
}

fn parse_buffered_responses(
    raw: Bytes,
    headers: HeaderMap,
) -> Result<ResponsesBufferedResult, String> {
    let parsed =
        serde_json::from_slice::<ResponsesResult>(&raw).map_err(|error| error.to_string())?;
    validate_buffered_usage(parsed.usage.as_ref())?;
    Ok(ResponsesBufferedResult {
        parsed,
        raw,
        headers,
    })
}

fn parse_buffered_compact(
    raw: Bytes,
    headers: HeaderMap,
) -> Result<ResponsesCompactBufferedResult, String> {
    let parsed = serde_json::from_slice::<ResponsesCompactResult>(&raw)
        .map_err(|error| error.to_string())?;
    validate_buffered_usage(parsed.usage.as_ref())?;
    Ok(ResponsesCompactBufferedResult {
        parsed,
        raw,
        headers,
    })
}

/// Read, size-bound, validate, and retain the exact bytes of a successful
/// buffered Responses reply. Both direct Copilot and provider compact routing
/// use this function so contract, header, and sanitized 502 behavior cannot
/// drift between branches.
pub async fn read_buffered_responses_response(
    response: reqwest::Response,
    contract: ResponsesBufferedContract,
    upstream_error_message: &str,
) -> Result<ResponsesBufferedBody, HttpError> {
    if !response.status().is_success() {
        return Err(http_error_from_response(upstream_error_message, response).await);
    }
    let headers = safe_buffered_response_headers(response.headers());
    let raw = crate::libs::http::read_bytes_capped(response)
        .await
        .map_err(|error| {
            HttpError::new(
                if error.contains("too large") || error.contains("exceeded") {
                    format!(
                        "Upstream {} body exceeded the maximum allowed size.",
                        contract.description()
                    )
                } else {
                    format!(
                        "The upstream {} body could not be read.",
                        contract.description()
                    )
                },
                StatusCode::BAD_GATEWAY,
                headers.clone(),
                String::new(),
            )
        })?;

    match contract {
        ResponsesBufferedContract::Regular => {
            let parsed = parse_buffered_responses(raw, headers.clone()).map_err(|_| {
                HttpError::new(
                    "The upstream Responses body was malformed.",
                    StatusCode::BAD_GATEWAY,
                    headers,
                    String::new(),
                )
            })?;
            Ok(ResponsesBufferedBody::Regular(Box::new(parsed)))
        }
        ResponsesBufferedContract::Compact => {
            let parsed = parse_buffered_compact(raw, headers.clone()).map_err(|_| {
                HttpError::new(
                    "The upstream compact Responses body was malformed.",
                    StatusCode::BAD_GATEWAY,
                    headers,
                    String::new(),
                )
            })?;
            Ok(ResponsesBufferedBody::Compact(Box::new(parsed)))
        }
    }
}

/// Return type of [`create_responses`] / [`create_http_responses`], mirroring
/// the TS `CreateResponsesReturn = ResponsesResult | ResponsesStream`.
///
/// The streaming arm carries a decoded `SseEvent` stream so the same route code
/// drives both the HTTP (SSE-over-body) and websocket transports. It therefore
/// cannot derive `Serialize`/`Deserialize`/`Clone`.
pub enum CreateResponsesReturn {
    /// Non-streaming: the fully-buffered, parsed result.
    Result(Box<ResponsesBufferedResult>),
    /// Non-streaming output-only response from `/v1/responses/compact`.
    CompactResult(Box<ResponsesCompactBufferedResult>),
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
    pub buffered_contract: ResponsesBufferedContract,
}

/// Mirrors `createResponses` in services/copilot/create-responses.ts.
///
/// Streaming requests use the selected HTTP or pooled WebSocket transport. A
/// WebSocket handshake failure falls back to HTTP before any request frame is
/// sent; compact requests always use HTTP.
pub async fn create_responses(
    mut payload: ResponsesPayload,
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
    payload.service_tier = None;
    payload.extra.remove("service_tier");

    tracing::info!("<-- model: {}", payload.model);

    let effective_transport = if options.compact_type == Some(COMPACT_REQUEST) {
        ResponsesTransport::Http
    } else {
        options.transport
    };

    if payload.stream == Some(true) && effective_transport == ResponsesTransport::Websocket {
        match create_web_socket_responses(&payload, &st, &headers, &options).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                // The pooled engine completes its handshake before returning a
                // stream, so this failure happened before any response.create
                // frame was sent. Falling back to HTTP cannot duplicate work.
                metrics::counter!(
                    "copilot_responses_websocket_fallback_total",
                    "provider" => "copilot"
                )
                .increment(1);
                tracing::warn!(
                    "responses websocket unavailable before request send; falling back to HTTP: {error}"
                );
            }
        }
    }

    let stream = payload.stream.unwrap_or(false);
    create_http_responses(payload, &st, &options, stream).await
}

/// Build and drive the pooled websocket `/responses` transport, returning a
/// decoded SSE event stream identical in shape to the HTTP path. Mirrors
/// `prepareResponsesWebSocketRequest` + `createPooledResponsesWebSocketStream`.
///
/// Once the response.create frame has been written, this path cannot safely
/// replay an auth/transport failure: output may already have been generated.
/// Handshake failures happen before that boundary and are returned to
/// `create_responses`, which safely falls back to the HTTP path (including its
/// inline 401 refresh support).
#[allow(clippy::result_large_err)]
async fn create_web_socket_responses(
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
            connect_timeout: crate::libs::http::UPSTREAM_CONNECT_TIMEOUT,
            read_timeout: crate::libs::http::upstream_read_timeout(),
            is_terminal_chunk: is_terminal_ws_chunk,
            open_error_message: "Failed to create responses websocket".to_string(),
            stream_error_message: "Responses websocket stream error".to_string(),
            terminal_chunk_missing_message: "Responses websocket ended without a terminal response"
                .to_string(),
            unavailable_error_message: None,
        },
    )
    .await
    .map_err(|error| HttpError::internal(error.to_string()))?;

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
///
/// Auth headers are rebuilt from current state on each send attempt (via the
/// `build` closure) so the single 401-triggered inline-refresh replay carries
/// the freshly-rotated Copilot token.
async fn create_http_responses(
    payload: ResponsesPayload,
    st: &state::State,
    options: &ResponsesRequestOptions<'_>,
    stream: bool,
) -> Result<CreateResponsesReturn, HttpError> {
    let base = copilot_base_url(st);
    let body = serialize_json_body(&payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    // The owned request tree can be very large; once serialized, release it
    // before waiting on the upstream response rather than retaining both forms.
    drop(payload);
    let upstream_start = std::time::Instant::now();
    // Auth headers are rebuilt per attempt from the token the helper hands us so
    // the 401-triggered replay carries the inline-refreshed token, against which
    // the refresh decision is made (no read/build token-rotation window).
    let build = |token: &str| {
        let mut st = state::snapshot();
        st.copilot_token = Some(token.to_string());
        let mut headers: HeaderMap = copilot_headers(&st, Some(options.request_id), options.vision);
        set_header(&mut headers, "x-initiator", options.initiator);
        prepare_interaction_headers(
            options.session_id,
            options.subagent_marker.is_some(),
            &mut headers,
        );
        prepare_for_compact(&mut headers, options.compact_type);
        client()
            .post(format!("{base}/responses"))
            .headers(headers)
            .body(body.clone())
    };
    let metrics_endpoint = options.buffered_contract.metrics_endpoint();
    let response = crate::libs::token::send_copilot_with_401_retry(
        crate::libs::http::retry_endpoint::RESPONSES,
        build,
    )
    .await
    .map_err(|e| {
        crate::libs::metrics::record_upstream_request(
            metrics_endpoint,
            crate::libs::metrics::UpstreamStatus::TransportError,
            upstream_start.elapsed().as_secs_f64(),
        );
        HttpError::internal(format!("Failed to create responses: {e}"))
    })?;
    crate::libs::metrics::record_upstream_request(
        metrics_endpoint,
        crate::libs::metrics::UpstreamStatus::from_code(response.status().as_u16()),
        upstream_start.elapsed().as_secs_f64(),
    );

    log_copilot_rate_limits(response.headers());

    if stream {
        if !response.status().is_success() {
            tracing::error!("Failed to create responses");
            return Err(http_error_from_response("Failed to create responses", response).await);
        }
        Ok(CreateResponsesReturn::Stream(Box::pin(
            crate::libs::sse::events(response),
        )))
    } else {
        match read_buffered_responses_response(
            response,
            options.buffered_contract,
            "Failed to create responses",
        )
        .await?
        {
            ResponsesBufferedBody::Compact(result) => {
                Ok(CreateResponsesReturn::CompactResult(result))
            }
            ResponsesBufferedBody::Regular(result) => Ok(CreateResponsesReturn::Result(result)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                let response: ResponsesResult =
                    serde_json::from_value(response).expect("parse complete terminal response");
                assert_eq!(response.id, "resp_123");
                assert_eq!(response.output_text.as_deref(), Some("hello"));
                assert_eq!(response.output.len(), 1);
                let usage = response.usage.expect("usage present");
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.total_tokens, 15);
                assert_eq!(usage.input_tokens_details.unwrap().cached_tokens, 2);
                match &response.output[0] {
                    ResponseOutputItem::Message(msg) => {
                        assert_eq!(msg.role, "assistant");
                        match &msg.content[0] {
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
    fn canonical_partial_terminal_is_not_deserialized_as_a_complete_result() {
        let event: ResponseStreamEvent = serde_json::from_value(json!({
            "type":"response.completed",
            "sequence_number":2,
            "response":{
                "id":"resp_partial",
                "status":null,
                "object":42,
                "created_at":"ignored",
                "metadata":["ignored"],
                "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}
            }
        }))
        .expect("source-valid partial terminal event");
        match event {
            ResponseStreamEvent::Completed { response, .. } => {
                assert_eq!(response["id"], "resp_partial");
                assert!(response["status"].is_null());
                assert_eq!(response["object"], 42);
                assert!(response.get("model").is_none());
                assert!(response.get("output").is_none());
                assert!(serde_json::from_value::<ResponsesResult>(response).is_err());
            }
            other => panic!("expected completed event, got {other:?}"),
        }
    }

    #[test]
    fn ignored_complete_response_extras_do_not_get_type_gated() {
        let result: ResponsesResult = serde_json::from_value(json!({
            "id":"resp_extras",
            "model":"gpt-5.4",
            "status":"completed",
            "output":[],
            "object":null,
            "created_at":"provider-extension",
            "metadata":["ignored-by-bridge"],
            "instructions":{"ignored":true},
            "parallel_tool_calls":"ignored",
            "tools":{"ignored":true},
            "temperature":"ignored",
            "top_p":false
        }))
        .expect("client-ignored response extras remain raw");
        assert!(result.object.is_null());
        assert_eq!(result.created_at, "provider-extension");
        assert_eq!(result.parallel_tool_calls, "ignored");
        assert_eq!(result.tools["ignored"], true);
        let serialized = serde_json::to_value(result).expect("serialize raw extras");
        assert!(serialized.get("object").is_none());
        assert_eq!(serialized["created_at"], "provider-extension");
        assert_eq!(serialized["metadata"][0], "ignored-by-bridge");
    }

    #[test]
    fn buffered_native_result_retains_exact_null_and_key_order() {
        let raw = Bytes::from_static(
            br#"{"unknown_before":1,"id":"r","object":null,"created_at":null,"model":"gpt","output":[],"output_text":null,"status":"completed","usage":null,"metadata":null,"unknown_after":null}"#,
        );
        let result = parse_buffered_responses(raw.clone(), HeaderMap::new())
            .expect("parse buffered response");
        assert_eq!(result.parsed.id, "r");
        assert!(result.parsed.object.is_null());
        assert_eq!(result.raw, raw);
    }

    #[test]
    fn buffered_compact_accepts_output_only_idless_shape() {
        let raw = Bytes::from_static(
            br#"{"unknown_before":null,"output":[{"type":"compaction","id":null,"encrypted_content":"enc"}],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3},"unknown_after":{"keep":true}}"#,
        );
        let result =
            parse_buffered_compact(raw.clone(), HeaderMap::new()).expect("parse compact response");
        assert_eq!(result.parsed.output.len(), 1);
        assert!(matches!(
            &result.parsed.output[0],
            ResponseOutputItem::Compaction(item)
                if item.id.is_none() && item.encrypted_content == "enc"
        ));
        assert_eq!(result.raw, raw);
    }

    #[test]
    fn buffered_compact_rejects_wrong_known_shapes() {
        for raw in [
            br#"{"output":"wrong"}"#.as_slice(),
            br#"{"output":[{"type":"compaction"}]}"#.as_slice(),
            br#"{"output":[],"usage":{"input_tokens":"2","output_tokens":1,"total_tokens":3}}"#
                .as_slice(),
            br#"{"output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":9}}"#
                .as_slice(),
            br#"{"output":[],"usage":{"input_tokens":-1,"output_tokens":1,"total_tokens":0}}"#
                .as_slice(),
            br#"{"output":[],"usage":{"input_tokens":9223372036854775807,"output_tokens":1,"total_tokens":9223372036854775807}}"#
                .as_slice(),
            br#"{"output":[],"usage":{"input_tokens":2,"input_tokens_details":{"cached_tokens":3},"output_tokens":1,"total_tokens":3}}"#
                .as_slice(),
        ] {
            assert!(parse_buffered_compact(Bytes::copy_from_slice(raw), HeaderMap::new()).is_err());
        }
    }

    #[test]
    fn buffered_regular_rejects_inconsistent_usage() {
        let raw = Bytes::from_static(
            br#"{"id":"r","model":"gpt","output":[],"status":"completed","usage":{"input_tokens":2,"output_tokens":1,"total_tokens":9}}"#,
        );
        assert!(parse_buffered_responses(raw, HeaderMap::new()).is_err());
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
                    "status": "completed",
                    "internal_chat_message_metadata_passthrough": {
                        "turn_id": "turn_123"
                    }
                }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15,
                "input_tokens_details": {
                    "cached_tokens": 2,
                    "cache_creation_tokens": 1
                },
                "future_usage_field": 9
            },
            "parallel_tool_calls": true,
            "custom_passthrough": { "keep": "me" }
        }"#;

        let result: ResponsesResult = serde_json::from_str(raw).expect("parse result");
        assert_eq!(result.id, "resp_456");
        assert_eq!(result.model, "gpt-5-codex");
        assert_eq!(result.parallel_tool_calls, json!(true));
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
        assert_eq!(
            reser["output"][0]["internal_chat_message_metadata_passthrough"]["turn_id"],
            "turn_123"
        );
        assert_eq!(
            reser["usage"]["input_tokens_details"]["cache_creation_tokens"],
            1
        );
        assert_eq!(reser["usage"]["future_usage_field"], 9);
    }

    #[test]
    fn roundtrips_unknown_fields_on_known_input_items() {
        let raw = r#"{
            "model": "gpt-5.6-sol",
            "input": [
                {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "internal_chat_message_metadata_passthrough": {
                        "turn_id": "turn_456"
                    },
                    "content": [
                        {
                            "type": "output_text",
                            "text": "hello",
                            "annotations": [],
                            "future_content_field": true
                        }
                    ]
                },
                {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "do_thing",
                    "arguments": "{}",
                    "status": "completed",
                    "internal_chat_message_metadata_passthrough": {
                        "turn_id": "turn_456"
                    }
                }
            ]
        }"#;

        let payload: ResponsesPayload = serde_json::from_str(raw).expect("parse payload");
        let reserialized = serde_json::to_value(payload).expect("serialize payload");

        assert_eq!(reserialized["input"][0]["id"], "msg_1");
        assert_eq!(
            reserialized["input"][0]["internal_chat_message_metadata_passthrough"]["turn_id"],
            "turn_456"
        );
        assert_eq!(
            reserialized["input"][0]["content"][0]["future_content_field"],
            true
        );
        assert_eq!(reserialized["input"][1]["id"], "fc_1");
        assert_eq!(
            reserialized["input"][1]["internal_chat_message_metadata_passthrough"]["turn_id"],
            "turn_456"
        );
    }

    #[test]
    fn codex_0_144_1_optional_response_items_round_trip_without_synthetic_fields() {
        // Systematic audit of codex-rs/protocol/src/models.rs `ResponseItem` at
        // 44918ea10c0f99151c6710411b4322c2f5c96bea. Variants not modeled below
        // intentionally use `Other(Value)` and therefore round-trip unchanged;
        // the typed variants exercise every Codex-optional field that previously
        // had a stricter local representation.
        let input = json!([
            {
                "type":"reasoning",
                "summary":[],
                "content":[{"type":"reasoning_text","text":"optional extension"}]
            },
            {"type":"compaction","encrypted_content":"enc_compact"},
            {
                "type":"message",
                "role":"user",
                "content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]
            },
            {
                "type":"function_call_output",
                "call_id":"call_image",
                "output":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]
            },
            {
                "type":"tool_search_call",
                "execution":"client",
                "arguments":{"query":"tool"}
            },
            {
                "type":"tool_search_output",
                "status":"completed",
                "execution":"client",
                "tools":[]
            },
            {"type":"additional_tools","role":"developer","tools":[]},
            {
                "type":"agent_message",
                "author":"agent",
                "recipient":"user",
                "content":[{"type":"input_text","text":"hi"}]
            },
            {
                "type":"local_shell_call",
                "status":"completed",
                "action":{"type":"exec","command":["pwd"],"timeout_ms":null,"working_directory":null,"env":null,"user":null}
            },
            {
                "type":"custom_tool_call",
                "call_id":"custom_1",
                "name":"freeform",
                "input":"payload"
            },
            {
                "type":"custom_tool_call_output",
                "call_id":"custom_1",
                "output":"done"
            },
            {"type":"web_search_call"},
            {
                "type":"image_generation_call",
                "status":"completed",
                "result":"image-data"
            },
            {"type":"context_compaction"},
            {"type":"compaction_trigger"}
        ]);
        let raw = json!({"model":"gpt-5.4","input":input.clone()});
        let payload: ResponsesPayload =
            serde_json::from_value(raw).expect("all audited Codex input items parse");
        let serialized = serde_json::to_value(payload).expect("serialize audited items");
        assert_eq!(serialized["input"], input);

        let output = json!([
            {
                "type":"message",
                "role":"assistant",
                "status":"completed",
                "content":[]
            },
            {"type":"reasoning","summary":[]},
            {
                "type":"tool_search_call",
                "arguments":{"query":"tool"},
                "execution":"client"
            },
            {
                "type":"tool_search_output",
                "status":"completed",
                "execution":"client",
                "tools":[]
            },
            {"type":"compaction","encrypted_content":"enc_compact"},
            {
                "type":"local_shell_call",
                "status":"completed",
                "action":{"type":"exec","command":["pwd"]}
            },
            {
                "type":"custom_tool_call_output",
                "call_id":"custom_1",
                "output":"done"
            },
            {
                "type":"web_search_call",
                "id":"web_1",
                "status":"completed",
                "action":{"type":"search","query":"tool"}
            },
            {
                "type":"image_generation_call",
                "status":"completed",
                "result":"image-data"
            },
            {"type":"context_compaction","encrypted_content":null},
            {"type":"future_response_item","future":{"keep":true}}
        ]);
        let result: ResponsesResult = serde_json::from_value(json!({
            "id":"resp_optional",
            "object":"response",
            "model":"gpt-5.4",
            "status":"completed",
            "output":output.clone()
        }))
        .expect("all audited optional output fields parse");
        let serialized = serde_json::to_value(result).expect("serialize optional output");
        assert_eq!(serialized["output"], output);
    }

    #[test]
    fn legacy_compaction_summary_alias_canonicalizes_without_inventing_id() {
        let payload: ResponsesPayload = serde_json::from_value(json!({
            "model":"gpt-5.4",
            "input":[{
                "type":"compaction_summary",
                "encrypted_content":"enc_legacy"
            }]
        }))
        .expect("legacy Codex compaction alias parses");
        let serialized = serde_json::to_value(payload).expect("serialize canonical compaction");
        assert_eq!(serialized["input"][0]["type"], "compaction");
        assert_eq!(serialized["input"][0]["encrypted_content"], "enc_legacy");
        assert!(serialized["input"][0].get("id").is_none());
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

    // `output_text` is the one explicitly observed nullable compatibility field.
    // It stays optional rather than being coerced to an indistinguishable empty
    // string. Known required output and usage fields intentionally reject null.

    #[test]
    fn null_output_text_does_not_500() {
        // `output_text` is null on tool-use / structured turns — the field that
        // sits right after the large `output` array (~57KB into the body).
        let raw = r#"{
            "id": "resp_1",
            "object": "response",
            "created_at":1,
            "model": "claude-opus-4-8",
            "status": "completed",
            "output_text": null,
            "output": []
        }"#;
        let result: ResponsesResult = serde_json::from_str(raw).expect("null output_text parses");
        assert_eq!(result.output_text, None);
        assert_eq!(result.id, "resp_1");
    }

    #[test]
    fn null_function_call_arguments_is_rejected() {
        let raw = r#"{
            "id": "resp_2",
            "object": "response",
            "model": "claude-opus-4-8",
            "status": "completed",
            "output": [
                {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "list_files",
                    "arguments": null
                }
            ]
        }"#;
        assert!(serde_json::from_str::<ResponsesResult>(raw).is_err());
    }

    #[test]
    fn null_message_status_stays_typed() {
        let raw = r#"{
            "id": "resp_3",
            "object":"response",
            "created_at":1,
            "model":"gpt-5.4",
            "status":"completed",
            "output": [
                {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "status": null,
                    "content": [
                        { "type": "output_text", "text": "hi", "annotations": [] }
                    ]
                }
            ]
        }"#;
        let result: ResponsesResult = serde_json::from_str(raw).expect("null status parses");
        match &result.output[0] {
            ResponseOutputItem::Message(m) => {
                assert_eq!(m.status, None);
                assert_eq!(m.role, "assistant");
            }
            other => panic!("expected typed message, got {other:?}"),
        }
    }

    #[test]
    fn null_usage_token_counts_are_rejected() {
        let raw = r#"{
            "id": "resp_4",
            "model":"gpt-5.4",
            "status":"completed",
            "output": [],
            "usage": {
                "input_tokens": null,
                "output_tokens": null,
                "total_tokens": null,
                "input_tokens_details": { "cached_tokens": null },
                "output_tokens_details": { "reasoning_tokens": null }
            }
        }"#;
        assert!(serde_json::from_str::<ResponsesResult>(raw).is_err());
    }

    #[test]
    fn null_to_default_helper_handles_string_array_and_int() {
        // The helper is generic: null coerces to T::default() for every type it
        // guards (string -> "", Vec -> [], i64 -> 0), while real values pass
        // through unchanged.
        #[derive(Deserialize)]
        struct Probe {
            #[serde(default, deserialize_with = "null_to_default")]
            s: String,
            #[serde(default, deserialize_with = "null_to_default")]
            v: Vec<i64>,
            #[serde(default, deserialize_with = "null_to_default")]
            n: i64,
        }
        let nulls: Probe = serde_json::from_str(r#"{"s":null,"v":null,"n":null}"#).unwrap();
        assert_eq!(nulls.s, "");
        assert!(nulls.v.is_empty());
        assert_eq!(nulls.n, 0);

        let vals: Probe = serde_json::from_str(r#"{"s":"x","v":[1,2],"n":7}"#).unwrap();
        assert_eq!(vals.s, "x");
        assert_eq!(vals.v, vec![1, 2]);
        assert_eq!(vals.n, 7);
    }
}
