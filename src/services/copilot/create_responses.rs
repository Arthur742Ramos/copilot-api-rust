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

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
        let tag = value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned);
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
        let tag = value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned);
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

/// Mirrors the TS `CreateResponsesReturn` union (`ResponsesResult | stream`).
/// The streaming arm is a Phase 3 placeholder.
///
/// `#[serde(untagged)]` so the `Result` arm (de)serializes as the bare
/// `ResponsesResult` wire shape rather than an externally-tagged
/// `{ "Result": ... }` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesOutcome {
    Result(Box<ResponsesResult>),
    // TODO Phase 3: replace `Box<Value>` with the real pooled-stream handle once
    // the websocket/HTTP transport is implemented.
    Stream(Box<Value>),
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
