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
}

/// `output_config`: `{ effort? }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicOutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
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
    pub max_tokens: i64,
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

/// `AnthropicResponse["usage"]`.
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
}

/// `AnthropicResponse` — the non-streaming `message` result.
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

// ---------------------------------------------------------------------------
// Streaming translation state
// ---------------------------------------------------------------------------

/// Per-tool-call tracking entry inside `AnthropicStreamState.tool_calls`,
/// keyed by the OpenAI tool index.
#[derive(Debug, Clone, Default)]
pub struct AnthropicStreamToolCall {
    pub id: String,
    pub name: String,
    pub anthropic_block_index: i64,
}

/// `AnthropicStreamState` — plain mutable scratch state for the streaming
/// translator. NOT a wire type (no serde).
#[derive(Debug, Clone, Default)]
pub struct AnthropicStreamState {
    pub message_start_sent: bool,
    pub content_block_index: i64,
    pub content_block_open: bool,
    pub thinking_block_open: bool,
    pub pending_message_delta: Option<AnthropicStreamEventData>,
    pub deferred_content: Option<String>,
    /// openAIToolIndex -> { id, name, anthropic_block_index }
    pub tool_calls: std::collections::HashMap<i64, AnthropicStreamToolCall>,
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
    fn stream_event_round_trips() {
        let input =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let ev: AnthropicStreamEventData = serde_json::from_str(input).unwrap();
        let output = serde_json::to_string(&ev).unwrap();
        assert_eq!(input, output);
    }
}
