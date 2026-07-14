//! Port of `src/routes/messages/responses-translation.ts`.
//!
//! Two main exports:
//! - [`translate_anthropic_messages_to_responses_payload`] — Anthropic Messages
//!   request -> OpenAI Responses request.
//! - [`translate_responses_result_to_anthropic`] — Responses output -> Anthropic
//!   message response.
//!
//! Polymorphic Anthropic content (`string | array`) is read as
//! `serde_json::Value`; fixed shapes use the typed structs from
//! `create_responses` / `anthropic_types`.
//!
//! Legacy complete reasoning/compaction carriers reproduce the TS format
//! byte-for-byte. Codex 0.144.1 also permits missing reasoning fields and
//! id-less compactions; versioned reasoning JSON carriers and the compaction
//! trailing-separator extension preserve those combinations across Messages.

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::libs::config::{get_extra_prompt_for_model, get_reasoning_effort_for_model};
use crate::libs::error::AppError;
use crate::libs::request_context::request_context_store;
use crate::libs::tool_search::{
    format_tool_search_bridge_arguments, is_bridge_tool_search_name, is_deferred_tool_name,
    list_deferred_tool_names, normalize_tool_search_bridge_arguments,
    should_enable_responses_tool_search, BRIDGE_TOOL_SEARCH_NAME,
};
use crate::libs::utils::parse_user_id_metadata;
use crate::routes::messages::anthropic_types::{
    AnthropicInputMessage, AnthropicMessagesPayload, AnthropicResponse, AnthropicTool,
    AnthropicUsage,
};
use crate::routes::messages::request_validation::{
    collect_open_object_extensions, merge_open_object_extensions, ANTHROPIC_TOOL_KNOWN_FIELDS,
};
use crate::services::copilot::create_responses::{
    FunctionCallOutputContent, InputField, MessageContent, ReasoningSummaryText,
    ResponseFunctionCallOutputItem, ResponseFunctionToolCallItem, ResponseInputCompaction,
    ResponseInputContent, ResponseInputFile, ResponseInputImage, ResponseInputItem,
    ResponseInputMessage, ResponseInputReasoning, ResponseInputText, ResponseOutputContentBlock,
    ResponseOutputItem, ResponseToolSearchCallItem, ResponseToolSearchOutputItem, ResponseUsage,
    ResponsesPayload, ResponsesResult,
};

const MESSAGE_TYPE: &str = "message";
const RESPONSES_MESSAGE_CANONICAL_FIELDS: &[&str] = &["type", "role", "content", "status", "phase"];
const RESPONSES_REQUEST_CANONICAL_FIELDS: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "temperature",
    "top_p",
    "max_output_tokens",
    "metadata",
    "stream",
    "safety_identifier",
    "prompt_cache_key",
    "prompt_cache_retention",
    "parallel_tool_calls",
    "store",
    "reasoning",
    "context_management",
    "include",
    "service_tier",
    // Not a Responses field, but accepting this extension would silently
    // bypass the explicit `stop_sequences` unsupported-control policy.
    "stop",
];
const COMPACTION_SIGNATURE_PREFIX: &str = "cm1#";
const COMPACTION_SIGNATURE_SEPARATOR: &str = "@";
const OPTIONAL_REASONING_SIGNATURE_PREFIX: &str = "rs1#";
/// Semantic boundary between distinct Codex reasoning-summary parts.
///
/// Codex 0.144.1 exposes `response.reasoning_summary_part.added` with a
/// `summary_index`; the audited TypeScript bridge represents each boundary with
/// this invisible separator plus a blank line. Unlike the reference's final
/// `.trim()`, this implementation deliberately preserves each segment's leading
/// and trailing whitespace exactly.
pub const REASONING_SUMMARY_SEPARATOR: &str = "\u{2063}\n\n";

/// Re-exported from [`super::utils`] so all translation modules share one
/// source of truth for the "Thinking..." placeholder.
pub use super::utils::THINKING_TEXT;

// ---------------------------------------------------------------------------
// normalizeToolSchema — defined locally (non_stream_translation may not exist
// yet as a Rust module; see assignment note about owning your own copy).
// ---------------------------------------------------------------------------

/// Mirrors `normalizeToolSchema` from `non-stream-translation.ts`: if the schema
/// is an object type with no `properties`, inject an empty `properties` map.
fn normalize_tool_schema(schema: Option<&Value>) -> Value {
    match schema {
        Some(Value::Object(map)) => {
            if map.get("type").and_then(Value::as_str) == Some("object")
                && !map.contains_key("properties")
            {
                let mut next = map.clone();
                next.insert("properties".to_string(), Value::Object(Map::new()));
                Value::Object(next)
            } else {
                Value::Object(map.clone())
            }
        }
        Some(other) => other.clone(),
        None => Value::Object(Map::new()),
    }
}

#[allow(clippy::result_large_err)]
fn validate_anthropic_context_management_for_responses(
    value: Option<&Value>,
) -> Result<(), AppError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let object = value.as_object().ok_or_else(|| {
        AppError::BadRequest("context_management must be an object when provided".to_string())
    })?;
    if object.keys().any(|key| key != "edits") {
        return Err(AppError::BadRequest(
            "context_management contains fields that cannot be represented by Responses"
                .to_string(),
        ));
    }
    let edits = object
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AppError::BadRequest("context_management.edits must be an array".to_string())
        })?;
    for (index, edit) in edits.iter().enumerate() {
        let edit = edit.as_object().ok_or_else(|| {
            AppError::BadRequest(format!(
                "context_management.edits[{index}] must be an object"
            ))
        })?;
        let is_keep_all_thinking = edit.get("type").and_then(Value::as_str)
            == Some("clear_thinking_20251015")
            && edit.get("keep").and_then(Value::as_str) == Some("all")
            && edit
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "keep"));
        if !is_keep_all_thinking {
            return Err(AppError::BadRequest(format!(
                "context_management.edits[{index}] cannot be represented by Responses; only clear_thinking_20251015 with keep=\"all\" is a safe no-op"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Signature codecs
// ---------------------------------------------------------------------------

struct CompactionCarrier {
    id: String,
    encrypted_content: String,
}

/// `cm1#${encrypted_content}@${id}`. An absent Codex compaction ID is encoded as
/// an empty suffix (`cm1#...@`) so the carrier remains distinguishable from an
/// ordinary reasoning signature.
pub fn encode_compaction_carrier_signature(encrypted_content: &str, id: &str) -> String {
    format!("{COMPACTION_SIGNATURE_PREFIX}{encrypted_content}{COMPACTION_SIGNATURE_SEPARATOR}{id}")
}

/// Inverse of [`encode_compaction_carrier_signature`]. Splits on the FIRST `@`
/// after the `cm1#` prefix. Returns `None` when the shape does not match.
fn decode_compaction_carrier_signature(signature: &str) -> Option<CompactionCarrier> {
    let raw = signature.strip_prefix(COMPACTION_SIGNATURE_PREFIX)?;

    // indexOf — first occurrence (byte index, ASCII '@').
    let separator_index = raw.find(COMPACTION_SIGNATURE_SEPARATOR)?;

    // Empty encrypted content is invalid. A trailing separator is valid and
    // represents Codex's optional compaction id.
    if separator_index == 0 {
        return None;
    }

    let encrypted_content = &raw[..separator_index];
    let id = &raw[separator_index + 1..];

    if encrypted_content.is_empty() {
        return None;
    }

    Some(CompactionCarrier {
        id: id.to_string(),
        encrypted_content: encrypted_content.to_string(),
    })
}

/// Use a versioned JSON carrier for every Responses reasoning signature so a
/// later native-Messages turn can distinguish proxy-generated history from an
/// opaque Anthropic signature without inspecting its character set.
pub fn encode_reasoning_signature(encrypted_content: Option<&str>, id: Option<&str>) -> String {
    format!(
        "{OPTIONAL_REASONING_SIGNATURE_PREFIX}{}",
        json!({"encrypted_content": encrypted_content, "id": id})
    )
}

/// Splits a legacy reasoning signature on the LAST `@`, or decodes the
/// versioned optional-field carrier emitted by [`encode_reasoning_signature`].
fn parse_reasoning_signature(signature: &str) -> (Option<String>, Option<String>) {
    if let Some(raw) = signature.strip_prefix(OPTIONAL_REASONING_SIGNATURE_PREFIX) {
        if let Ok(value) = serde_json::from_str::<Value>(raw) {
            let encrypted_content = value
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
            return (encrypted_content, id);
        }
    }

    match signature.rfind('@') {
        Some(idx) if idx != 0 && idx != signature.len() - 1 => (
            Some(signature[..idx].to_string()),
            Some(signature[idx + 1..].to_string()),
        ),
        _ => ((!signature.is_empty()).then(|| signature.to_string()), None),
    }
}

pub(crate) fn is_versioned_reasoning_carrier_signature(signature: &str) -> bool {
    let Some(raw) = signature.strip_prefix(OPTIONAL_REASONING_SIGNATURE_PREFIX) else {
        return false;
    };
    serde_json::from_str::<Value>(raw)
        .ok()
        .is_some_and(|value| {
            value.as_object().is_some_and(|value| {
                value.contains_key("encrypted_content") && value.contains_key("id")
            })
        })
}

fn is_reasoning_carrier_signature(signature: &str) -> bool {
    is_versioned_reasoning_carrier_signature(signature)
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic Messages -> Responses payload
// ---------------------------------------------------------------------------

struct TranslationState {
    /// Original Anthropic tools as `Value` for interop with `tool_search`.
    original_tools: Vec<Value>,
    tool_search_enabled: bool,
    deferred_tool_names: HashSet<String>,
    tool_use_name_by_id: std::collections::HashMap<String, String>,
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn build_prompt_cache_key(
    base_prompt_cache_key: Option<&str>,
    subagent_agent_id: Option<&str>,
) -> Option<String> {
    let base = base_prompt_cache_key?;
    if base.is_empty() {
        return None;
    }

    let normalized = subagent_agent_id.map(str::trim).filter(|s| !s.is_empty());
    match normalized {
        None => Some(base.to_string()),
        Some(agent) => Some(format!("{base}:agent:{agent}")),
    }
}

/// Serialize the typed Anthropic tools to `Value`s once, for interop with the
/// `tool_search` helpers and the per-tool converters.
#[allow(clippy::result_large_err)]
fn tools_as_values(tools: Option<&Vec<AnthropicTool>>) -> Result<Vec<Value>, AppError> {
    tools
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    serde_json::to_value(tool)
                        .map_err(|error| AppError::Other(anyhow::anyhow!("{error}")))
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}

#[allow(clippy::result_large_err)]
pub fn translate_anthropic_messages_to_responses_payload(
    payload: &AnthropicMessagesPayload,
    subagent_agent_id: Option<&str>,
) -> Result<ResponsesPayload, AppError> {
    validate_responses_request_controls(payload, false)?;
    let mut input: Vec<ResponseInputItem> = Vec::new();
    let apply_phase = should_apply_phase(&payload.model);

    let tool_values = tools_as_values(payload.tools.as_ref())?;
    let tool_slice: Option<&[Value]> = if tool_values.is_empty() {
        None
    } else {
        Some(tool_values.as_slice())
    };
    let tool_search_enabled = should_enable_responses_tool_search(&payload.model, tool_slice);
    let deferred_tool_names = list_deferred_tool_names(&tool_values).into_iter().collect();

    let mut state = TranslationState {
        original_tools: tool_values.clone(),
        tool_search_enabled,
        deferred_tool_names,
        tool_use_name_by_id: std::collections::HashMap::new(),
    };

    for (message_index, message) in payload.messages.iter().enumerate() {
        let path = format!("messages[{message_index}]");
        let items = translate_message(message, &payload.model, apply_phase, &mut state, &path)?;
        input.extend(items);
    }

    let has_original_tools = payload.tools.as_ref().is_some_and(|t| !t.is_empty());

    let translated_tools = convert_anthropic_tools(&tool_values, tool_search_enabled)?;
    let tool_choice = convert_anthropic_tool_choice(payload, tool_search_enabled)?;

    // Remove safetyIdentifier to align with vscode copilot.
    let user_id = payload.metadata.as_ref().and_then(|m| m.user_id.as_deref());
    let metadata_prompt_cache_key = parse_user_id_metadata(user_id).session_id;

    let session_affinity = request_context_store()
        .and_then(|s| s.session_affinity)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let base_prompt_cache_key = metadata_prompt_cache_key.or(session_affinity);
    let prompt_cache_key =
        build_prompt_cache_key(base_prompt_cache_key.as_deref(), subagent_agent_id);

    let metadata_value = payload
        .metadata
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| AppError::Other(anyhow::anyhow!("{error}")))?;

    // `max_tokens` is required on a real /v1/messages request; default a
    // missing value to 0 so the 12800 floor applies.
    let max_output_tokens = payload.max_tokens.unwrap_or(0).max(12800);

    let thinking_disabled = payload
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.kind == "disabled");
    let resolved_effort = if thinking_disabled {
        "none".to_string()
    } else {
        payload
            .output_config
            .as_ref()
            .and_then(|config| config.effort.clone())
            .unwrap_or_else(|| get_reasoning_effort_for_model(&payload.model))
    };
    tracing::info!(
        target: "audit",
        model = %payload.model,
        effort = %resolved_effort,
        api = "responses",
        "resolved reasoning effort"
    );
    let mut reasoning = Map::from_iter([("effort".to_string(), json!(resolved_effort))]);
    if !thinking_disabled
        && payload
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.display.as_ref().map(String::as_str))
            != Some("omitted")
    {
        reasoning.insert("summary".to_string(), json!("detailed"));
    }
    if let Some(thinking) = &payload.thinking {
        merge_open_object_extensions(&thinking.extra, &[], &mut reasoning, "thinking")?;
    }
    if let Some(output_config) = &payload.output_config {
        merge_open_object_extensions(&output_config.extra, &[], &mut reasoning, "output_config")?;
    }

    validate_anthropic_context_management_for_responses(payload.extra.get("context_management"))?;
    let extra = collect_open_object_extensions(
        &payload.extra,
        &["context_management"],
        RESPONSES_REQUEST_CANONICAL_FIELDS,
        "request",
    )?;
    let mut responses_payload = ResponsesPayload {
        model: payload.model.clone(),
        instructions: translate_system_prompt(payload.system.as_ref(), &payload.model)?,
        input: Some(InputField::Items(input)),
        tools: translated_tools,
        tool_choice: Some(tool_choice),
        // Preserve an explicit supported control. Responses reasoning defaults
        // to the established temperature of 1 only when Claude omitted it.
        temperature: payload.temperature.or(Some(1.0)),
        top_p: payload.top_p,
        max_output_tokens: Some(max_output_tokens),
        metadata: metadata_value,
        stream: payload.stream,
        safety_identifier: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        parallel_tool_calls: Some(true),
        store: Some(false),
        reasoning: Some(Value::Object(reasoning)),
        context_management: None,
        include: Some(vec!["reasoning.encrypted_content".to_string()]),
        service_tier: None,
        extra,
    };

    if has_original_tools {
        responses_payload.prompt_cache_key = prompt_cache_key;
    }

    Ok(responses_payload)
}

#[allow(clippy::result_large_err)]
pub(crate) fn validate_responses_request_controls(
    payload: &AnthropicMessagesPayload,
    codex_transport: bool,
) -> Result<(), AppError> {
    if payload
        .stop_sequences
        .as_ref()
        .is_some_and(|sequences| !sequences.is_empty())
    {
        return Err(AppError::BadRequest(
            "stop_sequences is not supported by the OpenAI Responses wire contract".to_string(),
        ));
    }
    if payload.top_k.is_some() {
        return Err(AppError::BadRequest(
            "top_k is not supported by the OpenAI Responses wire contract".to_string(),
        ));
    }
    if payload.cache_control.is_some() {
        return Err(AppError::BadRequest(
            "top-level cache_control is not supported by the OpenAI Responses wire contract"
                .to_string(),
        ));
    }
    if payload.service_tier.is_some() {
        return Err(AppError::BadRequest(
            "Anthropic service_tier cannot be represented safely by every configured OpenAI Responses transport"
                .to_string(),
        ));
    }
    if codex_transport && payload.temperature.is_some() {
        return Err(AppError::BadRequest(
            "temperature is not supported by the audited Codex Responses wire contract".to_string(),
        ));
    }
    if codex_transport && payload.top_p.is_some() {
        return Err(AppError::BadRequest(
            "top_p is not supported by the audited Codex Responses wire contract".to_string(),
        ));
    }
    Ok(())
}

fn should_apply_phase(_model: &str) -> bool {
    true
}

#[allow(clippy::result_large_err)]
fn translate_message(
    message: &AnthropicInputMessage,
    model: &str,
    apply_phase: bool,
    state: &mut TranslationState,
    path: &str,
) -> Result<Vec<ResponseInputItem>, AppError> {
    if message.role == "user" {
        translate_user_message(message, state, path)
    } else {
        translate_assistant_message(message, model, apply_phase, state, path)
    }
}

#[allow(clippy::result_large_err)]
fn translate_user_message(
    message: &AnthropicInputMessage,
    state: &mut TranslationState,
    path: &str,
) -> Result<Vec<ResponseInputItem>, AppError> {
    let message_extra = collect_open_object_extensions(
        &message.extra,
        &[],
        RESPONSES_MESSAGE_CANONICAL_FIELDS,
        path,
    )?;
    if let Some(text) = message.content.as_str() {
        return Ok(vec![create_message(
            "user",
            MessageContent::Text(text.to_string()),
            None,
            message_extra,
        )]);
    }

    let Some(blocks) = message.content.as_array() else {
        return Err(AppError::BadRequest(
            "user message content must be a string or array".to_string(),
        ));
    };

    let mut items: Vec<ResponseInputItem> = Vec::new();
    let mut pending: Vec<ResponseInputContent> = Vec::new();

    for block in blocks {
        if block_type(block) == Some("tool_result") {
            flush_pending_content(&mut pending, &mut items, "user", None, &message_extra);
            items.push(create_tool_call_output(block, state)?);
            continue;
        }

        let converted = translate_user_content_block(block)?;
        pending.extend(converted);
    }

    flush_pending_content(&mut pending, &mut items, "user", None, &message_extra);
    ensure_message_extensions_represented(&items, &message_extra, path)?;
    Ok(items)
}

#[allow(clippy::result_large_err)]
fn translate_assistant_message(
    message: &AnthropicInputMessage,
    model: &str,
    apply_phase: bool,
    state: &mut TranslationState,
    path: &str,
) -> Result<Vec<ResponseInputItem>, AppError> {
    let assistant_phase = resolve_assistant_phase(model, &message.content, apply_phase);
    let message_extra = collect_open_object_extensions(
        &message.extra,
        &[],
        RESPONSES_MESSAGE_CANONICAL_FIELDS,
        path,
    )?;

    if let Some(text) = message.content.as_str() {
        return Ok(vec![create_message(
            "assistant",
            MessageContent::Text(text.to_string()),
            assistant_phase,
            message_extra,
        )]);
    }

    let Some(blocks) = message.content.as_array() else {
        return Err(AppError::BadRequest(
            "assistant message content must be a string or array".to_string(),
        ));
    };

    let mut items: Vec<ResponseInputItem> = Vec::new();
    let mut pending: Vec<ResponseInputContent> = Vec::new();

    for block in blocks {
        if block_type(block) == Some("tool_use") {
            if let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) {
                state
                    .tool_use_name_by_id
                    .insert(id.to_string(), name.to_string());
            }
            flush_pending_content(
                &mut pending,
                &mut items,
                "assistant",
                assistant_phase.clone(),
                &message_extra,
            );
            items.push(create_tool_call(block, state)?);
            continue;
        }

        if block_type(block) == Some("thinking") {
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            if let Some(signature) = signature {
                if let Some(compaction) = create_compaction_content(signature) {
                    flush_pending_content(
                        &mut pending,
                        &mut items,
                        "assistant",
                        assistant_phase.clone(),
                        &message_extra,
                    );
                    items.push(ResponseInputItem::Compaction(compaction));
                    continue;
                }

                if is_reasoning_carrier_signature(signature) {
                    flush_pending_content(
                        &mut pending,
                        &mut items,
                        "assistant",
                        assistant_phase.clone(),
                        &message_extra,
                    );
                    items.push(ResponseInputItem::Reasoning(create_reasoning_content(
                        block,
                    )?));
                    continue;
                }
            }
        }

        if let Some(converted) = translate_assistant_content_block(block)? {
            pending.push(converted);
        }
    }

    flush_pending_content(
        &mut pending,
        &mut items,
        "assistant",
        assistant_phase,
        &message_extra,
    );
    ensure_message_extensions_represented(&items, &message_extra, path)?;
    Ok(items)
}

#[allow(clippy::result_large_err)]
fn ensure_message_extensions_represented(
    items: &[ResponseInputItem],
    message_extra: &Map<String, Value>,
    path: &str,
) -> Result<(), AppError> {
    if message_extra.is_empty()
        || items
            .iter()
            .any(|item| matches!(item, ResponseInputItem::Message(_)))
    {
        return Ok(());
    }

    Err(AppError::BadRequest(format!(
        "{path}: message extensions cannot be represented when the message contains only non-message Responses items"
    )))
}

#[allow(clippy::result_large_err)]
fn translate_user_content_block(block: &Value) -> Result<Vec<ResponseInputContent>, AppError> {
    Ok(match block_type(block) {
        Some("text") => vec![create_text_content_from_block(
            block,
            "input_text",
            "user text block",
        )?],
        Some("image") => vec![ResponseInputContent::Image(create_image_content(block)?)],
        Some("document") => vec![ResponseInputContent::File(create_file_content(block)?)],
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "Unsupported user content block type \"{other}\""
            )))
        }
        None => {
            return Err(AppError::BadRequest(
                "User content block type must be a non-empty string".to_string(),
            ))
        }
    })
}

#[allow(clippy::result_large_err)]
fn translate_assistant_content_block(
    block: &Value,
) -> Result<Option<ResponseInputContent>, AppError> {
    match block_type(block) {
        Some("text") => Ok(Some(create_text_content_from_block(
            block,
            "output_text",
            "assistant text block",
        )?)),
        Some(other) => Err(AppError::BadRequest(format!(
            "Unsupported assistant content block type \"{other}\""
        ))),
        None => Err(AppError::BadRequest(
            "Assistant content block type must be a non-empty string".to_string(),
        )),
    }
}

#[allow(clippy::result_large_err)]
fn text_field(block: &Value) -> Result<String, AppError> {
    block
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AppError::BadRequest("Text block must contain string text".to_string()))
}

fn flush_pending_content(
    pending: &mut Vec<ResponseInputContent>,
    target: &mut Vec<ResponseInputItem>,
    role: &str,
    phase: Option<String>,
    message_extra: &Map<String, Value>,
) {
    if pending.is_empty() {
        return;
    }

    let content = std::mem::take(pending);
    target.push(create_message(
        role,
        MessageContent::Blocks(content),
        phase,
        message_extra.clone(),
    ));
}

fn create_message(
    role: &str,
    content: MessageContent,
    phase: Option<String>,
    extra: Map<String, Value>,
) -> ResponseInputItem {
    let phase = if role == "assistant" { phase } else { None };
    ResponseInputItem::Message(ResponseInputMessage {
        item_type: Some(MESSAGE_TYPE.to_string()),
        role: role.to_string(),
        content: Some(content),
        status: None,
        phase,
        extra,
    })
}

fn resolve_assistant_phase(_model: &str, content: &Value, apply_phase: bool) -> Option<String> {
    if !apply_phase {
        return None;
    }

    if content.is_string() {
        return Some("final_answer".to_string());
    }

    let blocks = content.as_array()?;

    let has_text = blocks.iter().any(|b| block_type(b) == Some("text"));
    if !has_text {
        return None;
    }

    let has_tool_use = blocks.iter().any(|b| block_type(b) == Some("tool_use"));
    Some(
        if has_tool_use {
            "commentary"
        } else {
            "final_answer"
        }
        .to_string(),
    )
}

#[allow(clippy::result_large_err)]
fn create_text_content_from_block(
    block: &Value,
    target_type: &str,
    path: &str,
) -> Result<ResponseInputContent, AppError> {
    let source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    let extra = collect_open_object_extensions(
        source,
        &["type", "text", "cache_control"],
        &["type", "text"],
        path,
    )?;
    let text = text_field(block)?;
    Ok(ResponseInputContent::Text(ResponseInputText {
        block_type: target_type.to_string(),
        text,
        extra,
    }))
}

fn source_extensions(block: &Value) -> Map<String, Value> {
    let Some(source) = block.get("source").and_then(Value::as_object) else {
        return Map::new();
    };
    let known = ["type", "media_type", "data", "url", "file_id"];
    let extensions: Map<String, Value> = source
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if extensions.is_empty() {
        Map::new()
    } else {
        Map::from_iter([(
            "anthropic_source_extensions".to_string(),
            Value::Object(extensions),
        )])
    }
}

#[allow(clippy::result_large_err)]
fn translated_block_extensions(
    block: &Value,
    known_source_fields: &[&str],
    canonical_target_fields: &[&str],
    path: &str,
) -> Result<Map<String, Value>, AppError> {
    let source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    collect_open_object_extensions(source, known_source_fields, canonical_target_fields, path)
}

#[allow(clippy::result_large_err)]
fn create_image_content(block: &Value) -> Result<ResponseInputImage, AppError> {
    let source = block.get("source");
    let source_type = source
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        // Anthropic historically omitted `type` for base64 sources; default to it.
        .unwrap_or("base64");

    let image_url = match source_type {
        "url" => url_source(source, "Image")?,
        "file" => {
            return Err(AppError::BadRequest(
                "Image source of type \"file\" (Files API ids) is not supported".to_string(),
            ));
        }
        "base64" => base64_data_url(block, "image")?,
        other => {
            return Err(AppError::BadRequest(format!(
                "Unsupported image source type \"{other}\""
            )));
        }
    };

    let mut extra = source_extensions(block);
    extra.extend(translated_block_extensions(
        block,
        &["type", "source", "cache_control"],
        &[
            "type",
            "image_url",
            "file_id",
            "detail",
            "anthropic_source_extensions",
        ],
        "image block",
    )?);
    Ok(ResponseInputImage {
        block_type: "input_image".to_string(),
        image_url: Some(image_url),
        file_id: None,
        detail: Some("auto".to_string()),
        extra,
    })
}

#[allow(clippy::result_large_err)]
fn create_file_content(block: &Value) -> Result<ResponseInputFile, AppError> {
    let source = block.get("source");
    let source_type = source
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("base64");
    let filename = block
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("document.pdf")
        .to_string();

    let file_data = match source_type {
        "url" => url_source(source, "Document")?,
        "file" => {
            return Err(AppError::BadRequest(
                "Document source of type \"file\" (Files API ids) is not supported".to_string(),
            ));
        }
        "base64" => base64_data_url(block, "document")?,
        other => {
            return Err(AppError::BadRequest(format!(
                "Unsupported document source type \"{other}\""
            )));
        }
    };

    let mut extra = source_extensions(block);
    extra.extend(translated_block_extensions(
        block,
        &["type", "source", "title", "cache_control"],
        &[
            "type",
            "file_data",
            "file_id",
            "filename",
            "anthropic_source_extensions",
        ],
        "document block",
    )?);
    Ok(ResponseInputFile {
        block_type: "input_file".to_string(),
        file_data: Some(file_data),
        file_id: None,
        filename: Some(filename),
        extra,
    })
}

/// A trimmed, non-empty `source.<field>` string, or `None` when absent/blank.
fn source_field_opt<'a>(block: &'a Value, field: &str) -> Option<&'a str> {
    block
        .get("source")
        .and_then(|s| s.get(field))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Extract a non-empty `url` from a `url` source, rejecting a missing/blank URL
/// with a 400 rather than forwarding an empty image/file reference upstream.
#[allow(clippy::result_large_err)]
fn url_source(source: Option<&Value>, kind: &str) -> Result<String, AppError> {
    let url = source
        .and_then(|s| s.get("url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(format!("{kind} source of type \"url\" is missing \"url\""))
        })?;
    Ok(url.to_string())
}

/// Build a `data:` URL from a base64 source, rejecting missing/empty
/// `media_type`/`data` with a 400 instead of emitting a corrupt
/// `data:;base64,` payload.
#[allow(clippy::result_large_err)]
fn base64_data_url(block: &Value, kind: &str) -> Result<String, AppError> {
    match (
        source_field_opt(block, "media_type"),
        source_field_opt(block, "data"),
    ) {
        (Some(media_type), Some(data)) => Ok(format!("data:{media_type};base64,{data}")),
        _ => Err(AppError::BadRequest(format!(
            "Base64 {kind} source is missing \"media_type\" or \"data\""
        ))),
    }
}

#[allow(clippy::result_large_err)]
fn create_reasoning_content(block: &Value) -> Result<ResponseInputReasoning, AppError> {
    let signature = block
        .get("signature")
        .and_then(Value::as_str)
        .filter(|signature| !signature.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("thinking.signature must be a non-empty string".to_string())
        })?;
    let (encrypted_content, id) = parse_reasoning_signature(signature);
    let raw_thinking = block
        .get("thinking")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("thinking.thinking must be a string".to_string()))?;
    let thinking = if raw_thinking == THINKING_TEXT {
        ""
    } else {
        raw_thinking
    };
    let summary = create_reasoning_summary(thinking);
    let extra = translated_block_extensions(
        block,
        &["type", "thinking", "signature", "cache_control"],
        &["type", "id", "summary", "encrypted_content"],
        "thinking block",
    )?;
    Ok(ResponseInputReasoning {
        id,
        item_type: "reasoning".to_string(),
        summary,
        encrypted_content,
        extra,
    })
}

fn create_reasoning_summary(thinking: &str) -> Vec<ReasoningSummaryText> {
    if thinking.is_empty() {
        return Vec::new();
    }
    thinking
        .split(REASONING_SUMMARY_SEPARATOR)
        .map(|text| ReasoningSummaryText {
            block_type: "summary_text".to_string(),
            text: text.to_string(),
            extra: Default::default(),
        })
        .collect()
}

fn create_compaction_content(signature: &str) -> Option<ResponseInputCompaction> {
    let compaction = decode_compaction_carrier_signature(signature)?;
    Some(ResponseInputCompaction {
        id: (!compaction.id.is_empty()).then_some(compaction.id),
        item_type: "compaction".to_string(),
        encrypted_content: compaction.encrypted_content,
        extra: Default::default(),
    })
}

// ---------------------------------------------------------------------------
// Tool-call input items
// ---------------------------------------------------------------------------

#[allow(clippy::result_large_err)]
fn create_function_tool_call(
    block: &Value,
    state: &TranslationState,
) -> Result<ResponseFunctionToolCallItem, AppError> {
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("tool_use.id must be a non-empty string".to_string())
        })?;
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("tool_use.name must be a non-empty string".to_string())
        })?;
    let input = block
        .get("input")
        .filter(|input| input.is_object())
        .ok_or_else(|| AppError::BadRequest("tool_use.input must be an object".to_string()))?;
    let arguments = serde_json::to_string(input)
        .map_err(|error| AppError::Other(anyhow::anyhow!("{error}")))?;
    let namespace = if state.tool_search_enabled && state.deferred_tool_names.contains(name) {
        Some(name.to_string())
    } else {
        None
    };
    let extra = translated_block_extensions(
        block,
        &["type", "id", "name", "input", "cache_control"],
        &[
            "type",
            "call_id",
            "name",
            "arguments",
            "status",
            "namespace",
        ],
        "tool_use",
    )?;
    Ok(ResponseFunctionToolCallItem {
        item_type: "function_call".to_string(),
        call_id: id.to_string(),
        name: name.to_string(),
        arguments,
        status: Some("completed".to_string()),
        namespace,
        extra,
    })
}

#[allow(clippy::result_large_err)]
fn create_tool_search_call(block: &Value) -> Result<ResponseToolSearchCallItem, AppError> {
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("tool_use.id must be a non-empty string".to_string())
        })?;
    let input = block
        .get("input")
        .filter(|input| input.is_object())
        .ok_or_else(|| AppError::BadRequest("tool_use.input must be an object".to_string()))?;
    let extra = translated_block_extensions(
        block,
        &["type", "id", "name", "input", "cache_control"],
        &["type", "call_id", "arguments", "execution", "status"],
        "tool-search use",
    )?;
    Ok(ResponseToolSearchCallItem {
        item_type: "tool_search_call".to_string(),
        call_id: Some(id.to_string()),
        arguments: normalize_tool_search_bridge_arguments(input),
        execution: Some("client".to_string()),
        status: Some("completed".to_string()),
        extra,
    })
}

#[allow(clippy::result_large_err)]
fn create_tool_call(
    block: &Value,
    state: &TranslationState,
) -> Result<ResponseInputItem, AppError> {
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("tool_use.name must be a non-empty string".to_string())
        })?;
    if state.tool_search_enabled && is_bridge_tool_search_name(name) {
        Ok(ResponseInputItem::ToolSearchCall(create_tool_search_call(
            block,
        )?))
    } else {
        Ok(ResponseInputItem::FunctionToolCall(
            create_function_tool_call(block, state)?,
        ))
    }
}

#[allow(clippy::result_large_err)]
fn create_function_call_output(block: &Value) -> Result<ResponseFunctionCallOutputItem, AppError> {
    let call_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|call_id| !call_id.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("tool_result.tool_use_id must be a non-empty string".to_string())
        })?;
    let is_error = match block.get("is_error") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(is_error)) => *is_error,
        Some(_) => {
            return Err(AppError::BadRequest(
                "tool_result.is_error must be a boolean or null".to_string(),
            ))
        }
    };
    let extra = translated_block_extensions(
        block,
        &[
            "type",
            "tool_use_id",
            "content",
            "is_error",
            "cache_control",
        ],
        &["type", "call_id", "output", "status"],
        "tool_result",
    )?;
    Ok(ResponseFunctionCallOutputItem {
        item_type: "function_call_output".to_string(),
        call_id: call_id.to_string(),
        output: convert_tool_result_content(block.get("content"))?,
        status: Some(if is_error { "incomplete" } else { "completed" }.to_string()),
        extra,
    })
}

#[allow(clippy::result_large_err)]
fn create_tool_call_output(
    block: &Value,
    state: &TranslationState,
) -> Result<ResponseInputItem, AppError> {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|tool_use_id| !tool_use_id.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("tool_result.tool_use_id must be a non-empty string".to_string())
        })?;
    let tool_use_name = state
        .tool_use_name_by_id
        .get(tool_use_id)
        .map(String::as_str)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "tool_result.tool_use_id \"{tool_use_id}\" did not reference an earlier tool_use"
            ))
        })?;
    if state.tool_search_enabled && is_bridge_tool_search_name(tool_use_name) {
        Ok(ResponseInputItem::ToolSearchOutput(
            create_tool_search_output(block, &state.original_tools)?,
        ))
    } else {
        Ok(ResponseInputItem::FunctionCallOutput(
            create_function_call_output(block)?,
        ))
    }
}

#[allow(clippy::result_large_err)]
fn create_tool_search_output(
    block: &Value,
    original_tools: &[Value],
) -> Result<ResponseToolSearchOutputItem, AppError> {
    let content = block.get("content");
    let call_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|call_id| !call_id.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("tool_result.tool_use_id must be a non-empty string".to_string())
        })?;
    let is_error = match block.get("is_error") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(is_error)) => *is_error,
        Some(_) => {
            return Err(AppError::BadRequest(
                "tool_result.is_error must be a boolean or null".to_string(),
            ))
        }
    };

    let referenced_tool_names = resolve_tool_search_referenced_tool_names(content, original_tools)?;
    let tools: Vec<Value> = referenced_tool_names
        .iter()
        .map(|tool_name| -> Result<Value, AppError> {
            let tool = resolve_deferred_tool(tool_name, original_tools)?;
            convert_deferred_tool_to_namespace(&tool)
        })
        .collect::<Result<_, _>>()?;

    let mut extra = translated_block_extensions(
        block,
        &[
            "type",
            "tool_use_id",
            "content",
            "is_error",
            "cache_control",
        ],
        &["type", "call_id", "tools", "execution", "status"],
        "tool-search result",
    )?;
    if let Some(Value::Array(blocks)) = content {
        let mut reference_extensions = Vec::new();
        for block in blocks {
            if block_type(block) != Some("tool_reference") {
                continue;
            }
            let source = block.as_object().ok_or_else(|| {
                AppError::BadRequest("tool_reference must be an object".to_string())
            })?;
            let extensions = collect_open_object_extensions(
                source,
                &["type", "tool_name", "cache_control"],
                &[],
                "tool_reference",
            )?;
            if !extensions.is_empty() {
                let mut preserved = Map::new();
                preserved.insert(
                    "tool_name".to_string(),
                    block.get("tool_name").cloned().unwrap_or(Value::Null),
                );
                preserved.extend(extensions);
                reference_extensions.push(Value::Object(preserved));
            }
        }
        if !reference_extensions.is_empty() {
            extra.insert(
                "anthropic_tool_reference_extensions".to_string(),
                Value::Array(reference_extensions),
            );
        }
    }
    Ok(ResponseToolSearchOutputItem {
        item_type: "tool_search_output".to_string(),
        call_id: Some(call_id.to_string()),
        tools,
        execution: Some("client".to_string()),
        status: Some(if is_error { "incomplete" } else { "completed" }.to_string()),
        extra,
    })
}

#[allow(clippy::result_large_err)]
fn resolve_tool_search_referenced_tool_names(
    content: Option<&Value>,
    original_tools: &[Value],
) -> Result<Vec<String>, AppError> {
    let explicit = extract_tool_reference_names(content)?;
    if !explicit.is_empty() {
        return Ok(explicit);
    }

    if let Some(sentinel) = extract_mcp_tool_search_sentinel(content)? {
        for name in &sentinel.names {
            resolve_deferred_tool(name, original_tools)?;
        }
        return Ok(sentinel.names);
    }

    Ok(Vec::new())
}

#[allow(clippy::result_large_err)]
fn extract_tool_reference_names(content: Option<&Value>) -> Result<Vec<String>, AppError> {
    let arr = match content {
        None | Some(Value::Null | Value::String(_)) => return Ok(Vec::new()),
        Some(Value::Array(array)) => array,
        Some(_) => {
            return Err(AppError::BadRequest(
                "tool_result.content must be a string, array, or null".to_string(),
            ))
        }
    };
    arr.iter()
        .filter(|block| block_type(block) == Some("tool_reference"))
        .map(|block| {
            block
                .get("tool_name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    AppError::BadRequest(
                        "tool_reference.tool_name must be a non-empty string".to_string(),
                    )
                })
        })
        .collect()
}

#[allow(clippy::result_large_err)]
fn extract_mcp_tool_search_sentinel(
    content: Option<&Value>,
) -> Result<Option<crate::libs::tool_search::McpToolSearchSentinel>, AppError> {
    let parse = |text: &str| -> Result<Option<_>, AppError> {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return Ok(None);
        };
        if value.get("type").and_then(Value::as_str)
            != Some(crate::libs::tool_search::MCP_TOOL_SEARCH_SENTINEL_TYPE)
        {
            return Ok(None);
        }
        let names = value.get("names").ok_or_else(|| {
            AppError::BadRequest("tool search sentinel names field is required".to_string())
        })?;
        let names = names.as_array().ok_or_else(|| {
            AppError::BadRequest("tool search sentinel names must be an array".to_string())
        })?;
        if names.is_empty() {
            return Err(AppError::BadRequest(
                "tool search sentinel names must not be empty".to_string(),
            ));
        }
        let names = names
            .iter()
            .map(|name| {
                name.as_str()
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        AppError::BadRequest(
                            "tool search sentinel names must contain non-empty strings".to_string(),
                        )
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(Some(crate::libs::tool_search::McpToolSearchSentinel {
            r#type: crate::libs::tool_search::MCP_TOOL_SEARCH_SENTINEL_TYPE.to_string(),
            names,
        }))
    };
    match content {
        Some(Value::String(s)) => parse(s),
        Some(Value::Array(arr)) => {
            for block in arr {
                if block_type(block) != Some("text") {
                    continue;
                }
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if let Some(sentinel) = parse(text)? {
                        return Ok(Some(sentinel));
                    }
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

#[allow(clippy::result_large_err)]
fn resolve_deferred_tool(tool_name: &str, original_tools: &[Value]) -> Result<Value, AppError> {
    let found = original_tools
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(tool_name));
    if let Some(tool) = found {
        if tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(is_deferred_tool_name)
            && tool.get("defer_loading") == Some(&Value::Bool(true))
        {
            return Ok(tool.clone());
        }
    }
    Err(AppError::BadRequest(format!(
        "tool_reference.tool_name \"{tool_name}\" did not reference a defined deferred tool"
    )))
}

// ---------------------------------------------------------------------------
// System prompt + tool conversion
// ---------------------------------------------------------------------------

#[allow(clippy::result_large_err)]
fn translate_system_prompt(
    system: Option<&Value>,
    model: &str,
) -> Result<Option<String>, AppError> {
    let Some(system) = system else {
        return Ok(None);
    };
    if system.is_null() {
        return Ok(None);
    }

    let extra_prompt = get_extra_prompt_for_model(model);

    if let Some(s) = system.as_str() {
        if s.is_empty() {
            return Ok(None);
        }
        return Ok(Some(format!("{s}{extra_prompt}")));
    }

    let blocks = system.as_array().ok_or_else(|| {
        AppError::BadRequest("system must be a string, array, or null".to_string())
    })?;
    let parts: Vec<String> = blocks
        .iter()
        .enumerate()
        .map(|(index, block)| -> Result<String, AppError> {
            let block_object = block.as_object().ok_or_else(|| {
                AppError::BadRequest(format!("system[{index}] must be an object"))
            })?;
            if let Some((key, _)) = block_object
                .iter()
                .find(|(key, _)| !matches!(key.as_str(), "type" | "text" | "cache_control"))
            {
                return Err(AppError::BadRequest(format!(
                    "system[{index}].{key} cannot be represented in Responses instructions"
                )));
            }
            let text = block.get("text").and_then(Value::as_str).ok_or_else(|| {
                AppError::BadRequest(format!("system[{index}].text must be a string"))
            })?;
            Ok(if index == 0 {
                format!("{text}\n\n{extra_prompt}\n\n")
            } else {
                text.to_string()
            })
        })
        .collect::<Result<_, _>>()?;
    let text = parts.join(" ");
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

#[allow(clippy::result_large_err)]
fn convert_anthropic_tools(
    tools: &[Value],
    tool_search_enabled: bool,
) -> Result<Option<Vec<Value>>, AppError> {
    if tools.is_empty() {
        return Ok(None);
    }

    let mut converted: Vec<Value> = Vec::new();
    let mut added_tool_search = false;
    let searchable_tool_names = if tool_search_enabled {
        list_deferred_tool_names(tools)
    } else {
        Vec::new()
    };

    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                AppError::BadRequest("tool.name must be a non-empty string".to_string())
            })?;

        if is_bridge_tool_search_name(name) {
            if tool_search_enabled && !added_tool_search {
                converted.push(create_responses_tool_search_definition(
                    tool,
                    &searchable_tool_names,
                )?);
                added_tool_search = true;
            }
            continue;
        }

        if tool_search_enabled
            && is_deferred_tool_name(name)
            && tool.get("defer_loading") == Some(&Value::Bool(true))
        {
            converted.push(convert_deferred_tool_to_namespace(tool)?);
            continue;
        }

        converted.push(convert_tool_to_function(tool)?);
    }

    Ok(Some(converted))
}

#[allow(clippy::result_large_err)]
fn create_responses_tool_search_definition(
    source_tool: &Value,
    searchable_tool_names: &[String],
) -> Result<Value, AppError> {
    let mut value = json!({
        "type": "tool_search",
        "execution": "client",
        "description": "Load deferred tools by exact name before using them. Return only the searchable tool names you need for the next step.",
        "parameters": {
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "description": "Exact deferred tool names to load.",
                    "items": {
                        "type": "string",
                        "enum": searchable_tool_names,
                    },
                    "minItems": 1,
                },
            },
            "required": ["names"],
            "additionalProperties": false,
        },
    });
    let target = value
        .as_object_mut()
        .expect("static tool-search definition object");
    let source = source_tool.as_object().ok_or_else(|| {
        AppError::BadRequest("tool-search bridge definition must be an object".to_string())
    })?;
    merge_open_object_extensions(
        source,
        ANTHROPIC_TOOL_KNOWN_FIELDS,
        target,
        "tool-search bridge",
    )?;
    Ok(value)
}

#[allow(clippy::result_large_err)]
fn convert_tool_to_function(tool: &Value) -> Result<Value, AppError> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest("tool.name must be a non-empty string".to_string()))?;
    let schema = tool
        .get("input_schema")
        .filter(|schema| schema.is_object() || schema.is_boolean())
        .ok_or_else(|| {
            AppError::BadRequest(
                "tool.input_schema must be an object or boolean schema".to_string(),
            )
        })?;
    let parameters = normalize_tool_schema(Some(schema));
    let mut obj = Map::new();
    obj.insert("type".to_string(), json!("function"));
    obj.insert("name".to_string(), json!(name));
    obj.insert("parameters".to_string(), parameters);
    obj.insert(
        "strict".to_string(),
        tool.get("strict").cloned().unwrap_or(Value::Bool(false)),
    );
    if let Some(description) = tool.get("description").and_then(Value::as_str) {
        if !description.is_empty() {
            obj.insert("description".to_string(), json!(description));
        }
    }
    let source = tool
        .as_object()
        .ok_or_else(|| AppError::BadRequest("tool definition must be an object".to_string()))?;
    merge_open_object_extensions(source, ANTHROPIC_TOOL_KNOWN_FIELDS, &mut obj, "tool")?;
    Ok(Value::Object(obj))
}

#[allow(clippy::result_large_err)]
fn convert_deferred_tool_to_namespace(tool: &Value) -> Result<Value, AppError> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("deferred tool.name must be a non-empty string".to_string())
        })?;
    let schema = tool
        .get("input_schema")
        .filter(|schema| schema.is_object() || schema.is_boolean())
        .ok_or_else(|| {
            AppError::BadRequest(
                "deferred tool.input_schema must be an object or boolean schema".to_string(),
            )
        })?;
    let parameters = normalize_tool_schema(Some(schema));
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty());

    let mut inner = Map::new();
    inner.insert("type".to_string(), json!("function"));
    inner.insert("name".to_string(), json!(name));
    inner.insert("parameters".to_string(), parameters);
    inner.insert(
        "strict".to_string(),
        tool.get("strict").cloned().unwrap_or(Value::Bool(false)),
    );
    inner.insert("defer_loading".to_string(), json!(true));
    if let Some(description) = description {
        inner.insert("description".to_string(), json!(description));
    }

    let mut obj = Map::new();
    obj.insert("type".to_string(), json!("namespace"));
    obj.insert("name".to_string(), json!(name));
    if let Some(description) = description {
        obj.insert("description".to_string(), json!(description));
    }
    obj.insert(
        "tools".to_string(),
        Value::Array(vec![Value::Object(inner)]),
    );
    let source = tool.as_object().ok_or_else(|| {
        AppError::BadRequest("deferred tool definition must be an object".to_string())
    })?;
    merge_open_object_extensions(
        source,
        ANTHROPIC_TOOL_KNOWN_FIELDS,
        &mut obj,
        "deferred tool",
    )?;
    Ok(Value::Object(obj))
}

#[allow(clippy::result_large_err)]
fn convert_anthropic_tool_choice(
    payload: &AnthropicMessagesPayload,
    tool_search_enabled: bool,
) -> Result<Value, AppError> {
    let Some(choice) = payload.tool_choice.as_ref() else {
        return Ok(json!("auto"));
    };

    let scalar_choice = |value: Value| -> Result<Value, AppError> {
        if let Some((key, _)) = choice.extra.iter().next() {
            return Err(AppError::BadRequest(format!(
                "tool_choice.{key} cannot be represented by a scalar Responses tool choice"
            )));
        }
        Ok(value)
    };

    match choice.kind.as_str() {
        "auto" => scalar_choice(json!("auto")),
        "any" => scalar_choice(json!("required")),
        "tool" => {
            if tool_search_enabled {
                if let Some(name) = choice.name.as_deref() {
                    if is_bridge_tool_search_name(name) {
                        return scalar_choice(json!("auto"));
                    }
                }
            }
            let name = match choice.name.as_deref() {
                Some(name) if !name.trim().is_empty() => name,
                _ => {
                    return Err(AppError::BadRequest(
                        "tool_choice.name must be a non-empty string for type tool".to_string(),
                    ))
                }
            };
            let mut target = Map::from_iter([
                ("type".to_string(), json!("function")),
                ("name".to_string(), json!(name)),
            ]);
            merge_open_object_extensions(&choice.extra, &[], &mut target, "tool_choice")?;
            Ok(Value::Object(target))
        }
        "none" => scalar_choice(json!("none")),
        _ => Err(AppError::BadRequest(
            "tool_choice.type must be one of auto, any, tool, or none".to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Result translation: Responses output -> Anthropic message response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputValidationPhase {
    Added,
    Done,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ValidatedResponsesUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: i64,
    pub reasoning_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponsesTerminalKind {
    Completed,
    Incomplete,
    Failed,
}

impl ResponsesTerminalKind {
    pub fn from_event_type(event_type: &str) -> Option<Self> {
        match event_type {
            "response.completed" => Some(Self::Completed),
            "response.incomplete" => Some(Self::Incomplete),
            "response.failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn expected_status(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
        }
    }
}

pub(crate) fn validate_function_arguments(
    arguments: &str,
    phase: OutputValidationPhase,
) -> Result<(), &'static str> {
    if arguments.is_empty() && phase == OutputValidationPhase::Added {
        return Ok(());
    }
    if arguments.is_empty()
        || serde_json::from_str::<Value>(arguments)
            .ok()
            .is_none_or(|arguments| !arguments.is_object())
    {
        return Err("Function call arguments were empty or not a JSON object.");
    }
    Ok(())
}

fn validate_item_type(actual: &str, expected: &str) -> Result<(), &'static str> {
    if actual == expected {
        Ok(())
    } else {
        Err("A typed Responses output item had an inconsistent type.")
    }
}

fn validate_optional_item_status(
    status: Option<&str>,
    phase: OutputValidationPhase,
) -> Result<(), &'static str> {
    let Some(status) = status else {
        return Ok(());
    };
    let valid = match phase {
        OutputValidationPhase::Added => {
            matches!(status, "in_progress" | "completed" | "incomplete")
        }
        OutputValidationPhase::Done => matches!(status, "completed" | "incomplete"),
    };
    if valid {
        Ok(())
    } else {
        Err("A Responses output item had an invalid status.")
    }
}

fn validate_internal_metadata(extra: &Map<String, Value>) -> Result<(), &'static str> {
    let Some(metadata) = extra.get("internal_chat_message_metadata_passthrough") else {
        return Ok(());
    };
    if metadata.is_null() {
        return Ok(());
    }
    let Some(metadata) = metadata.as_object() else {
        return Err("Internal chat message metadata was not an object or null.");
    };
    if let Some(turn_id) = metadata.get("turn_id") {
        if !turn_id.is_null() && !turn_id.is_string() {
            return Err("Internal chat message metadata turn_id was not a string or null.");
        }
    }
    Ok(())
}

pub(crate) fn validate_typed_output_item(
    item: &ResponseOutputItem,
    phase: OutputValidationPhase,
) -> Result<(), &'static str> {
    match item {
        ResponseOutputItem::FunctionCall(call) => {
            validate_item_type(&call.item_type, "function_call")?;
            if call.call_id.trim().is_empty() {
                return Err("A function_call item had a missing or empty call_id.");
            }
            if call.name.trim().is_empty() {
                return Err("A function_call item had a missing or empty name.");
            }
            validate_function_arguments(&call.arguments, phase)?;
            validate_optional_item_status(call.status.as_deref(), phase)?;
            validate_internal_metadata(&call.extra)?;
        }
        ResponseOutputItem::CustomToolCall(call) => {
            validate_item_type(&call.item_type, "custom_tool_call")?;
            if call.call_id.trim().is_empty() || call.name.trim().is_empty() {
                return Err("A custom_tool_call item had missing required identity.");
            }
            validate_optional_item_status(call.status.as_deref(), phase)?;
            validate_internal_metadata(&call.extra)?;
        }
        ResponseOutputItem::ToolSearchCall(call) => {
            validate_item_type(&call.item_type, "tool_search_call")?;
            if call.execution.trim().is_empty() {
                return Err("A tool_search_call item had a missing or empty execution.");
            }
            if call
                .call_id
                .as_deref()
                .is_some_and(|call_id| call_id.trim().is_empty())
            {
                return Err("A tool_search_call item had an empty call_id.");
            }
            validate_optional_item_status(call.status.as_deref(), phase)?;
            validate_internal_metadata(&call.extra)?;
        }
        ResponseOutputItem::ToolSearchOutput(output) => {
            validate_item_type(&output.item_type, "tool_search_output")?;
            if output.execution.trim().is_empty() || output.status.trim().is_empty() {
                return Err("A tool_search_output item had missing required scalars.");
            }
            if !matches!(output.status.as_str(), "completed" | "incomplete") {
                return Err("A tool_search_output item had an invalid status.");
            }
            validate_internal_metadata(&output.extra)?;
        }
        ResponseOutputItem::Message(message) => {
            validate_item_type(&message.item_type, "message")?;
            if message.role != "assistant" {
                return Err("A streamed output message did not have the assistant role.");
            }
            validate_optional_item_status(message.status.as_deref(), phase)?;
            if let Some(phase) = message.extra.get("phase") {
                if !matches!(phase.as_str(), Some("commentary" | "final_answer"))
                    && !phase.is_null()
                {
                    return Err("A message item had an invalid phase.");
                }
            }
            validate_internal_metadata(&message.extra)?;
            for block in &message.content {
                match block {
                    ResponseOutputContentBlock::Text(text) => {
                        if !matches!(text.block_type.as_str(), "output_text" | "input_text") {
                            return Err("A message text block had an unsupported type.");
                        }
                        if text
                            .annotations
                            .iter()
                            .flatten()
                            .any(|annotation| !annotation.is_object())
                        {
                            return Err("An annotation entry was not an object.");
                        }
                    }
                    ResponseOutputContentBlock::Refusal(_) => {
                        return Err("A message content block had an unsupported type.");
                    }
                    ResponseOutputContentBlock::Other(_) => {
                        return Err("A message content block had an unsupported type.");
                    }
                }
            }
        }
        ResponseOutputItem::Reasoning(reasoning) => {
            validate_item_type(&reasoning.item_type, "reasoning")?;
            validate_optional_item_status(reasoning.status.as_deref(), phase)?;
            validate_internal_metadata(&reasoning.extra)?;
            for summary in &reasoning.summary {
                if summary.block_type != "summary_text" {
                    return Err("A reasoning summary block had an unsupported type.");
                }
            }
            if let Some(content) = &reasoning.content {
                for content in content {
                    if !matches!(content.block_type.as_str(), "reasoning_text" | "text") {
                        return Err("A reasoning content block had an unsupported type.");
                    }
                }
            }
        }
        ResponseOutputItem::Compaction(compaction) => {
            validate_item_type(&compaction.item_type, "compaction")?;
            if compaction.encrypted_content.trim().is_empty() {
                return Err("A compaction item had missing or empty encrypted_content.");
            }
            validate_internal_metadata(&compaction.extra)?;
        }
        ResponseOutputItem::Other(_) => {
            // Raw variants remain valid for native forwarding and the dedicated
            // web-search classifier. Anthropic translation applies the stricter
            // `parse_and_validate_anthropic_output_item` policy below.
        }
    }
    Ok(())
}

pub(crate) fn parse_and_validate_output_item(
    value: &Value,
    phase: OutputValidationPhase,
) -> Result<ResponseOutputItem, &'static str> {
    let item: ResponseOutputItem = serde_json::from_value(value.clone())
        .map_err(|_| "A known output item did not match its typed Responses contract.")?;
    validate_typed_output_item(&item, phase)?;
    Ok(item)
}

pub(crate) fn parse_and_validate_anthropic_output_item(
    value: &Value,
    phase: OutputValidationPhase,
) -> Result<ResponseOutputItem, &'static str> {
    let item = parse_and_validate_output_item(value, phase)?;
    if matches!(item, ResponseOutputItem::Other(_)) {
        return Err("The Responses output contained a variant unsupported by Anthropic Messages.");
    }
    Ok(item)
}

pub(crate) fn canonical_anthropic_output_item(
    value: &Value,
    phase: OutputValidationPhase,
) -> Result<Value, &'static str> {
    let item = parse_and_validate_anthropic_output_item(value, phase)?;
    serde_json::to_value(item)
        .map_err(|_| "A Responses output item could not be canonicalized for reconciliation.")
}

fn required_nonnegative_usage_field(
    object: &Map<String, Value>,
    field: &str,
) -> Result<i64, &'static str> {
    object
        .get(field)
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or("A Responses usage object had a missing, wrong-typed, or negative token field.")
}

fn optional_usage_detail(
    usage: &Map<String, Value>,
    details_field: &str,
    token_field: &str,
) -> Result<Option<i64>, &'static str> {
    let Some(details) = usage.get(details_field) else {
        return Ok(None);
    };
    if details.is_null() {
        return Ok(None);
    }
    let Some(details) = details.as_object() else {
        return Err("A Responses usage details field was not an object or null.");
    };
    required_nonnegative_usage_field(details, token_field).map(Some)
}

fn validate_usage_counters(
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    cached_input_tokens: Option<i64>,
    reasoning_output_tokens: Option<i64>,
) -> Result<ValidatedResponsesUsage, &'static str> {
    if input_tokens < 0 || output_tokens < 0 || total_tokens < 0 {
        return Err("A Responses usage object contained a negative token field.");
    }
    let Some(expected_total) = input_tokens.checked_add(output_tokens) else {
        return Err("A Responses usage total overflowed the supported integer range.");
    };
    if total_tokens != expected_total {
        return Err("A Responses usage total did not equal input plus output tokens.");
    }
    if cached_input_tokens.is_some_and(|cached| cached < 0 || cached > input_tokens) {
        return Err("A Responses cached token count exceeded its input token count.");
    }
    if reasoning_output_tokens.is_some_and(|reasoning| reasoning < 0 || reasoning > output_tokens) {
        return Err("A Responses reasoning token count exceeded its output token count.");
    }
    Ok(ValidatedResponsesUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
    })
}

pub(crate) fn validate_raw_responses_usage(
    response: &Value,
) -> Result<ValidatedResponsesUsage, &'static str> {
    let Some(usage) = response.get("usage") else {
        return Ok(ValidatedResponsesUsage::default());
    };
    if usage.is_null() {
        return Ok(ValidatedResponsesUsage::default());
    }
    let Some(usage) = usage.as_object() else {
        return Err("A Responses usage field was not an object or null.");
    };
    let input_tokens = required_nonnegative_usage_field(usage, "input_tokens")?;
    let output_tokens = required_nonnegative_usage_field(usage, "output_tokens")?;
    let total_tokens = required_nonnegative_usage_field(usage, "total_tokens")?;
    let cached_input_tokens =
        optional_usage_detail(usage, "input_tokens_details", "cached_tokens")?;
    let reasoning_output_tokens =
        optional_usage_detail(usage, "output_tokens_details", "reasoning_tokens")?;
    validate_usage_counters(
        input_tokens,
        output_tokens,
        total_tokens,
        cached_input_tokens,
        reasoning_output_tokens,
    )
}

pub(crate) fn validate_typed_responses_usage(
    usage: Option<&ResponseUsage>,
) -> Result<ValidatedResponsesUsage, &'static str> {
    let Some(usage) = usage else {
        return Ok(ValidatedResponsesUsage::default());
    };
    validate_usage_counters(
        usage.input_tokens,
        usage.output_tokens,
        usage.total_tokens,
        usage
            .input_tokens_details
            .as_ref()
            .map(|details| details.cached_tokens),
        usage
            .output_tokens_details
            .as_ref()
            .map(|details| details.reasoning_tokens),
    )
}

pub(crate) fn validate_terminal_status(
    response: &Value,
    terminal_kind: ResponsesTerminalKind,
) -> Result<(), &'static str> {
    match response.get("status") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(status)) if status == terminal_kind.expected_status() => Ok(()),
        Some(Value::String(_)) => {
            Err("A terminal Responses event had an inconsistent response status.")
        }
        Some(_) => Err("A terminal Responses event had a non-string response status."),
    }
}

pub(crate) fn validate_created_status(response: &Value) -> Result<(), &'static str> {
    match response.get("status") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(status)) if status == "in_progress" => Ok(()),
        Some(Value::String(_)) => {
            Err("A response.created event had an inconsistent response status.")
        }
        Some(_) => Err("A response.created event had a non-string response status."),
    }
}

pub(crate) fn optional_nonnull_string_field<'a>(
    response: &'a Value,
    field: &str,
) -> Result<Option<&'a str>, &'static str> {
    match response.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err("A supported Responses field was not a string or null."),
    }
}

pub(crate) fn reconcile_tool_search_call_id<'a>(
    added: Option<&'a str>,
    done: Option<&'a str>,
) -> Result<Option<&'a str>, &'static str> {
    match (
        added.filter(|id| !id.is_empty()),
        done.filter(|id| !id.is_empty()),
    ) {
        (Some(added), Some(done)) if added == done => Ok(Some(done)),
        (Some(_), Some(_)) => Err("A completed tool_search_call changed its call id."),
        (Some(added), None) => Ok(Some(added)),
        (None, done) => Ok(done),
    }
}

fn raw_optional_string<'a>(item: &'a Value, field: &str) -> Option<&'a str> {
    item.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn raw_resolved_tool_name(item: &Value) -> Option<&str> {
    raw_optional_string(item, "namespace")
        .filter(|namespace| !namespace.trim().is_empty())
        .or_else(|| raw_optional_string(item, "name"))
}

pub(crate) fn validate_output_item_reconciliation(
    added: &Value,
    done: &Value,
) -> Result<(), &'static str> {
    let added_type = added.get("type").and_then(Value::as_str);
    let done_type = done.get("type").and_then(Value::as_str);
    if added_type != done_type {
        return Err("A completed output item changed its type.");
    }
    let Some(item_type) = done_type else {
        return Err("A completed output item had an invalid type.");
    };

    if let (Some(added_id), Some(done_id)) = (
        raw_optional_string(added, "id"),
        raw_optional_string(done, "id"),
    ) {
        if added_id != done_id {
            return Err("A completed output item changed its item id.");
        }
    }
    if let (Some(added_metadata), Some(done_metadata)) = (
        added.get("internal_chat_message_metadata_passthrough"),
        done.get("internal_chat_message_metadata_passthrough"),
    ) {
        if !added_metadata.is_null() && !done_metadata.is_null() && added_metadata != done_metadata
        {
            return Err("A completed output item changed its internal metadata.");
        }
    }

    match item_type {
        "function_call" | "custom_tool_call" => {
            if raw_optional_string(added, "call_id") != raw_optional_string(done, "call_id") {
                return Err("A completed function/tool call changed its call id.");
            }
            if raw_resolved_tool_name(added) != raw_resolved_tool_name(done) {
                return Err("A completed function/tool call changed its function name.");
            }
            if item_type == "custom_tool_call" {
                if let (Some(added_input), Some(done_input)) = (
                    added.get("input").and_then(Value::as_str),
                    done.get("input").and_then(Value::as_str),
                ) {
                    if !added_input.is_empty() && added_input != done_input {
                        return Err("A completed custom tool call changed its input.");
                    }
                }
            }
        }
        "tool_search_call" => {
            reconcile_tool_search_call_id(
                added.get("call_id").and_then(Value::as_str),
                done.get("call_id").and_then(Value::as_str),
            )?;
            if added.get("execution") != done.get("execution") {
                return Err("A completed tool search changed its execution mode.");
            }
            if added.get("arguments") != done.get("arguments") {
                return Err("A completed tool search changed its arguments.");
            }
        }
        "tool_search_output" => {
            if let (Some(added_call_id), Some(done_call_id)) = (
                raw_optional_string(added, "call_id"),
                raw_optional_string(done, "call_id"),
            ) {
                if added_call_id != done_call_id {
                    return Err("A completed tool search output changed its call id.");
                }
            }
            if added.get("execution") != done.get("execution")
                || added.get("tools") != done.get("tools")
            {
                return Err("A completed tool search output changed its payload.");
            }
        }
        "message" => {
            if added.get("role") != done.get("role") {
                return Err("A completed message changed its role.");
            }
        }
        "reasoning" => {
            if let (Some(added_encrypted), Some(done_encrypted)) = (
                added.get("encrypted_content"),
                done.get("encrypted_content"),
            ) {
                if !added_encrypted.is_null()
                    && !done_encrypted.is_null()
                    && added_encrypted != done_encrypted
                {
                    return Err("A completed reasoning item changed its encrypted content.");
                }
            }
        }
        "compaction" | "compaction_summary" => {
            if added.get("encrypted_content") != done.get("encrypted_content") {
                return Err("A completed compaction item changed its encrypted content.");
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn stable_tool_use_id(
    call_id: Option<&str>,
    item_id: Option<&str>,
    output_index: i64,
) -> String {
    call_id
        .filter(|id| !id.is_empty())
        .or_else(|| item_id.filter(|id| !id.is_empty()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("tool_call_{output_index}"))
}

fn output_item_id(item: &ResponseOutputItem) -> Option<&str> {
    match item {
        ResponseOutputItem::Message(item) => item.id.as_deref(),
        ResponseOutputItem::FunctionCall(item) => item.id.as_deref(),
        ResponseOutputItem::CustomToolCall(item) => item.id.as_deref(),
        ResponseOutputItem::ToolSearchOutput(item) => item.id.as_deref(),
        ResponseOutputItem::ToolSearchCall(item) => item.id.as_deref(),
        ResponseOutputItem::Compaction(item) => item.id.as_deref(),
        ResponseOutputItem::Reasoning(item) => item.id.as_deref(),
        ResponseOutputItem::Other(_) => None,
    }
}

fn output_tool_call_id(item: &ResponseOutputItem) -> Option<&str> {
    match item {
        ResponseOutputItem::FunctionCall(item) => Some(item.call_id.as_str()),
        ResponseOutputItem::CustomToolCall(item) => Some(item.call_id.as_str()),
        ResponseOutputItem::ToolSearchCall(item) => item.call_id.as_deref(),
        ResponseOutputItem::Message(_)
        | ResponseOutputItem::ToolSearchOutput(_)
        | ResponseOutputItem::Compaction(_)
        | ResponseOutputItem::Reasoning(_)
        | ResponseOutputItem::Other(_) => None,
    }
}

fn output_item_status(item: &ResponseOutputItem) -> Option<&str> {
    match item {
        ResponseOutputItem::Message(item) => item.status.as_deref(),
        ResponseOutputItem::FunctionCall(item) => item.status.as_deref(),
        ResponseOutputItem::CustomToolCall(item) => item.status.as_deref(),
        ResponseOutputItem::ToolSearchOutput(item) => Some(item.status.as_str()),
        ResponseOutputItem::ToolSearchCall(item) => item.status.as_deref(),
        ResponseOutputItem::Reasoning(item) => item.status.as_deref(),
        ResponseOutputItem::Compaction(_) | ResponseOutputItem::Other(_) => None,
    }
}

#[allow(clippy::result_large_err)]
pub(crate) fn validate_typed_output_items_and_usage(
    response: &ResponsesResult,
) -> Result<(), AppError> {
    let mut item_ids = HashSet::new();
    let mut tool_call_ids = HashSet::new();
    for item in &response.output {
        validate_typed_output_item(item, OutputValidationPhase::Done)
            .map_err(invalid_upstream_output)?;
        if response.status == "completed" && output_item_status(item) == Some("incomplete") {
            return Err(invalid_upstream_output(
                "a completed response contained an incomplete output item",
            ));
        }
        if let Some(item_id) = output_item_id(item).filter(|id| !id.is_empty()) {
            if !item_ids.insert(item_id) {
                return Err(invalid_upstream_output("response output reused an item id"));
            }
        }
        if let Some(call_id) = output_tool_call_id(item).filter(|id| !id.is_empty()) {
            if !tool_call_ids.insert(call_id) {
                return Err(invalid_upstream_output(
                    "response output reused a tool call id",
                ));
            }
        }
    }
    validate_typed_responses_usage(response.usage.as_ref()).map_err(invalid_upstream_output)?;
    Ok(())
}

#[allow(clippy::result_large_err)]
pub(crate) fn validate_complete_responses_result(
    response: &ResponsesResult,
) -> Result<(), AppError> {
    if response.id.trim().is_empty()
        || response.model.trim().is_empty()
        || !matches!(response.status.as_str(), "completed" | "incomplete")
    {
        return Err(invalid_upstream_output(
            "response id/model/status was missing or invalid",
        ));
    }
    validate_typed_output_items_and_usage(response)
}

#[allow(clippy::result_large_err)]
pub fn translate_responses_result_to_anthropic(
    response: &ResponsesResult,
    tool_search_name: Option<&str>,
) -> Result<AnthropicResponse, AppError> {
    validate_complete_responses_result(response)?;
    let content_blocks = map_output_to_anthropic_content(&response.output, tool_search_name)?;
    let usage = map_responses_usage(response)?;

    let anthropic_content = if content_blocks.is_empty() {
        match response.output_text.as_deref() {
            Some(output_text) => fallback_content_blocks(output_text),
            None => Vec::new(),
        }
    } else {
        content_blocks
    };

    // Derive `tool_use` from the validated blocks actually emitted. Required
    // tool identity and arguments have already been rejected rather than
    // defaulted, so stop_reason cannot describe a silently dropped tool call.
    let has_tool_use = anthropic_content
        .iter()
        .any(|b| block_type(b) == Some("tool_use"));
    let stop_reason = map_responses_stop_reason(response, has_tool_use)?;

    Ok(AnthropicResponse {
        id: response.id.clone(),
        kind: "message".to_string(),
        role: "assistant".to_string(),
        content: anthropic_content,
        model: response.model.clone(),
        stop_reason,
        stop_sequence: None,
        usage,
        extra: serde_json::Map::new(),
    })
}

#[allow(clippy::result_large_err)]
fn map_output_to_anthropic_content(
    output: &[ResponseOutputItem],
    tool_search_name: Option<&str>,
) -> Result<Vec<Value>, AppError> {
    let mut content_blocks: Vec<Value> = Vec::new();

    for (output_index, item) in output.iter().enumerate() {
        validate_typed_output_item(item, OutputValidationPhase::Done)
            .map_err(invalid_upstream_output)?;
        match item {
            ResponseOutputItem::Reasoning(reasoning) => {
                if let Some(thinking_text) = extract_reasoning_text(reasoning) {
                    let signature = encode_reasoning_signature(
                        reasoning.encrypted_content.as_deref(),
                        reasoning.id.as_deref(),
                    );
                    content_blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking_text,
                        "signature": signature,
                    }));
                }
            }
            ResponseOutputItem::FunctionCall(call) => {
                content_blocks.push(create_tool_use_content_block(call)?);
            }
            ResponseOutputItem::CustomToolCall(call) => {
                content_blocks.push(create_custom_tool_use_content_block(call));
            }
            ResponseOutputItem::ToolSearchCall(call) => {
                let output_index = i64::try_from(output_index)
                    .map_err(|_| invalid_upstream_output("output index exceeded i64"))?;
                content_blocks.push(create_tool_search_use_content_block(
                    call,
                    tool_search_name,
                    output_index,
                )?);
            }
            ResponseOutputItem::ToolSearchOutput(_) => {}
            ResponseOutputItem::Message(message) => {
                let combined = combine_message_text_content(&message.content);
                if !combined.is_empty() {
                    content_blocks.push(json!({ "type": "text", "text": combined }));
                }
            }
            ResponseOutputItem::Compaction(compaction) => {
                content_blocks.push(create_compaction_thinking_block(compaction));
            }
            ResponseOutputItem::Other(_) => {
                return Err(invalid_upstream_output(
                    "output variant is unsupported by Anthropic Messages",
                ))
            }
        }
    }

    Ok(content_blocks)
}

fn combine_message_text_content(blocks: &[ResponseOutputContentBlock]) -> String {
    let mut aggregated = String::new();
    for block in blocks {
        match block {
            ResponseOutputContentBlock::Text(t) => aggregated.push_str(&t.text),
            ResponseOutputContentBlock::Refusal(_) | ResponseOutputContentBlock::Other(_) => {}
        }
    }
    aggregated
}

fn extract_reasoning_text(
    item: &crate::services::copilot::create_responses::ResponseOutputReasoning,
) -> Option<String> {
    effective_reasoning_text(
        item.summary.iter().map(|block| block.text.as_str()).chain(
            item.content
                .iter()
                .flatten()
                .map(|block| block.text.as_str()),
        ),
        item.encrypted_content.as_deref(),
        item.id.as_deref(),
    )
}

pub(crate) fn effective_reasoning_text<'a>(
    summary_texts: impl IntoIterator<Item = &'a str>,
    encrypted_content: Option<&str>,
    id: Option<&str>,
) -> Option<String> {
    let segments: Vec<&str> = summary_texts.into_iter().collect();
    if segments.iter().any(|text| !text.trim().is_empty()) {
        return Some(segments.join(REASONING_SUMMARY_SEPARATOR));
    }

    let has_opaque_carrier = encrypted_content.is_some() || id.is_some();
    has_opaque_carrier.then(|| THINKING_TEXT.to_string())
}

fn invalid_upstream_output(message: impl std::fmt::Display) -> AppError {
    AppError::Other(anyhow::anyhow!(
        "Invalid upstream Responses output: {message}"
    ))
}

#[allow(clippy::result_large_err)]
fn create_tool_use_content_block(
    call: &crate::services::copilot::create_responses::ResponseOutputFunctionCall,
) -> Result<Value, AppError> {
    let tool_id = &call.call_id;
    let tool_name = resolve_tool_use_name(call.name.as_str(), call.namespace.as_deref());
    let input = parse_function_call_arguments(&call.arguments)?;
    Ok(json!({
        "type": "tool_use",
        "id": tool_id,
        "name": tool_name,
        "input": input,
    }))
}

#[allow(clippy::result_large_err)]
fn create_tool_search_use_content_block(
    call: &crate::services::copilot::create_responses::ResponseOutputToolSearchCall,
    tool_search_name: Option<&str>,
    output_index: i64,
) -> Result<Value, AppError> {
    let tool_id = stable_tool_use_id(call.call_id.as_deref(), call.id.as_deref(), output_index);
    let name = tool_search_name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(BRIDGE_TOOL_SEARCH_NAME);
    Ok(json!({
        "type": "tool_use",
        "id": tool_id,
        "name": name,
        "input": parse_tool_search_arguments(&call.arguments),
    }))
}

fn create_custom_tool_use_content_block(
    call: &crate::services::copilot::create_responses::ResponseOutputCustomToolCall,
) -> Value {
    json!({
        "type":"tool_use",
        "id":call.call_id,
        "name":resolve_tool_use_name(&call.name, call.namespace.as_deref()),
        "input":{"input":call.input}
    })
}

/// Mirrors `resolveToolUseName`: prefer a non-empty namespace, else the name.
pub fn resolve_tool_use_name(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) if !ns.trim().is_empty() => ns.to_string(),
        _ => name.to_string(),
    }
}

fn create_compaction_thinking_block(
    item: &crate::services::copilot::create_responses::ResponseOutputCompaction,
) -> Value {
    let id = item.id.as_deref().unwrap_or_default();

    json!({
        "type": "thinking",
        "thinking": THINKING_TEXT,
        "signature": encode_compaction_carrier_signature(&item.encrypted_content, id),
    })
}

#[allow(clippy::result_large_err)]
fn parse_function_call_arguments(raw_arguments: &str) -> Result<Value, AppError> {
    match serde_json::from_str::<Value>(raw_arguments) {
        Ok(parsed) => Ok(parsed),
        Err(_) => Err(invalid_upstream_output(format_args!(
            "function arguments were invalid JSON ({} bytes)",
            raw_arguments.len()
        ))),
    }
}

fn parse_tool_search_arguments(arguments_value: &Value) -> Value {
    format_tool_search_bridge_arguments(arguments_value)
}

fn fallback_content_blocks(output_text: &str) -> Vec<Value> {
    if output_text.is_empty() {
        return Vec::new();
    }
    vec![json!({ "type": "text", "text": output_text })]
}

#[allow(clippy::result_large_err)]
fn map_responses_stop_reason(
    response: &ResponsesResult,
    has_tool_call: bool,
) -> Result<Option<String>, AppError> {
    let status = response.status.as_str();

    if status == "completed" {
        if response.extra.get("end_turn").and_then(Value::as_bool) == Some(false) && !has_tool_call
        {
            return Ok(Some("pause_turn".to_string()));
        }
        if let Some(end_turn) = response.extra.get("end_turn") {
            if !end_turn.is_null() && !end_turn.is_boolean() {
                return Err(invalid_upstream_output("end_turn was not boolean or null"));
            }
        }
        return Ok(Some(
            if has_tool_call {
                "tool_use"
            } else {
                "end_turn"
            }
            .to_string(),
        ));
    }

    if status == "incomplete" {
        let reason = response
            .incomplete_details
            .get("reason")
            .and_then(Value::as_str);
        match reason {
            Some("max_output_tokens") => return Ok(Some("max_tokens".to_string())),
            Some("content_filter") => return Ok(Some("refusal".to_string())),
            _ => return Err(invalid_upstream_output("unsupported incomplete reason")),
        }
    }

    Err(invalid_upstream_output("unsupported response status"))
}

#[allow(clippy::result_large_err)]
fn map_responses_usage(response: &ResponsesResult) -> Result<AnthropicUsage, AppError> {
    let usage =
        validate_typed_responses_usage(response.usage.as_ref()).map_err(invalid_upstream_output)?;
    let cached_input_tokens = usage.cached_input_tokens.unwrap_or_default();

    Ok(AnthropicUsage {
        input_tokens: usage.input_tokens - cached_input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: usage.cached_input_tokens,
        service_tier: None,
        extra: serde_json::Map::new(),
    })
}

#[allow(clippy::result_large_err)]
fn convert_tool_result_content(
    content: Option<&Value>,
) -> Result<FunctionCallOutputContent, AppError> {
    Ok(match content {
        Some(Value::String(s)) => FunctionCallOutputContent::Text(s.clone()),
        Some(Value::Array(arr)) => {
            let mut result: Vec<ResponseInputContent> = Vec::new();
            for block in arr {
                match block_type(block) {
                    Some("text") => result.push(create_text_content_from_block(
                        block,
                        "input_text",
                        "tool_result text block",
                    )?),
                    Some("image") => {
                        result.push(ResponseInputContent::Image(create_image_content(block)?))
                    }
                    Some("document") => {
                        result.push(ResponseInputContent::File(create_file_content(block)?))
                    }
                    Some("tool_reference") => {
                        let tool_name = block
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .filter(|tool_name| !tool_name.trim().is_empty())
                            .ok_or_else(|| {
                                AppError::BadRequest(
                                    "tool_reference.tool_name must be a non-empty string"
                                        .to_string(),
                                )
                            })?;
                        let extra = translated_block_extensions(
                            block,
                            &["type", "tool_name", "cache_control"],
                            &["type", "text"],
                            "tool_reference",
                        )?;
                        result.push(ResponseInputContent::Text(ResponseInputText {
                            block_type: "input_text".to_string(),
                            text: format!("Tool {tool_name} loaded"),
                            extra,
                        }));
                    }
                    Some(other) => {
                        return Err(AppError::BadRequest(format!(
                            "Unsupported tool_result content block type \"{other}\""
                        )))
                    }
                    None => {
                        return Err(AppError::BadRequest(
                            "tool_result content block type must be a non-empty string".to_string(),
                        ))
                    }
                }
            }
            FunctionCallOutputContent::Blocks(result)
        }
        None | Some(Value::Null) => FunctionCallOutputContent::Text(String::new()),
        Some(_) => {
            return Err(AppError::BadRequest(
                "tool_result.content must be a string, array, or null".to_string(),
            ))
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_thinking_maps_to_no_reasoning_effort_or_summary() {
        let payload: AnthropicMessagesPayload = serde_json::from_value(json!({
            "model": "gpt-5.4",
            "max_tokens": 64,
            "thinking": {"type": "disabled"},
            "output_config": {"effort": "high"},
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();

        let translated = translate_anthropic_messages_to_responses_payload(&payload, None).unwrap();
        let reasoning = translated.reasoning.unwrap();

        assert_eq!(reasoning["effort"], "none");
        assert!(reasoning.get("summary").is_none());
    }

    #[test]
    fn unversioned_signature_with_at_is_not_a_responses_carrier() {
        assert!(!is_reasoning_carrier_signature("sig@opaque"));
        let payload: AnthropicMessagesPayload = serde_json::from_value(json!({
            "model": "gpt-5.4",
            "max_tokens": 64,
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "thinking",
                    "thinking": "native reasoning",
                    "signature": "sig@opaque"
                }]
            }]
        }))
        .unwrap();

        let error = translate_anthropic_messages_to_responses_payload(&payload, None)
            .expect_err("native thinking cannot be losslessly represented by Responses");
        assert!(error
            .to_string()
            .contains("Unsupported assistant content block"));
    }

    #[test]
    fn reasoning_signature_splits_on_last_at() {
        let (enc, id) = parse_reasoning_signature("abc@def@id123");
        assert_eq!(enc.as_deref(), Some("abc@def"));
        assert_eq!(id.as_deref(), Some("id123"));
    }

    #[test]
    fn reasoning_signature_no_at_returns_whole() {
        let (enc, id) = parse_reasoning_signature("noatsign");
        assert_eq!(enc.as_deref(), Some("noatsign"));
        assert!(id.is_none());
    }

    #[test]
    fn reasoning_signature_trailing_at_is_invalid() {
        let (enc, id) = parse_reasoning_signature("abc@");
        assert_eq!(enc.as_deref(), Some("abc@"));
        assert!(id.is_none());
    }

    #[test]
    fn optional_reasoning_signature_combinations_round_trip() {
        for (encrypted_content, id) in [
            (Some("enc"), None),
            (None, Some("reasoning-id")),
            (None, None),
            (Some(""), None),
            (None, Some("")),
            (Some(""), Some("")),
        ] {
            let signature = encode_reasoning_signature(encrypted_content, id);
            let (decoded_encrypted_content, decoded_id) = parse_reasoning_signature(&signature);
            assert_eq!(decoded_encrypted_content.as_deref(), encrypted_content);
            assert_eq!(decoded_id.as_deref(), id);
        }
    }

    #[test]
    fn assistant_reasoning_carriers_translate_all_optional_field_combinations() {
        for (encrypted_content, id) in [
            (Some("enc-both"), Some("reasoning-both")),
            (Some("enc-only"), None),
            (None, Some("reasoning-only")),
            (None, None),
            (Some(""), None),
            (None, Some("")),
            (Some(""), Some("")),
        ] {
            let signature = encode_reasoning_signature(encrypted_content, id);
            let payload: AnthropicMessagesPayload = serde_json::from_value(json!({
                "model":"gpt-5.4",
                "max_tokens":128,
                "messages":[{
                    "role":"assistant",
                    "content":[{
                        "type":"thinking",
                        "thinking":"optional reasoning",
                        "signature":signature
                    }]
                }]
            }))
            .expect("Anthropic reasoning history");
            let translated = translate_anthropic_messages_to_responses_payload(&payload, None)
                .expect("translate reasoning history");
            let InputField::Items(items) = translated.input.expect("translated input") else {
                panic!("expected item input");
            };
            assert_eq!(items.len(), 1, "signature: {signature}");
            let ResponseInputItem::Reasoning(reasoning) = &items[0] else {
                panic!("reasoning carrier was dropped: {signature}");
            };
            assert_eq!(reasoning.encrypted_content.as_deref(), encrypted_content);
            assert_eq!(reasoning.id.as_deref(), id);
            assert_eq!(reasoning.summary.len(), 1);
            assert_eq!(reasoning.summary[0].text, "optional reasoning");
        }
    }

    #[test]
    fn compaction_round_trip_splits_on_first_at() {
        let sig = encode_compaction_carrier_signature("enc@with@ats", "theid");
        assert_eq!(sig, "cm1#enc@with@ats@theid");
        let decoded = decode_compaction_carrier_signature(&sig).expect("decode");
        assert_eq!(decoded.encrypted_content, "enc");
        assert_eq!(decoded.id, "with@ats@theid");
    }

    #[test]
    fn compaction_decode_requires_prefix() {
        assert!(decode_compaction_carrier_signature("enc@id").is_none());
    }

    #[test]
    fn idless_compaction_carrier_round_trips() {
        let output = crate::services::copilot::create_responses::ResponseOutputCompaction {
            id: None,
            item_type: "compaction".to_string(),
            encrypted_content: "enc_idless".to_string(),
            extra: Default::default(),
        };
        let block = create_compaction_thinking_block(&output);
        let signature = block["signature"].as_str().expect("signature");
        assert_eq!(signature, "cm1#enc_idless@");

        let decoded = create_compaction_content(signature).expect("decode id-less carrier");
        assert!(decoded.id.is_none());
        assert_eq!(decoded.encrypted_content, "enc_idless");
    }

    #[test]
    fn image_content_url_source_passes_through() {
        let block = json!({
            "type": "image",
            "source": { "type": "url", "url": "https://example.com/pic.png" },
        });
        let img = create_image_content(&block).unwrap();
        assert_eq!(
            img.image_url.as_deref(),
            Some("https://example.com/pic.png")
        );
    }

    #[test]
    fn image_content_base64_source_builds_data_url() {
        let block = json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" },
        });
        let img = create_image_content(&block).unwrap();
        assert_eq!(img.image_url.as_deref(), Some("data:image/png;base64,AAAA"));
    }

    #[test]
    fn image_content_file_source_is_rejected() {
        let block = json!({
            "type": "image",
            "source": { "type": "file", "file_id": "file_123" },
        });
        let err = create_image_content(&block).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn image_content_base64_missing_fields_is_rejected() {
        // Missing data, empty media_type, and blank url must all 400 rather than
        // emit a corrupt `data:;base64,` / empty image reference upstream.
        for block in [
            json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png" } }),
            json!({ "type": "image", "source": { "type": "base64", "media_type": "", "data": "AAAA" } }),
            json!({ "type": "image", "source": { "type": "url", "url": "   " } }),
        ] {
            let err = create_image_content(&block).unwrap_err();
            assert!(matches!(err, AppError::BadRequest(_)), "block: {block}");
        }
    }

    #[test]
    fn file_content_base64_missing_fields_is_rejected() {
        for block in [
            json!({ "type": "document", "source": { "type": "base64", "data": "" } }),
            json!({ "type": "document", "source": { "type": "url", "url": "" } }),
        ] {
            let err = create_file_content(&block).unwrap_err();
            assert!(matches!(err, AppError::BadRequest(_)), "block: {block}");
        }
    }

    #[test]
    fn file_content_file_source_is_rejected() {
        let block = json!({
            "type": "document",
            "source": { "type": "file", "file_id": "file_456" },
        });
        let err = create_file_content(&block).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn url_image_propagates_through_full_translation() {
        let payload: AnthropicMessagesPayload = serde_json::from_str(
            r#"{
                "model": "gpt-5.4",
                "max_tokens": 100,
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "image", "source": { "type": "url", "url": "https://example.com/x.png" } }
                    ]
                }]
            }"#,
        )
        .unwrap();

        let result = translate_anthropic_messages_to_responses_payload(&payload, None).unwrap();
        let items = match result.input.expect("input") {
            InputField::Items(items) => items,
            InputField::Text(_) => panic!("expected items"),
        };
        let ResponseInputItem::Message(msg) = &items[0] else {
            panic!("expected message item");
        };
        let Some(MessageContent::Blocks(blocks)) = &msg.content else {
            panic!("expected block content");
        };
        let ResponseInputContent::Image(img) = &blocks[0] else {
            panic!("expected image content");
        };
        assert_eq!(img.image_url.as_deref(), Some("https://example.com/x.png"));
    }

    #[test]
    fn file_image_rejected_through_full_translation() {
        let payload: AnthropicMessagesPayload = serde_json::from_str(
            r#"{
                "model": "gpt-5.4",
                "max_tokens": 100,
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "image", "source": { "type": "file", "file_id": "file_1" } }
                    ]
                }]
            }"#,
        )
        .unwrap();

        let err = translate_anthropic_messages_to_responses_payload(&payload, None).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn basic_user_message_translation() {
        let payload: AnthropicMessagesPayload = serde_json::from_str(
            r#"{
                "model": "gpt-5.4",
                "max_tokens": 100,
                "messages": [{ "role": "user", "content": "hello world" }]
            }"#,
        )
        .unwrap();

        let result = translate_anthropic_messages_to_responses_payload(&payload, None).unwrap();
        assert_eq!(result.model, "gpt-5.4");
        assert_eq!(result.max_output_tokens, Some(12800));
        assert_eq!(result.temperature, Some(1.0));
        assert_eq!(result.store, Some(false));

        let items = match result.input.expect("input") {
            InputField::Items(items) => items,
            InputField::Text(_) => panic!("expected items"),
        };
        assert_eq!(items.len(), 1);
        match &items[0] {
            ResponseInputItem::Message(msg) => {
                assert_eq!(msg.role, "user");
                match msg.content.as_ref().expect("content") {
                    MessageContent::Text(t) => assert_eq!(t, "hello world"),
                    other => panic!("expected text content, got {other:?}"),
                }
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn assistant_blocks_become_phased_message() {
        let payload: AnthropicMessagesPayload = serde_json::from_str(
            r#"{
                "model": "gpt-5.4",
                "max_tokens": 20000,
                "messages": [{
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "done" }]
                }]
            }"#,
        )
        .unwrap();

        let result = translate_anthropic_messages_to_responses_payload(&payload, None).unwrap();
        assert_eq!(result.max_output_tokens, Some(20000));
        let items = match result.input.expect("input") {
            InputField::Items(items) => items,
            InputField::Text(_) => panic!("expected items"),
        };
        match &items[0] {
            ResponseInputItem::Message(msg) => {
                assert_eq!(msg.role, "assistant");
                assert_eq!(msg.phase.as_deref(), Some("final_answer"));
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn result_with_function_call_maps_to_tool_use() {
        let result: ResponsesResult = serde_json::from_str(
            r#"{
                "id": "resp_1",
                "model": "gpt-5.4",
                "status": "completed",
                "output_text": "",
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call_42",
                        "name": "do_thing",
                        "arguments": "{\"x\":1}",
                        "status": "completed"
                    }
                ],
                "usage": {
                    "input_tokens": 30,
                    "output_tokens": 7,
                    "total_tokens": 37,
                    "input_tokens_details": { "cached_tokens": 10 }
                }
            }"#,
        )
        .unwrap();

        let anthropic = translate_responses_result_to_anthropic(&result, None).unwrap();
        assert_eq!(anthropic.id, "resp_1");
        assert_eq!(anthropic.stop_reason.as_deref(), Some("tool_use"));
        // input_tokens net of cached: 30 - 10 = 20.
        assert_eq!(anthropic.usage.input_tokens, 20);
        assert_eq!(anthropic.usage.output_tokens, 7);
        assert_eq!(anthropic.usage.cache_read_input_tokens, Some(10));

        assert_eq!(anthropic.content.len(), 1);
        let block = &anthropic.content[0];
        assert_eq!(block.get("type").and_then(Value::as_str), Some("tool_use"));
        assert_eq!(block.get("id").and_then(Value::as_str), Some("call_42"));
        assert_eq!(block.get("name").and_then(Value::as_str), Some("do_thing"));
        assert_eq!(block.get("input").and_then(|i| i.get("x")), Some(&json!(1)));
    }

    #[test]
    fn result_message_text_and_reasoning() {
        use crate::services::copilot::create_responses::{
            ResponseOutputMessage, ResponseOutputReasoning, ResponseReasoningBlock,
        };

        // Built via the typed enum directly. (The `ResponseOutputItem`
        // deserializer dispatches on the `type` discriminant, so wire reasoning
        // items now classify correctly; this test exercises the mapping logic.)
        let result = ResponsesResult {
            id: "resp_2".to_string(),
            model: "gpt-5.4".to_string(),
            status: "completed".to_string(),
            output_text: Some("ignored fallback".to_string()),
            output: vec![
                ResponseOutputItem::Reasoning(ResponseOutputReasoning {
                    id: Some("rs_1".to_string()),
                    item_type: "reasoning".to_string(),
                    summary: vec![ResponseReasoningBlock {
                        block_type: "summary_text".to_string(),
                        text: "pondering".to_string(),
                        extra: Default::default(),
                    }],
                    content: None,
                    encrypted_content: Some("ENC".to_string()),
                    status: None,
                    extra: Default::default(),
                }),
                ResponseOutputItem::Message(ResponseOutputMessage {
                    id: Some("msg_1".to_string()),
                    item_type: "message".to_string(),
                    role: "assistant".to_string(),
                    status: Some("completed".to_string()),
                    content: vec![ResponseOutputContentBlock::Text(
                        crate::services::copilot::create_responses::ResponseOutputText {
                            block_type: "output_text".to_string(),
                            text: "the answer".to_string(),
                            annotations: Some(vec![]),
                            extra: Default::default(),
                        },
                    )],
                    extra: Default::default(),
                }),
            ],
            ..Default::default()
        };

        let anthropic = translate_responses_result_to_anthropic(&result, None).unwrap();
        assert_eq!(anthropic.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(anthropic.content.len(), 2);
        assert_eq!(
            anthropic.content[0].get("type").and_then(Value::as_str),
            Some("thinking")
        );
        let expected_signature = encode_reasoning_signature(Some("ENC"), Some("rs_1"));
        assert_eq!(
            anthropic.content[0]
                .get("signature")
                .and_then(Value::as_str),
            Some(expected_signature.as_str())
        );
        assert_eq!(
            anthropic.content[1].get("text").and_then(Value::as_str),
            Some("the answer")
        );
    }

    #[test]
    fn reasoning_summary_and_content_share_lossless_framing() {
        let result: ResponsesResult = serde_json::from_value(json!({
            "id":"resp_reasoning_content",
            "model":"gpt-5.4",
            "status":"completed",
            "output":[
                {
                    "type":"reasoning",
                    "id":"reasoning-content",
                    "encrypted_content":"opaque",
                    "summary":[
                        {"type":"summary_text","text":" summary "},
                        {"type":"summary_text","text":""}
                    ],
                    "content":[
                        {"type":"reasoning_text","text":" raw "},
                        {"type":"reasoning_text","text":"second\n"}
                    ]
                }
            ]
        }))
        .unwrap();

        let anthropic = translate_responses_result_to_anthropic(&result, None).unwrap();
        let expected = [" summary ", "", " raw ", "second\n"].join(REASONING_SUMMARY_SEPARATOR);
        assert_eq!(anthropic.content[0]["thinking"], expected);
        assert_eq!(
            anthropic.content[0]["signature"],
            encode_reasoning_signature(Some("opaque"), Some("reasoning-content"))
        );
    }

    #[test]
    fn malformed_tool_call_is_rejected_instead_of_dropped() {
        use crate::services::copilot::create_responses::ResponseOutputFunctionCall;

        let result = ResponsesResult {
            id: "resp_x".to_string(),
            model: "claude-opus-4-8".to_string(),
            status: "completed".to_string(),
            output: vec![ResponseOutputItem::FunctionCall(
                ResponseOutputFunctionCall {
                    id: None,
                    item_type: "function_call".to_string(),
                    call_id: String::new(),
                    name: String::new(),
                    arguments: "{}".to_string(),
                    status: None,
                    namespace: None,
                    extra: Default::default(),
                },
            )],
            ..Default::default()
        };

        assert!(translate_responses_result_to_anthropic(&result, None).is_err());
    }

    #[test]
    fn valid_tool_call_still_reports_tool_use() {
        use crate::services::copilot::create_responses::ResponseOutputFunctionCall;

        let result = ResponsesResult {
            id: "resp_y".to_string(),
            model: "gpt-5.4".to_string(),
            status: "completed".to_string(),
            output: vec![ResponseOutputItem::FunctionCall(
                ResponseOutputFunctionCall {
                    id: None,
                    item_type: "function_call".to_string(),
                    call_id: "call_1".to_string(),
                    name: "list_files".to_string(),
                    arguments: "{}".to_string(),
                    status: None,
                    namespace: None,
                    extra: Default::default(),
                },
            )],
            ..Default::default()
        };

        let anthropic = translate_responses_result_to_anthropic(&result, None).unwrap();
        assert!(anthropic
            .content
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")));
        assert_eq!(anthropic.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn empty_reasoning_summary_yields_thinking_default() {
        use crate::services::copilot::create_responses::ResponseOutputReasoning;

        // Built typed for the same union-ordering reason noted above.
        let result = ResponsesResult {
            id: "resp_3".to_string(),
            model: "gpt-5.4".to_string(),
            status: "completed".to_string(),
            output: vec![ResponseOutputItem::Reasoning(ResponseOutputReasoning {
                id: Some("rs".to_string()),
                item_type: "reasoning".to_string(),
                summary: vec![],
                content: None,
                encrypted_content: Some("E".to_string()),
                status: None,
                extra: Default::default(),
            })],
            ..Default::default()
        };

        let anthropic = translate_responses_result_to_anthropic(&result, None).unwrap();
        assert_eq!(anthropic.content.len(), 1);
        assert_eq!(
            anthropic.content[0].get("thinking").and_then(Value::as_str),
            Some(THINKING_TEXT)
        );
        let expected_signature = encode_reasoning_signature(Some("E"), Some("rs"));
        assert_eq!(
            anthropic.content[0]
                .get("signature")
                .and_then(Value::as_str),
            Some(expected_signature.as_str())
        );
    }

    #[test]
    fn aggregate_empty_reasoning_uses_placeholder_only_for_opaque_carriers() {
        use crate::services::copilot::create_responses::{
            ResponseOutputReasoning, ResponseReasoningBlock,
        };

        let summaries = vec![
            vec![],
            vec![ResponseReasoningBlock {
                block_type: "summary_text".to_string(),
                text: String::new(),
                extra: Default::default(),
            }],
            vec![
                ResponseReasoningBlock {
                    block_type: "summary_text".to_string(),
                    text: " \n\t".to_string(),
                    extra: Default::default(),
                },
                ResponseReasoningBlock {
                    block_type: "summary_text".to_string(),
                    text: String::new(),
                    extra: Default::default(),
                },
            ],
        ];

        for summary in summaries {
            let carrier = ResponsesResult {
                id: "resp_carrier".to_string(),
                model: "gpt-5.4".to_string(),
                status: "completed".to_string(),
                output: vec![ResponseOutputItem::Reasoning(ResponseOutputReasoning {
                    id: Some("reasoning-id".to_string()),
                    item_type: "reasoning".to_string(),
                    summary: summary.clone(),
                    content: None,
                    encrypted_content: Some("encrypted".to_string()),
                    status: None,
                    extra: Default::default(),
                })],
                ..Default::default()
            };
            let anthropic = translate_responses_result_to_anthropic(&carrier, None).unwrap();
            assert_eq!(anthropic.content.len(), 1, "summary: {summary:?}");
            assert_eq!(anthropic.content[0]["type"], "thinking");
            assert_eq!(anthropic.content[0]["thinking"], THINKING_TEXT);
            assert_eq!(
                anthropic.content[0]["signature"],
                encode_reasoning_signature(Some("encrypted"), Some("reasoning-id"))
            );

            let carrier_free = ResponsesResult {
                id: "resp_carrier_free".to_string(),
                model: "gpt-5.4".to_string(),
                status: "completed".to_string(),
                output: vec![ResponseOutputItem::Reasoning(ResponseOutputReasoning {
                    id: None,
                    item_type: "reasoning".to_string(),
                    summary: summary.clone(),
                    content: None,
                    encrypted_content: None,
                    status: None,
                    extra: Default::default(),
                })],
                ..Default::default()
            };
            let anthropic = translate_responses_result_to_anthropic(&carrier_free, None).unwrap();
            assert!(
                !anthropic
                    .content
                    .iter()
                    .any(|block| block["type"] == "thinking"),
                "carrier-free empty summary invented thinking: {summary:?}"
            );
        }

        for (encrypted_content, id) in [(Some(""), None), (None, Some("")), (Some(""), Some(""))] {
            let result = ResponsesResult {
                id: "resp_empty_values".to_string(),
                model: "gpt-5.4".to_string(),
                status: "completed".to_string(),
                output: vec![ResponseOutputItem::Reasoning(ResponseOutputReasoning {
                    id: id.map(str::to_string),
                    item_type: "reasoning".to_string(),
                    summary: vec![ResponseReasoningBlock {
                        block_type: "summary_text".to_string(),
                        text: " \n".to_string(),
                        extra: Default::default(),
                    }],
                    content: None,
                    encrypted_content: encrypted_content.map(str::to_string),
                    status: None,
                    extra: Default::default(),
                })],
                ..Default::default()
            };
            let anthropic = translate_responses_result_to_anthropic(&result, None).unwrap();
            assert_eq!(anthropic.content.len(), 1);
            assert_eq!(anthropic.content[0]["thinking"], THINKING_TEXT);
            assert_eq!(
                anthropic.content[0]["signature"],
                encode_reasoning_signature(encrypted_content, id)
            );
        }
    }

    #[test]
    fn reasoning_summary_policy_preserves_whitespace_parts_and_round_trips() {
        use crate::services::copilot::create_responses::{
            ResponseOutputReasoning, ResponseReasoningBlock,
        };

        for segments in [vec!["  analysis  "], vec!["  first ", "", "\tsecond\n", ""]] {
            let result = ResponsesResult {
                id: "resp_segments".to_string(),
                model: "gpt-5.4".to_string(),
                status: "completed".to_string(),
                output: vec![ResponseOutputItem::Reasoning(ResponseOutputReasoning {
                    id: Some("reasoning-id".to_string()),
                    item_type: "reasoning".to_string(),
                    summary: segments
                        .iter()
                        .map(|text| ResponseReasoningBlock {
                            block_type: "summary_text".to_string(),
                            text: (*text).to_string(),
                            extra: Default::default(),
                        })
                        .collect(),
                    content: None,
                    encrypted_content: Some("encrypted".to_string()),
                    status: None,
                    extra: Default::default(),
                })],
                ..Default::default()
            };
            let anthropic = translate_responses_result_to_anthropic(&result, None).unwrap();
            let expected = segments.join(REASONING_SUMMARY_SEPARATOR);
            assert_eq!(anthropic.content[0]["thinking"], expected);
            assert_eq!(
                anthropic.content[0]["signature"],
                encode_reasoning_signature(Some("encrypted"), Some("reasoning-id"))
            );

            let history: AnthropicMessagesPayload = serde_json::from_value(json!({
                "model":"gpt-5.4",
                "max_tokens":128,
                "messages":[{
                    "role":"assistant",
                    "content":[anthropic.content[0].clone()]
                }]
            }))
            .unwrap();
            let translated =
                translate_anthropic_messages_to_responses_payload(&history, None).unwrap();
            let InputField::Items(items) = translated.input.unwrap() else {
                panic!("expected item input");
            };
            let ResponseInputItem::Reasoning(reasoning) = &items[0] else {
                panic!("expected reasoning item");
            };
            let round_trip: Vec<&str> = reasoning
                .summary
                .iter()
                .map(|block| block.text.as_str())
                .collect();
            assert_eq!(round_trip, segments);
        }
    }
}
