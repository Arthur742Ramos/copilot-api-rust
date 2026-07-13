//! Anthropic API wire vocabulary.
//!
//! Mirrors `src/routes/messages/anthropic-types.ts`. Because `preprocess` walks
//! the payload as `serde_json::Value` in place (matching the TS `{...spread}` /
//! `delete` / unknown-key-passthrough style), the primary deliverable here is:
//!
//! (a) the module CONSTANTS that `preprocess.ts` and friends reference, and
//! (b) typed structs/enums for the shapes that downstream TYPED code
//!     (translations, services) needs.
//!
//! Open shapes carry a `#[serde(flatten)] extra` map so unknown keys round-trip
//! unchanged, matching the JS passthrough semantics.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Module constants
//
// These mirror the `const` strings declared at the top of `preprocess.ts`.
// Copied EXACTLY so the in-place Value walk compares byte-identical text.
// ---------------------------------------------------------------------------

/// `preprocess.ts`: `export const TOOL_REFERENCE_TURN_BOUNDARY = "Tool loaded."`
pub const TOOL_REFERENCE_TURN_BOUNDARY: &str = "Tool loaded.";

/// `preprocess.ts`: `const SYSTEM_REMINDER_START = "<system-reminder>"`
pub const SYSTEM_REMINDER_START: &str = "<system-reminder>";

/// `preprocess.ts`: `const SYSTEM_REMINDER_END = "</system-reminder>"`
pub const SYSTEM_REMINDER_END: &str = "</system-reminder>";

/// `preprocess.ts`: `const SUBAGENT_START_HOOK_ADDITIONAL_PREFIX = "SubagentStart hook additional"`
pub const SUBAGENT_START_HOOK_ADDITIONAL_PREFIX: &str = "SubagentStart hook additional";

/// `preprocess.ts`: `const IDE_EXECUTE_CODE_TOOL = "mcp__ide__executeCode"`
pub const IDE_EXECUTE_CODE_TOOL: &str = "mcp__ide__executeCode";

/// `preprocess.ts`: `const IDE_GET_DIAGNOSTICS_TOOL = "mcp__ide__getDiagnostics"`
pub const IDE_GET_DIAGNOSTICS_TOOL: &str = "mcp__ide__getDiagnostics";

/// `preprocess.ts`: `const IDE_GET_DIAGNOSTICS_DESCRIPTION = ...`
pub const IDE_GET_DIAGNOSTICS_DESCRIPTION: &str = "Get language diagnostics from VS Code. Returns errors, warnings, information, and hints for files in the workspace.";

/// `preprocess.ts`: `const PDF_FILE_READ_PREFIX = "PDF file read:"`
pub const PDF_FILE_READ_PREFIX: &str = "PDF file read:";

/// `preprocess.ts`: `const CLAUDE_CODE_BILLING_HEADER_PREFIX = "x-anthropic-billing-header:"`
pub const CLAUDE_CODE_BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

// ---------------------------------------------------------------------------
// Cache control & content blocks
//
// The content-block hierarchy is `string | array` polymorphic and is walked as
// `Value` in `preprocess`. Downstream typed code only needs a handful of shapes
// strongly; the rest are documented `Value` aliases. Open shapes flatten extra
// keys so they round-trip.
// ---------------------------------------------------------------------------

/// `AnthropicCacheControl`: `{ type: "ephemeral"; ttl?; scope?; [key]: unknown }`.
/// Open shape — unknown keys pass through.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicCacheControl {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `AnthropicTextBlock`: `{ type: "text"; text; cache_control? }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicTextBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
}

/// `AnthropicUserContentBlock` / `AnthropicAssistantContentBlock` /
/// `AnthropicToolResultContentBlock` and the web-search blocks are all walked as
/// raw `Value` in `preprocess`. Alias kept for documentation at call sites.
pub type AnthropicContentBlock = Value;

/// `string | Array<...>` content. Modelled untagged so it round-trips either
/// form. `preprocess` still walks the underlying `Value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<Value>),
}

// ---------------------------------------------------------------------------
// Messages & request payload
// ---------------------------------------------------------------------------

/// `AnthropicInputMessage` — user/assistant/system. Open shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicInputMessage {
    pub role: String,
    pub content: Value,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `AnthropicTool` — custom or server-side tool. Open shape: server tools carry
/// a `type` and omit `input_schema`, custom tools carry `input_schema`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicTool {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_callers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_inclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `tool_choice`: `{ type: "auto"|"any"|"tool"|"none"; name? }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicToolChoice {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `thinking`: `{ type: "enabled"|"adaptive"; budget_tokens?; display? }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicThinkingConfig {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `output_config`: `{ effort? }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicOutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `metadata`: `{ user_id? }`. Open shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `AnthropicMessagesPayload` — the inbound `/v1/messages` request body.
/// Strongly-typed for the fields downstream code reads; everything else passes
/// through `extra`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicMessagesPayload {
    pub model: String,
    pub messages: Vec<AnthropicInputMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    /// `string | Array<AnthropicTextBlock>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    // Required by the Anthropic Messages API, but `/v1/messages/count_tokens`
    // legitimately omits it (you're counting input, not generating). The TS
    // original types it as `number` but does no runtime validation, so a
    // count_tokens payload without it deserializes fine there; model it as
    // optional here to match that behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<AnthropicOutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AnthropicMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// `AnthropicResponse["usage"]`. Open shape: Anthropic adds fields over time
/// (`cache_creation`, `server_tool_use`, etc.) and the non-streaming native
/// `/v1/messages` path must round-trip them unchanged, matching the streaming
/// path which forwards raw bytes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `AnthropicResponse` — the non-streaming `message` result. Open shape so any
/// top-level fields the upstream adds (e.g. `container`) survive the round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: Vec<Value>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: AnthropicUsage,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Default for AnthropicResponse {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: Vec::new(),
            model: String::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: AnthropicUsage::default(),
            extra: serde_json::Map::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Stream events
// ---------------------------------------------------------------------------

/// `message_start.message` — `Omit<AnthropicResponse, content|stop_reason|stop_sequence>`
/// with `content: []`, `stop_reason: null`, `stop_sequence: null`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessageStart {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: Vec<Value>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: AnthropicUsage,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Default for AnthropicMessageStart {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: Vec::new(),
            model: String::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: AnthropicUsage::default(),
            extra: serde_json::Map::new(),
        }
    }
}

/// `content_block_start.content_block` — text | tool_use | thinking. Walked as
/// `Value`, kept open so any of the three round-trip.
pub type AnthropicContentBlockStartBlock = Value;

/// `content_block_delta.delta` union.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
// Variant names mirror the Anthropic wire discriminants (`*_delta`) one-to-one;
// the shared postfix is intentional for fidelity with the upstream protocol.
#[allow(clippy::enum_variant_names)]
pub enum AnthropicContentBlockDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
}

/// `message_delta.delta` — `{ stop_reason?; stop_sequence? }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicMessageDeltaBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

/// `message_delta.usage` — `output_tokens` required, the rest optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicMessageDeltaUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    pub output_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `error` event body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicErrorBody {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

/// `AnthropicStreamEventData` — discriminated by `type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
// The `message_start` variant carries a full `AnthropicMessageStart` (id, model,
// usage, ...) and is inherently larger than the small delta/stop variants; this
// is a wire type, so boxing it would only add indirection for no real benefit.
#[allow(clippy::large_enum_variant)]
pub enum AnthropicStreamEventData {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessageStart },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: i64,
        content_block: AnthropicContentBlockStartBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: i64,
        delta: AnthropicContentBlockDelta,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: i64 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicMessageDeltaBody,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<AnthropicMessageDeltaUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: AnthropicErrorBody },
}

impl AnthropicStreamEventData {
    /// The wire `type` discriminant for this event, matching each variant's
    /// `#[serde(rename = ...)]`. Lets the SSE encoder set the `event:` line
    /// without serializing the event to an intermediate `Value` first.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::MessageStart { .. } => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop => "message_stop",
            Self::Ping => "ping",
            Self::Error { .. } => "error",
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming translation state
// ---------------------------------------------------------------------------

/// Per-tool-call tracking entry inside `AnthropicStreamState.tool_calls`,
/// keyed by the OpenAI tool index.
#[derive(Debug, Clone, Default)]
pub struct AnthropicStreamToolCall {
    pub id: Option<String>,
    pub name: Option<String>,
    pub anthropic_block_index: i64,
    /// Argument fragments for calls that cannot be emitted yet because another
    /// tool block is active. Anthropic content blocks are strictly sequential,
    /// while OpenAI may interleave parallel tool-call indices.
    pub buffered_arguments: Vec<String>,
    /// Full ordered argument text used to validate the terminal JSON object.
    pub arguments: String,
    /// First-delta tool/function extensions preserved on the tool_use block.
    pub extra: serde_json::Map<String, Value>,
    pub started: bool,
}

/// Source-ordered output that cannot be emitted while an Anthropic tool block
/// is open. Chat deltas can interleave text/reasoning and parallel tool calls,
/// while Anthropic content blocks must remain strictly sequential.
#[derive(Debug, Clone)]
pub enum AnthropicStreamDeferredOutput {
    Text {
        text: String,
        /// True only for ordinary `ChoiceDelta.content`. Reasoning fallbacks
        /// share the text scheduler but are not refusal-mirror authority.
        source_content: bool,
    },
    ToolCall(i64),
    ReasoningOpaque(String),
}

/// `AnthropicStreamState` — plain mutable scratch state for the streaming
/// translator. NOT a wire type (no serde).
#[derive(Debug, Clone, Default)]
pub struct AnthropicStreamState {
    /// Stable OpenAI chunk identity established by the first chunk.
    pub chat_id: Option<String>,
    pub chat_model: Option<String>,
    pub chat_created: Option<i64>,
    pub chat_service_tier: Option<Option<String>>,
    pub chat_system_fingerprint: Option<Option<String>>,
    pub chat_top_level_extras: serde_json::Map<String, Value>,
    pub chat_usage: Option<AnthropicUsage>,
    pub chat_usage_source: Option<Value>,
    pub chat_output_seen: bool,
    pub chat_refusal_text: Option<String>,
    /// Ordinary Chat `content` observed from the source, including fragments
    /// currently deferred behind tool blocks. Refusal reconciliation uses this.
    pub chat_content_seen: String,
    /// Ordinary Chat `content` for which an Anthropic text delta was actually
    /// emitted. This advances only after the event is appended.
    pub chat_content_emitted: String,
    /// Every client-visible Anthropic text delta actually emitted, including
    /// reasoning fallbacks and refusal-only suffixes.
    pub chat_text_emitted: String,
    pub chat_finish_reason: Option<String>,
    /// True after a finish chunk carried usage or one post-finish usage-only
    /// chunk was accepted. Success stays pending until [DONE]/EOF so a later
    /// chunk can still invalidate the stream.
    pub chat_terminal_usage_seen: bool,
    pub message_start_sent: bool,
    pub content_block_index: i64,
    pub content_block_open: bool,
    pub thinking_block_open: bool,
    pub pending_message_delta: Option<AnthropicStreamEventData>,
    pub deferred_output: std::collections::VecDeque<AnthropicStreamDeferredOutput>,
    pub deferred_output_bytes: usize,
    /// openAIToolIndex -> { id, name, anthropic_block_index }
    pub tool_calls: std::collections::HashMap<i64, AnthropicStreamToolCall>,
    /// First-seen order for deterministic serialization of parallel calls.
    pub tool_call_order: Vec<i64>,
    /// The one tool call whose Anthropic content block is currently streaming.
    pub active_tool_call_index: Option<i64>,
    /// Set once a terminal `message_stop` has been emitted, so the end-of-stream
    /// flush can distinguish success from an upstream that ended without a
    /// `finish_reason` and emit a terminal error instead of silently truncating.
    pub message_stop_emitted: bool,
    /// Set after either a successful `message_stop` or a terminal `error`.
    pub terminal_event_emitted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_payload_round_trips_byte_stable() {
        // preserve_order is enabled crate-wide, so insertion order is preserved.
        // Named fields serialize in struct-declaration order (stream before
        // max_tokens); flattened unknown keys follow.
        let input = r#"{"model":"claude-3-5-sonnet","messages":[{"role":"user","content":"hello"}],"stream":true,"max_tokens":1024,"custom_passthrough":{"keep":1}}"#;
        let payload: AnthropicMessagesPayload = serde_json::from_str(input).unwrap();
        let output = serde_json::to_string(&payload).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn payload_deserializes_without_max_tokens() {
        // `/v1/messages/count_tokens` omits `max_tokens` (you're counting input,
        // not generating). The TS original does no runtime validation, so the
        // field must be optional here too — otherwise count_tokens 400s.
        let input = r#"{"model":"claude-sonnet-4.6","messages":[{"role":"user","content":"hi"}]}"#;
        let payload: AnthropicMessagesPayload = serde_json::from_str(input).unwrap();
        assert_eq!(payload.max_tokens, None);
    }

    #[test]
    fn response_round_trips_unknown_usage_and_top_level_fields() {
        // The non-streaming native /v1/messages path deserializes into
        // AnthropicResponse then re-serializes. Unknown usage fields
        // (cache_creation, server_tool_use) and unknown top-level fields
        // (container) must survive unchanged.
        let input = r#"{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-opus-4.8","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":5,"cache_creation":{"ephemeral_5m_input_tokens":3},"server_tool_use":{"web_search_requests":1}},"container":{"id":"c_1"}}"#;
        let response: AnthropicResponse = serde_json::from_str(input).unwrap();
        let output = serde_json::to_string(&response).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn stream_event_round_trips() {
        let input =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let ev: AnthropicStreamEventData = serde_json::from_str(input).unwrap();
        let output = serde_json::to_string(&ev).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn event_name_matches_serde_tag() {
        // event_name() must stay in lockstep with the serialized `type` field
        // for EVERY variant; the SSE encoder relies on this to skip an
        // intermediate Value. Covering all variants catches drift if a new one
        // is added.
        let cases = [
            AnthropicStreamEventData::MessageStart {
                message: AnthropicMessageStart::default(),
            },
            AnthropicStreamEventData::ContentBlockStart {
                index: 0,
                content_block: serde_json::json!({"type": "text", "text": ""}),
            },
            AnthropicStreamEventData::ContentBlockDelta {
                index: 0,
                delta: AnthropicContentBlockDelta::TextDelta {
                    text: String::new(),
                },
            },
            AnthropicStreamEventData::ContentBlockStop { index: 0 },
            AnthropicStreamEventData::MessageDelta {
                delta: AnthropicMessageDeltaBody::default(),
                usage: None,
            },
            AnthropicStreamEventData::MessageStop,
            AnthropicStreamEventData::Ping,
            AnthropicStreamEventData::Error {
                error: AnthropicErrorBody::default(),
            },
        ];
        for ev in cases {
            let value = serde_json::to_value(&ev).unwrap();
            let tag = value.get("type").and_then(Value::as_str).unwrap();
            assert_eq!(tag, ev.event_name());
        }
    }
}
