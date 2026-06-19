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
//! Reasoning-signature and compaction-carrier encode/decode reproduce the TS
//! byte-for-byte: reasoning splits on the LAST `@` (`encrypted_content@id`),
//! compaction carriers use the `cm1#...@id` prefix form and split on the FIRST
//! `@`. Byte exactness matters for Copilot prompt-cache hits.

use serde_json::{json, Map, Value};

use crate::libs::config::{get_extra_prompt_for_model, get_reasoning_effort_for_model};
use crate::libs::error::AppError;
use crate::libs::request_context::request_context_store;
use crate::libs::tool_search::{
    format_tool_search_bridge_arguments, is_bridge_tool_search_name, is_deferred_tool_name,
    list_deferred_tool_names, normalize_tool_search_bridge_arguments,
    parse_mcp_tool_search_sentinel, select_deferred_tools_by_names,
    should_enable_responses_tool_search, BRIDGE_TOOL_SEARCH_NAME,
};
use crate::libs::utils::parse_user_id_metadata;
use crate::routes::messages::anthropic_types::{
    AnthropicInputMessage, AnthropicMessagesPayload, AnthropicResponse, AnthropicTool,
    AnthropicUsage,
};
use crate::services::copilot::create_responses::{
    FunctionCallOutputContent, InputField, MessageContent, ReasoningSummaryText,
    ResponseFunctionCallOutputItem, ResponseFunctionToolCallItem, ResponseInputCompaction,
    ResponseInputContent, ResponseInputFile, ResponseInputImage, ResponseInputItem,
    ResponseInputMessage, ResponseInputReasoning, ResponseInputText, ResponseOutputContentBlock,
    ResponseOutputItem, ResponseToolSearchCallItem, ResponseToolSearchOutputItem, ResponsesPayload,
    ResponsesResult,
};

const MESSAGE_TYPE: &str = "message";
const COMPACTION_SIGNATURE_PREFIX: &str = "cm1#";
const COMPACTION_SIGNATURE_SEPARATOR: &str = "@";

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

// ---------------------------------------------------------------------------
// Signature codecs
// ---------------------------------------------------------------------------

struct CompactionCarrier {
    id: String,
    encrypted_content: String,
}

/// `cm1#${encrypted_content}@${id}`.
pub fn encode_compaction_carrier_signature(encrypted_content: &str, id: &str) -> String {
    format!("{COMPACTION_SIGNATURE_PREFIX}{encrypted_content}{COMPACTION_SIGNATURE_SEPARATOR}{id}")
}

/// Inverse of [`encode_compaction_carrier_signature`]. Splits on the FIRST `@`
/// after the `cm1#` prefix. Returns `None` when the shape does not match.
fn decode_compaction_carrier_signature(signature: &str) -> Option<CompactionCarrier> {
    let raw = signature.strip_prefix(COMPACTION_SIGNATURE_PREFIX)?;

    // indexOf — first occurrence (byte index, ASCII '@').
    let separator_index = raw.find(COMPACTION_SIGNATURE_SEPARATOR)?;

    // separatorIndex <= 0 || separatorIndex === raw.length - 1 -> undefined
    if separator_index == 0 || separator_index == raw.len() - 1 {
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

/// Splits a reasoning signature on the LAST `@` into `(encrypted_content, id)`.
/// When there is no valid split, the whole signature is the encrypted content
/// and the id is empty.
fn parse_reasoning_signature(signature: &str) -> (String, String) {
    match signature.rfind('@') {
        Some(idx) if idx != 0 && idx != signature.len() - 1 => (
            signature[..idx].to_string(),
            signature[idx + 1..].to_string(),
        ),
        _ => (signature.to_string(), String::new()),
    }
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic Messages -> Responses payload
// ---------------------------------------------------------------------------

struct TranslationState {
    /// Original Anthropic tools as `Value` for interop with `tool_search`.
    original_tools: Vec<Value>,
    tool_search_enabled: bool,
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
fn tools_as_values(tools: Option<&Vec<AnthropicTool>>) -> Vec<Value> {
    tools
        .map(|ts| {
            ts.iter()
                .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
                .collect()
        })
        .unwrap_or_default()
}

#[allow(clippy::result_large_err)]
pub fn translate_anthropic_messages_to_responses_payload(
    payload: &AnthropicMessagesPayload,
    subagent_agent_id: Option<&str>,
) -> Result<ResponsesPayload, AppError> {
    let mut input: Vec<ResponseInputItem> = Vec::new();
    let apply_phase = should_apply_phase(&payload.model);

    let tool_values = tools_as_values(payload.tools.as_ref());
    let tool_slice: Option<&[Value]> = if tool_values.is_empty() {
        None
    } else {
        Some(tool_values.as_slice())
    };
    let tool_search_enabled = should_enable_responses_tool_search(&payload.model, tool_slice);

    let mut state = TranslationState {
        original_tools: tool_values.clone(),
        tool_search_enabled,
        tool_use_name_by_id: std::collections::HashMap::new(),
    };

    for message in &payload.messages {
        let items = translate_message(message, &payload.model, apply_phase, &mut state)?;
        input.extend(items);
    }

    let has_original_tools = payload.tools.as_ref().is_some_and(|t| !t.is_empty());

    let translated_tools = convert_anthropic_tools(&tool_values, tool_search_enabled);
    let tool_choice = convert_anthropic_tool_choice(payload, tool_search_enabled);

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
        .map(|m| serde_json::to_value(m).unwrap_or(Value::Null));

    // `max_tokens` is required on a real /v1/messages request; default a
    // missing value to 0 so the 12800 floor applies.
    let max_output_tokens = payload.max_tokens.unwrap_or(0).max(12800);

    let resolved_effort = get_reasoning_effort_for_model(&payload.model);
    tracing::info!(
        target: "audit",
        model = %payload.model,
        effort = %resolved_effort,
        api = "responses",
        "resolved reasoning effort"
    );
    let reasoning = json!({
        "effort": resolved_effort,
        "summary": "detailed",
    });

    let mut responses_payload = ResponsesPayload {
        model: payload.model.clone(),
        instructions: translate_system_prompt(payload.system.as_ref(), &payload.model),
        input: Some(InputField::Items(input)),
        tools: translated_tools,
        tool_choice: Some(tool_choice),
        temperature: Some(1.0), // reasoning high temperature fixed to 1
        top_p: payload.top_p,
        max_output_tokens: Some(max_output_tokens),
        metadata: metadata_value,
        stream: payload.stream,
        safety_identifier: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        parallel_tool_calls: Some(true),
        store: Some(false),
        reasoning: Some(reasoning),
        context_management: None,
        include: Some(vec!["reasoning.encrypted_content".to_string()]),
        service_tier: None,
        extra: Map::new(),
    };

    if has_original_tools {
        responses_payload.prompt_cache_key = prompt_cache_key;
    }

    Ok(responses_payload)
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
) -> Result<Vec<ResponseInputItem>, AppError> {
    if message.role == "user" {
        translate_user_message(message, state)
    } else {
        Ok(translate_assistant_message(
            message,
            model,
            apply_phase,
            state,
        ))
    }
}

#[allow(clippy::result_large_err)]
fn translate_user_message(
    message: &AnthropicInputMessage,
    state: &mut TranslationState,
) -> Result<Vec<ResponseInputItem>, AppError> {
    if let Some(text) = message.content.as_str() {
        return Ok(vec![create_message(
            "user",
            MessageContent::Text(text.to_string()),
            None,
        )]);
    }

    let Some(blocks) = message.content.as_array() else {
        return Ok(Vec::new());
    };

    let mut items: Vec<ResponseInputItem> = Vec::new();
    let mut pending: Vec<ResponseInputContent> = Vec::new();

    for block in blocks {
        if block_type(block) == Some("tool_result") {
            flush_pending_content(&mut pending, &mut items, "user", None);
            items.push(create_tool_call_output(block, state)?);
            continue;
        }

        let converted = translate_user_content_block(block)?;
        pending.extend(converted);
    }

    flush_pending_content(&mut pending, &mut items, "user", None);
    Ok(items)
}

fn translate_assistant_message(
    message: &AnthropicInputMessage,
    model: &str,
    apply_phase: bool,
    state: &mut TranslationState,
) -> Vec<ResponseInputItem> {
    let assistant_phase = resolve_assistant_phase(model, &message.content, apply_phase);

    if let Some(text) = message.content.as_str() {
        return vec![create_message(
            "assistant",
            MessageContent::Text(text.to_string()),
            assistant_phase,
        )];
    }

    let Some(blocks) = message.content.as_array() else {
        return Vec::new();
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
            );
            items.push(create_tool_call(block, state));
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
                    );
                    items.push(ResponseInputItem::Compaction(compaction));
                    continue;
                }

                if signature.contains('@') {
                    flush_pending_content(
                        &mut pending,
                        &mut items,
                        "assistant",
                        assistant_phase.clone(),
                    );
                    items.push(ResponseInputItem::Reasoning(create_reasoning_content(
                        block,
                    )));
                    continue;
                }
            }
        }

        if let Some(converted) = translate_assistant_content_block(block) {
            pending.push(converted);
        }
    }

    flush_pending_content(&mut pending, &mut items, "assistant", assistant_phase);
    items
}

#[allow(clippy::result_large_err)]
fn translate_user_content_block(block: &Value) -> Result<Vec<ResponseInputContent>, AppError> {
    Ok(match block_type(block) {
        Some("text") => vec![create_text_content(text_field(block))],
        Some("image") => vec![ResponseInputContent::Image(create_image_content(block)?)],
        Some("document") => vec![ResponseInputContent::File(create_file_content(block)?)],
        _ => Vec::new(),
    })
}

fn translate_assistant_content_block(block: &Value) -> Option<ResponseInputContent> {
    match block_type(block) {
        Some("text") => Some(create_output_text_content(text_field(block))),
        _ => None,
    }
}

fn text_field(block: &Value) -> String {
    block
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn flush_pending_content(
    pending: &mut Vec<ResponseInputContent>,
    target: &mut Vec<ResponseInputItem>,
    role: &str,
    phase: Option<String>,
) {
    if pending.is_empty() {
        return;
    }

    let content = std::mem::take(pending);
    target.push(create_message(role, MessageContent::Blocks(content), phase));
}

fn create_message(role: &str, content: MessageContent, phase: Option<String>) -> ResponseInputItem {
    let phase = if role == "assistant" { phase } else { None };
    ResponseInputItem::Message(ResponseInputMessage {
        item_type: Some(MESSAGE_TYPE.to_string()),
        role: role.to_string(),
        content: Some(content),
        status: None,
        phase,
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

fn create_text_content(text: String) -> ResponseInputContent {
    ResponseInputContent::Text(ResponseInputText {
        block_type: "input_text".to_string(),
        text,
    })
}

fn create_output_text_content(text: String) -> ResponseInputContent {
    ResponseInputContent::Text(ResponseInputText {
        block_type: "output_text".to_string(),
        text,
    })
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

    Ok(ResponseInputImage {
        block_type: "input_image".to_string(),
        image_url: Some(image_url),
        file_id: None,
        detail: "auto".to_string(),
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

    Ok(ResponseInputFile {
        block_type: "input_file".to_string(),
        file_data: Some(file_data),
        file_id: None,
        filename: Some(filename),
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

fn create_reasoning_content(block: &Value) -> ResponseInputReasoning {
    let signature = block
        .get("signature")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (encrypted_content, id) = parse_reasoning_signature(signature);
    let raw_thinking = block
        .get("thinking")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let thinking = if raw_thinking == THINKING_TEXT {
        ""
    } else {
        raw_thinking
    };
    let summary = if thinking.is_empty() {
        Vec::new()
    } else {
        vec![ReasoningSummaryText {
            block_type: "summary_text".to_string(),
            text: thinking.to_string(),
        }]
    };
    ResponseInputReasoning {
        id: Some(id),
        item_type: "reasoning".to_string(),
        summary,
        encrypted_content,
    }
}

fn create_compaction_content(signature: &str) -> Option<ResponseInputCompaction> {
    let compaction = decode_compaction_carrier_signature(signature)?;
    Some(ResponseInputCompaction {
        id: compaction.id,
        item_type: "compaction".to_string(),
        encrypted_content: compaction.encrypted_content,
    })
}

// ---------------------------------------------------------------------------
// Tool-call input items
// ---------------------------------------------------------------------------

fn create_function_tool_call(
    block: &Value,
    state: &TranslationState,
) -> ResponseFunctionToolCallItem {
    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = serde_json::to_string(block.get("input").unwrap_or(&Value::Null))
        .unwrap_or_else(|_| "null".to_string());
    let namespace = if state.tool_search_enabled && is_deferred_tool_name(name) {
        Some(name.to_string())
    } else {
        None
    };
    ResponseFunctionToolCallItem {
        item_type: "function_call".to_string(),
        call_id: id.to_string(),
        name: name.to_string(),
        arguments,
        status: Some("completed".to_string()),
        namespace,
    }
}

fn create_tool_search_call(block: &Value) -> ResponseToolSearchCallItem {
    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    ResponseToolSearchCallItem {
        item_type: "tool_search_call".to_string(),
        call_id: id.to_string(),
        arguments: normalize_tool_search_bridge_arguments(&input),
        execution: Some("client".to_string()),
        status: Some("completed".to_string()),
    }
}

fn create_tool_call(block: &Value, state: &TranslationState) -> ResponseInputItem {
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if state.tool_search_enabled && is_bridge_tool_search_name(name) {
        ResponseInputItem::ToolSearchCall(create_tool_search_call(block))
    } else {
        ResponseInputItem::FunctionToolCall(create_function_tool_call(block, state))
    }
}

#[allow(clippy::result_large_err)]
fn create_function_call_output(block: &Value) -> Result<ResponseFunctionCallOutputItem, AppError> {
    let call_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ResponseFunctionCallOutputItem {
        item_type: "function_call_output".to_string(),
        call_id: call_id.to_string(),
        output: convert_tool_result_content(block.get("content"))?,
        status: Some(if is_error { "incomplete" } else { "completed" }.to_string()),
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
        .unwrap_or_default();
    let tool_use_name = state
        .tool_use_name_by_id
        .get(tool_use_id)
        .map(String::as_str)
        .unwrap_or("");
    if state.tool_search_enabled && is_bridge_tool_search_name(tool_use_name) {
        Ok(ResponseInputItem::ToolSearchOutput(
            create_tool_search_output(block, &state.original_tools),
        ))
    } else {
        Ok(ResponseInputItem::FunctionCallOutput(
            create_function_call_output(block)?,
        ))
    }
}

fn create_tool_search_output(
    block: &Value,
    original_tools: &[Value],
) -> ResponseToolSearchOutputItem {
    let content = block.get("content");
    let call_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let referenced_tool_names = resolve_tool_search_referenced_tool_names(content, original_tools);
    let tools: Vec<Value> = referenced_tool_names
        .iter()
        .map(|tool_name| {
            let tool = resolve_deferred_tool(tool_name, original_tools);
            convert_deferred_tool_to_namespace(&tool)
        })
        .collect();

    ResponseToolSearchOutputItem {
        item_type: "tool_search_output".to_string(),
        call_id: call_id.to_string(),
        tools,
        execution: Some("client".to_string()),
        status: Some(if is_error { "incomplete" } else { "completed" }.to_string()),
    }
}

fn resolve_tool_search_referenced_tool_names(
    content: Option<&Value>,
    original_tools: &[Value],
) -> Vec<String> {
    let explicit = extract_tool_reference_names(content);
    if !explicit.is_empty() {
        return unique_tool_names(explicit);
    }

    if let Some(sentinel) = extract_mcp_tool_search_sentinel(content) {
        let names_value = Value::Array(sentinel.names.into_iter().map(Value::String).collect());
        return select_deferred_tools_by_names(&names_value, original_tools)
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
    }

    Vec::new()
}

fn extract_tool_reference_names(content: Option<&Value>) -> Vec<String> {
    let Some(arr) = content.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter(|b| block_type(b) == Some("tool_reference"))
        .filter_map(|b| {
            b.get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn extract_mcp_tool_search_sentinel(
    content: Option<&Value>,
) -> Option<crate::libs::tool_search::McpToolSearchSentinel> {
    match content {
        Some(Value::String(s)) => parse_mcp_tool_search_sentinel(s),
        Some(Value::Array(arr)) => {
            for block in arr {
                if block_type(block) != Some("text") {
                    continue;
                }
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if let Some(sentinel) = parse_mcp_tool_search_sentinel(text) {
                        return Some(sentinel);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn resolve_deferred_tool(tool_name: &str, original_tools: &[Value]) -> Value {
    let found = original_tools
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(tool_name));
    if let Some(tool) = found {
        if tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(is_deferred_tool_name)
        {
            return tool.clone();
        }
    }
    // The TS throws an HTTPError(400). Pure-function port: emit a placeholder
    // namespace tool with the requested name so downstream serialization stays
    // total. (The real error path lands with the route handler in a later phase.)
    json!({ "name": tool_name })
}

fn unique_tool_names(tool_names: Vec<String>) -> Vec<String> {
    let mut set: indexmap::IndexSet<String> = indexmap::IndexSet::new();
    for name in tool_names {
        set.insert(name);
    }
    set.into_iter().collect()
}

// ---------------------------------------------------------------------------
// System prompt + tool conversion
// ---------------------------------------------------------------------------

fn translate_system_prompt(system: Option<&Value>, model: &str) -> Option<String> {
    let system = system?;
    if system.is_null() {
        return None;
    }

    let extra_prompt = get_extra_prompt_for_model(model);

    if let Some(s) = system.as_str() {
        if s.is_empty() {
            return None;
        }
        return Some(format!("{s}{extra_prompt}"));
    }

    let blocks = system.as_array()?;
    let parts: Vec<String> = blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if index == 0 {
                format!("{text}\n\n{extra_prompt}\n\n")
            } else {
                text.to_string()
            }
        })
        .collect();
    let text = parts.join(" ");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn convert_anthropic_tools(tools: &[Value], tool_search_enabled: bool) -> Option<Vec<Value>> {
    if tools.is_empty() {
        return None;
    }

    let mut converted: Vec<Value> = Vec::new();
    let mut added_tool_search = false;
    let searchable_tool_names = if tool_search_enabled {
        list_deferred_tool_names(tools)
    } else {
        Vec::new()
    };

    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");

        if is_bridge_tool_search_name(name) {
            if tool_search_enabled && !added_tool_search {
                converted.push(create_responses_tool_search_definition(
                    &searchable_tool_names,
                ));
                added_tool_search = true;
            }
            continue;
        }

        if tool_search_enabled && is_deferred_tool_name(name) {
            converted.push(convert_deferred_tool_to_namespace(tool));
            continue;
        }

        converted.push(convert_tool_to_function(tool));
    }

    Some(converted)
}

fn create_responses_tool_search_definition(searchable_tool_names: &[String]) -> Value {
    json!({
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
    })
}

fn convert_tool_to_function(tool: &Value) -> Value {
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
    let parameters = normalize_tool_schema(tool.get("input_schema"));
    let mut obj = Map::new();
    obj.insert("type".to_string(), json!("function"));
    obj.insert("name".to_string(), json!(name));
    obj.insert("parameters".to_string(), parameters);
    obj.insert("strict".to_string(), json!(false));
    if let Some(description) = tool.get("description").and_then(Value::as_str) {
        if !description.is_empty() {
            obj.insert("description".to_string(), json!(description));
        }
    }
    Value::Object(obj)
}

fn convert_deferred_tool_to_namespace(tool: &Value) -> Value {
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
    let parameters = normalize_tool_schema(tool.get("input_schema"));
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty());

    let mut inner = Map::new();
    inner.insert("type".to_string(), json!("function"));
    inner.insert("name".to_string(), json!(name));
    inner.insert("parameters".to_string(), parameters);
    inner.insert("strict".to_string(), json!(false));
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
    Value::Object(obj)
}

fn convert_anthropic_tool_choice(
    payload: &AnthropicMessagesPayload,
    tool_search_enabled: bool,
) -> Value {
    let Some(choice) = payload.tool_choice.as_ref() else {
        return json!("auto");
    };

    match choice.kind.as_str() {
        "auto" => json!("auto"),
        "any" => json!("required"),
        "tool" => {
            if tool_search_enabled {
                if let Some(name) = choice.name.as_deref() {
                    if is_bridge_tool_search_name(name) {
                        return json!("auto");
                    }
                }
            }
            match choice.name.as_deref() {
                Some(name) => json!({ "type": "function", "name": name }),
                None => json!("auto"),
            }
        }
        "none" => json!("none"),
        _ => json!("auto"),
    }
}

// ---------------------------------------------------------------------------
// Result translation: Responses output -> Anthropic message response
// ---------------------------------------------------------------------------

pub fn translate_responses_result_to_anthropic(
    response: &ResponsesResult,
    tool_search_name: Option<&str>,
) -> AnthropicResponse {
    let content_blocks = map_output_to_anthropic_content(&response.output, tool_search_name);
    let usage = map_responses_usage(response);

    let anthropic_content = if content_blocks.is_empty() {
        fallback_content_blocks(&response.output_text)
    } else {
        content_blocks
    };

    // Derive `tool_use` stop_reason from whether a tool_use block was actually
    // emitted, not from the mere presence of a FunctionCall/ToolSearchCall output
    // item. A tool call whose id/name decoded to "" (e.g. upstream sent null,
    // now coerced by `null_to_default`) is dropped by the block builders, so
    // reporting `tool_use` here would leave stop_reason inconsistent with the
    // content — a mismatch that can stall agent loops.
    let has_tool_use = anthropic_content
        .iter()
        .any(|b| block_type(b) == Some("tool_use"));
    let stop_reason = map_responses_stop_reason(response, has_tool_use);

    AnthropicResponse {
        id: response.id.clone(),
        kind: "message".to_string(),
        role: "assistant".to_string(),
        content: anthropic_content,
        model: response.model.clone(),
        stop_reason,
        stop_sequence: None,
        usage,
        extra: serde_json::Map::new(),
    }
}

fn map_output_to_anthropic_content(
    output: &[ResponseOutputItem],
    tool_search_name: Option<&str>,
) -> Vec<Value> {
    let mut content_blocks: Vec<Value> = Vec::new();

    for item in output {
        match item {
            ResponseOutputItem::Reasoning(reasoning) => {
                let thinking_text = extract_reasoning_text(reasoning);
                if !thinking_text.is_empty() {
                    let signature = format!(
                        "{}@{}",
                        reasoning.encrypted_content.clone().unwrap_or_default(),
                        reasoning.id
                    );
                    content_blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking_text,
                        "signature": signature,
                    }));
                }
            }
            ResponseOutputItem::FunctionCall(call) => {
                if let Some(block) = create_tool_use_content_block(call) {
                    content_blocks.push(block);
                }
            }
            ResponseOutputItem::ToolSearchCall(call) => {
                if let Some(block) = create_tool_search_use_content_block(call, tool_search_name) {
                    content_blocks.push(block);
                }
            }
            ResponseOutputItem::ToolSearchOutput(_) => {}
            ResponseOutputItem::Message(message) => {
                let combined = combine_message_text_content(message.content.as_deref());
                if !combined.is_empty() {
                    content_blocks.push(json!({ "type": "text", "text": combined }));
                }
            }
            ResponseOutputItem::Compaction(compaction) => {
                if let Some(block) = create_compaction_thinking_block(compaction) {
                    content_blocks.push(block);
                }
            }
            ResponseOutputItem::Other(value) => {
                // Future compatibility: pull text out of an unknown `content` array.
                let content = value
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|arr| combine_message_text_content_value(arr));
                if let Some(combined) = content {
                    if !combined.is_empty() {
                        content_blocks.push(json!({ "type": "text", "text": combined }));
                    }
                }
            }
        }
    }

    content_blocks
}

fn combine_message_text_content(content: Option<&[ResponseOutputContentBlock]>) -> String {
    let Some(blocks) = content else {
        return String::new();
    };

    let mut aggregated = String::new();
    for block in blocks {
        match block {
            ResponseOutputContentBlock::Text(t) => aggregated.push_str(&t.text),
            ResponseOutputContentBlock::Refusal(r) => aggregated.push_str(&r.refusal),
            ResponseOutputContentBlock::Other(value) => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    aggregated.push_str(text);
                } else if let Some(reasoning) = value.get("reasoning").and_then(Value::as_str) {
                    aggregated.push_str(reasoning);
                }
            }
        }
    }
    aggregated
}

fn combine_message_text_content_value(blocks: &[Value]) -> String {
    let mut aggregated = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("output_text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                aggregated.push_str(text);
                continue;
            }
        }
        if block.get("type").and_then(Value::as_str) == Some("refusal") {
            if let Some(refusal) = block.get("refusal").and_then(Value::as_str) {
                aggregated.push_str(refusal);
                continue;
            }
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            aggregated.push_str(text);
            continue;
        }
        if let Some(reasoning) = block.get("reasoning").and_then(Value::as_str) {
            aggregated.push_str(reasoning);
        }
    }
    aggregated
}

fn extract_reasoning_text(
    item: &crate::services::copilot::create_responses::ResponseOutputReasoning,
) -> String {
    // Compatible with opencode: it filters out blocks with empty thinking text,
    // so emit a default when the summary is absent/empty.
    let summary = match item.summary.as_ref() {
        Some(s) if !s.is_empty() => s,
        _ => return THINKING_TEXT.to_string(),
    };

    let mut segments = String::new();
    for block in summary {
        if let Some(text) = block.text.as_deref() {
            segments.push_str(text);
        }
    }
    segments.trim().to_string()
}

fn create_tool_use_content_block(
    call: &crate::services::copilot::create_responses::ResponseOutputFunctionCall,
) -> Option<Value> {
    let tool_id = &call.call_id;
    let tool_name = resolve_tool_use_name(call.name.as_str(), call.namespace.as_deref());
    if tool_name.is_empty() || tool_id.is_empty() {
        return None;
    }

    let input = parse_function_call_arguments(&call.arguments);
    Some(json!({
        "type": "tool_use",
        "id": tool_id,
        "name": tool_name,
        "input": input,
    }))
}

fn create_tool_search_use_content_block(
    call: &crate::services::copilot::create_responses::ResponseOutputToolSearchCall,
    tool_search_name: Option<&str>,
) -> Option<Value> {
    let tool_id = &call.call_id;
    if tool_id.is_empty() {
        return None;
    }
    let name = tool_search_name.unwrap_or(BRIDGE_TOOL_SEARCH_NAME);
    Some(json!({
        "type": "tool_use",
        "id": tool_id,
        "name": name,
        "input": parse_tool_search_arguments(&call.arguments),
    }))
}

/// Mirrors `resolveToolUseName`: prefer a non-empty namespace, else the name.
pub fn resolve_tool_use_name(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => ns.to_string(),
        _ => name.to_string(),
    }
}

fn create_compaction_thinking_block(
    item: &crate::services::copilot::create_responses::ResponseOutputCompaction,
) -> Option<Value> {
    if item.id.is_empty() || item.encrypted_content.is_empty() {
        return None;
    }

    Some(json!({
        "type": "thinking",
        "thinking": THINKING_TEXT,
        "signature": encode_compaction_carrier_signature(&item.encrypted_content, &item.id),
    }))
}

fn parse_function_call_arguments(raw_arguments: &str) -> Value {
    if raw_arguments.trim().is_empty() {
        return json!({});
    }

    match serde_json::from_str::<Value>(raw_arguments) {
        Ok(Value::Array(arr)) => json!({ "arguments": arr }),
        Ok(parsed) if parsed.is_object() => parsed,
        Ok(_) => json!({ "raw_arguments": raw_arguments }),
        Err(_) => {
            // Avoid logging the raw arguments — they may contain user data or
            // secrets. Log only the length as a diagnostic.
            tracing::warn!(
                "Failed to parse function call arguments ({} bytes)",
                raw_arguments.len()
            );
            json!({ "raw_arguments": raw_arguments })
        }
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

fn map_responses_stop_reason(response: &ResponsesResult, has_tool_call: bool) -> Option<String> {
    let status = response.status.as_str();

    if status == "completed" {
        // Consistent with the emitted content: `has_tool_call` reflects whether a
        // tool_use block was actually produced (see caller), so a FunctionCall /
        // ToolSearchCall item that was dropped for an empty id/name does not
        // falsely yield `tool_use`.
        return Some(if has_tool_call { "tool_use" } else { "end_turn" }.to_string());
    }

    if status == "incomplete" {
        let reason = response
            .incomplete_details
            .get("reason")
            .and_then(Value::as_str);
        match reason {
            Some("max_output_tokens") => return Some("max_tokens".to_string()),
            Some("content_filter") => return Some("end_turn".to_string()),
            _ => {}
        }
    }

    None
}

fn map_responses_usage(response: &ResponsesResult) -> AnthropicUsage {
    let usage = response.usage.as_ref();
    let input_tokens = usage.map(|u| u.input_tokens).unwrap_or(0);
    let output_tokens = usage.and_then(|u| u.output_tokens).unwrap_or(0);
    let cached_tokens = usage
        .and_then(|u| u.input_tokens_details.as_ref())
        .map(|d| d.cached_tokens);

    AnthropicUsage {
        input_tokens: (input_tokens - cached_tokens.unwrap_or(0)).max(0),
        output_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: cached_tokens,
        service_tier: None,
        extra: serde_json::Map::new(),
    }
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
                    Some("text") => result.push(create_text_content(text_field(block))),
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
                            .unwrap_or_default();
                        result.push(create_text_content(format!("Tool {tool_name} loaded")));
                    }
                    _ => {}
                }
            }
            FunctionCallOutputContent::Blocks(result)
        }
        _ => FunctionCallOutputContent::Text(String::new()),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_signature_splits_on_last_at() {
        let (enc, id) = parse_reasoning_signature("abc@def@id123");
        assert_eq!(enc, "abc@def");
        assert_eq!(id, "id123");
    }

    #[test]
    fn reasoning_signature_no_at_returns_whole() {
        let (enc, id) = parse_reasoning_signature("noatsign");
        assert_eq!(enc, "noatsign");
        assert_eq!(id, "");
    }

    #[test]
    fn reasoning_signature_trailing_at_is_invalid() {
        let (enc, id) = parse_reasoning_signature("abc@");
        assert_eq!(enc, "abc@");
        assert_eq!(id, "");
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

        let anthropic = translate_responses_result_to_anthropic(&result, None);
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
            output_text: "ignored fallback".to_string(),
            output: vec![
                ResponseOutputItem::Reasoning(ResponseOutputReasoning {
                    id: "rs_1".to_string(),
                    item_type: "reasoning".to_string(),
                    summary: Some(vec![ResponseReasoningBlock {
                        block_type: "summary_text".to_string(),
                        text: Some("pondering".to_string()),
                    }]),
                    encrypted_content: Some("ENC".to_string()),
                    status: None,
                }),
                ResponseOutputItem::Message(ResponseOutputMessage {
                    id: "msg_1".to_string(),
                    item_type: "message".to_string(),
                    role: "assistant".to_string(),
                    status: "completed".to_string(),
                    content: Some(vec![ResponseOutputContentBlock::Text(
                        crate::services::copilot::create_responses::ResponseOutputText {
                            block_type: "output_text".to_string(),
                            text: "the answer".to_string(),
                            annotations: vec![],
                        },
                    )]),
                }),
            ],
            ..Default::default()
        };

        let anthropic = translate_responses_result_to_anthropic(&result, None);
        assert_eq!(anthropic.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(anthropic.content.len(), 2);
        assert_eq!(
            anthropic.content[0].get("type").and_then(Value::as_str),
            Some("thinking")
        );
        assert_eq!(
            anthropic.content[0]
                .get("signature")
                .and_then(Value::as_str),
            Some("ENC@rs_1")
        );
        assert_eq!(
            anthropic.content[1].get("text").and_then(Value::as_str),
            Some("the answer")
        );
    }

    #[test]
    fn dropped_tool_call_keeps_stop_reason_consistent() {
        use crate::services::copilot::create_responses::ResponseOutputFunctionCall;

        // A function_call whose call_id/name decoded to "" (upstream sent null,
        // now coerced by null_to_default) is dropped by the tool_use block
        // builder. stop_reason must then be `end_turn`, NOT `tool_use`, so the
        // Anthropic response stays internally consistent (no tool_use stop with
        // zero tool_use blocks). Regression for PR #83 Copilot review.
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
                },
            )],
            ..Default::default()
        };

        let anthropic = translate_responses_result_to_anthropic(&result, None);
        assert!(
            !anthropic
                .content
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")),
            "empty-id tool call must not emit a tool_use block"
        );
        assert_eq!(anthropic.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn valid_tool_call_still_reports_tool_use() {
        use crate::services::copilot::create_responses::ResponseOutputFunctionCall;

        let result = ResponsesResult {
            id: "resp_y".to_string(),
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
                },
            )],
            ..Default::default()
        };

        let anthropic = translate_responses_result_to_anthropic(&result, None);
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
                id: "rs".to_string(),
                item_type: "reasoning".to_string(),
                summary: None,
                encrypted_content: Some("E".to_string()),
                status: None,
            })],
            ..Default::default()
        };

        let anthropic = translate_responses_result_to_anthropic(&result, None);
        assert_eq!(anthropic.content.len(), 1);
        assert_eq!(
            anthropic.content[0].get("thinking").and_then(Value::as_str),
            Some(THINKING_TEXT)
        );
        assert_eq!(
            anthropic.content[0]
                .get("signature")
                .and_then(Value::as_str),
            Some("E@rs")
        );
    }
}
