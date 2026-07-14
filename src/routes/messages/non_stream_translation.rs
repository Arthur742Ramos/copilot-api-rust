//! Non-streaming translation between the Anthropic Messages API and the
//! OpenAI Chat Completions API.
//!
//! Mirrors `src/routes/messages/non-stream-translation.ts`. These are PURE
//! transformation functions: no network, no async. The only global read is
//! `state.models` inside `get_thinking_budget` (matching the TS lookup).
//!
//! Because the Anthropic/OpenAI content is `string | array` polymorphic, the
//! dynamic object construction the TS does with object literals is mirrored with
//! `serde_json::json!` / `Value` building, while fixed shapes use the typed
//! structs from `anthropic_types` / `create_chat_completions`.

use serde_json::{json, Map, Value};

use crate::libs::error::{AppError, HttpError};
use crate::libs::state::with_state;
use crate::routes::messages::anthropic_types::{
    AnthropicInputMessage, AnthropicMessagesPayload, AnthropicResponse, AnthropicThinkingConfig,
    AnthropicTool, AnthropicToolChoice, AnthropicUsage,
};
use crate::routes::messages::request_validation::{
    collect_open_object_extensions, merge_open_object_extensions,
};
use crate::routes::messages::stream_translation::{
    nonnegative_i64, safe_upstream_error_message, safe_upstream_error_type,
};
use crate::routes::messages::utils::map_openai_stop_reason_to_anthropic;
use crate::services::copilot::create_chat_completions::{ChatCompletionsPayload, Message};
use crate::services::copilot::get_models::Model;

// ---------------------------------------------------------------------------
// Module constants (copied locally, NOT depended on from a sibling Phase-2
// file, so this module compiles standalone given the Phase-1 modules).
// ---------------------------------------------------------------------------

/// Compatible with opencode, it filters out blocks where the thinking text is
/// empty, so we add a default thinking text.
pub use super::utils::THINKING_TEXT;

const CHAT_REASONING_SIGNATURE_PREFIX: &str = "cr1#";

pub fn encode_chat_reasoning_signature(signature: &str) -> String {
    format!(
        "{CHAT_REASONING_SIGNATURE_PREFIX}{}",
        json!({"signature": signature})
    )
}

fn decode_chat_reasoning_signature(signature: &str) -> Option<String> {
    let raw = signature.strip_prefix(CHAT_REASONING_SIGNATURE_PREFIX)?;
    serde_json::from_str::<Value>(raw)
        .ok()?
        .get("signature")?
        .as_str()
        .map(str::to_string)
}

pub(crate) fn is_chat_reasoning_carrier_signature(signature: &str) -> bool {
    decode_chat_reasoning_signature(signature).is_some()
}

/// Inserted into the tool message when rich content had to be relocated to a
/// user message because the upstream does not support it in tool messages.
pub const RICH_TOOL_RESULT_MOVED_TEXT: &str =
    "Rich tool result content was moved to a user message because this upstream does not support it in tool messages.";

/// `COPILOT_TOOL_CONTENT_SUPPORT_TYPE = ["array", "image"]`.
const COPILOT_TOOL_CONTENT_SUPPORT_TYPE: [&str; 2] = ["array", "image"];
const CHAT_REQUEST_CANONICAL_FIELDS: &[&str] = &[
    "messages",
    "model",
    "max_tokens",
    "stream",
    "stop",
    "temperature",
    "top_p",
    "top_k",
    "user",
    "tools",
    "tool_choice",
    "thinking_budget",
    "reasoning_effort",
    "service_tier",
    "parallel_tool_calls",
    "stream_options",
    "cache_control",
];
const CHAT_MESSAGE_CANONICAL_FIELDS: &[&str] = &[
    "role",
    "content",
    "name",
    "tool_call_id",
    "tool_calls",
    "reasoning_text",
    "reasoning_content",
    "reasoning_opaque",
    "copilot_cache_control",
];
const ANTHROPIC_RESPONSE_CANONICAL_FIELDS: &[&str] = &[
    "id",
    "type",
    "role",
    "content",
    "model",
    "stop_reason",
    "stop_sequence",
    "usage",
    "chat_choice_extensions",
    "chat_message_extensions",
];
const ANTHROPIC_USAGE_CANONICAL_FIELDS: &[&str] = &[
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "service_tier",
    "chat_prompt_tokens_details",
];
const ANTHROPIC_TOOL_USE_CANONICAL_FIELDS: &[&str] =
    &["type", "id", "name", "input", "chat_function_extensions"];

// ---------------------------------------------------------------------------
// Capability flags (mirror TranslationCapabilities).
// ---------------------------------------------------------------------------

/// Mirrors `TranslationCapabilities`.
#[derive(Debug, Clone)]
struct TranslationCapabilities {
    support_pdf: bool,
    tool_content_support_type: Vec<String>,
}

impl Default for TranslationCapabilities {
    fn default() -> Self {
        // The api-flows caller passes NO options, so defaults apply:
        // support_pdf = false, tool_content_support_type = ["array", "image"].
        Self {
            support_pdf: false,
            tool_content_support_type: COPILOT_TOOL_CONTENT_SUPPORT_TYPE
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Mirrors `TranslateToOpenAIOptions`. Threaded from `modelConfig` at the
/// provider call sites; the `Default` matches the hardcoded capabilities used
/// by the main `/v1/messages` spine (support_pdf = false,
/// tool_content_support_type = ["array", "image"]).
#[derive(Debug, Clone)]
pub struct TranslateToOpenAiOptions {
    pub support_pdf: bool,
    pub tool_content_support_type: Vec<String>,
}

impl Default for TranslateToOpenAiOptions {
    fn default() -> Self {
        Self {
            support_pdf: false,
            tool_content_support_type: COPILOT_TOOL_CONTENT_SUPPORT_TYPE
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Mirrors `ToolContentSupport`.
struct ToolContentSupport {
    array: bool,
    image: bool,
    pdf: bool,
}

/// Mirrors `ToolResultMessages`.
struct ToolResultMessages {
    moved_user_message: Option<Message>,
    tool_message: Message,
}

// ---------------------------------------------------------------------------
// DIRECTION A: Anthropic request -> OpenAI ChatCompletions payload
// ---------------------------------------------------------------------------

/// Mirrors `translateToOpenAI`. The api-flows caller passes no options, so the
/// default capabilities apply. Thin wrapper over
/// [`translate_to_openai_with_options`].
#[allow(clippy::result_large_err)]
pub fn translate_to_openai(
    payload: &AnthropicMessagesPayload,
) -> Result<ChatCompletionsPayload, AppError> {
    translate_to_openai_with_options(payload, &TranslateToOpenAiOptions::default())
}

/// Mirrors `translateToOpenAI(payload, options)`: honours `supportPdf` /
/// `toolContentSupportType` from the provider model config.
#[allow(clippy::result_large_err)]
pub fn translate_to_openai_with_options(
    payload: &AnthropicMessagesPayload,
    options: &TranslateToOpenAiOptions,
) -> Result<ChatCompletionsPayload, AppError> {
    validate_chat_config_extensions(payload)?;
    let model_id = payload.model.clone();
    let thinking_budget = get_thinking_budget(payload);
    let capabilities = TranslationCapabilities {
        support_pdf: options.support_pdf,
        tool_content_support_type: options.tool_content_support_type.clone(),
    };

    let messages = translate_anthropic_messages_to_openai(payload, &model_id, &capabilities)?;

    // The Rust ChatCompletionsPayload keeps model/messages/max_tokens/stream
    // strongly typed; the remaining fields the TS sets go through `extra`,
    // inserted in the TS object-literal order.
    let mut extra: Map<String, Value> = Map::new();
    if let Some(stop) = &payload.stop_sequences {
        extra.insert("stop".to_string(), json!(stop));
    }
    if let Some(temperature) = payload.temperature {
        extra.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = payload.top_p {
        extra.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(top_k) = payload.top_k {
        extra.insert("top_k".to_string(), json!(top_k));
    }
    if let Some(user) = payload.metadata.as_ref().and_then(|m| m.user_id.clone()) {
        extra.insert("user".to_string(), json!(user));
    }
    if let Some(tools) = translate_anthropic_tools_to_openai(payload.tools.as_ref())? {
        extra.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) =
        translate_anthropic_tool_choice_to_openai(payload.tool_choice.as_ref())?
    {
        extra.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(budget) = thinking_budget {
        extra.insert("thinking_budget".to_string(), json!(budget));
    }
    let thinking_disabled = payload
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.kind == "disabled");
    if !thinking_disabled {
        if let Some(effort) = payload
            .output_config
            .as_ref()
            .and_then(|config| config.effort.as_ref())
        {
            extra.insert("reasoning_effort".to_string(), json!(effort));
        }
    }
    if let Some(service_tier) = &payload.service_tier {
        extra.insert("service_tier".to_string(), json!(service_tier));
    }
    let request_extensions = collect_open_object_extensions(
        &payload.extra,
        &[],
        CHAT_REQUEST_CANONICAL_FIELDS,
        "request",
    )?;
    extra.extend(request_extensions);

    Ok(ChatCompletionsPayload {
        messages,
        model: model_id,
        max_tokens: payload.max_tokens,
        stream: payload.stream,
        extra,
    })
}

#[allow(clippy::result_large_err)]
fn validate_chat_config_extensions(payload: &AnthropicMessagesPayload) -> Result<(), AppError> {
    if payload.cache_control.is_some() {
        return Err(AppError::BadRequest(
            "cache_control cannot be represented by the Chat Completions request object"
                .to_string(),
        ));
    }
    if let Some(metadata) = &payload.metadata {
        reject_unrepresentable_extensions(
            &metadata.extra,
            "metadata",
            "the scalar Chat user field",
        )?;
    }
    if let Some(thinking) = &payload.thinking {
        if thinking.display.as_ref().is_some() {
            return Err(AppError::BadRequest(
                "thinking.display cannot be represented by Chat Completions".to_string(),
            ));
        }
        reject_unrepresentable_extensions(
            &thinking.extra,
            "thinking",
            "the scalar Chat thinking_budget field",
        )?;
    }
    if let Some(output_config) = &payload.output_config {
        reject_unrepresentable_extensions(
            &output_config.extra,
            "output_config",
            "the scalar Chat reasoning_effort field",
        )?;
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn reject_unrepresentable_extensions(
    source: &Map<String, Value>,
    path: &str,
    target: &str,
) -> Result<(), AppError> {
    if let Some((key, _)) = source.iter().next() {
        return Err(AppError::BadRequest(format!(
            "{path}.{key} cannot be represented by {target}"
        )));
    }
    Ok(())
}

/// Mirrors `getThinkingBudget`, reading `state.models` to find the model by id.
fn get_thinking_budget(payload: &AnthropicMessagesPayload) -> Option<i64> {
    with_state(|s| {
        let model = s
            .models
            .as_ref()
            .and_then(|m| m.data.iter().find(|md| md.id == payload.model));
        thinking_budget_from_model(payload.thinking.as_ref(), model)
    })
}

/// The pure arithmetic core of `getThinkingBudget`, split out for testing.
fn thinking_budget_from_model(
    thinking: Option<&AnthropicThinkingConfig>,
    model: Option<&Model>,
) -> Option<i64> {
    let (thinking, model) = match (thinking, model) {
        (Some(t), Some(m)) => (t, m),
        _ => return None,
    };

    if thinking.kind == "disabled" {
        return None;
    }

    let supports = &model.capabilities.supports;
    let limits = &model.capabilities.limits;

    let max_thinking_budget = std::cmp::min(
        supports.max_thinking_budget.unwrap_or(0),
        limits.max_output_tokens.unwrap_or(0) - 1,
    );

    // thinking.budget_tokens ??= maxThinkingBudget
    let budget_tokens = thinking
        .budget_tokens
        .as_ref()
        .copied()
        .unwrap_or(max_thinking_budget);

    if max_thinking_budget > 0 {
        let bt = std::cmp::min(budget_tokens, max_thinking_budget);
        Some(std::cmp::max(
            bt,
            supports.min_thinking_budget.unwrap_or(1024),
        ))
    } else {
        None
    }
}

/// Mirrors `translateAnthropicMessagesToOpenAI`: system messages first, then the
/// user/assistant messages flat-mapped.
#[allow(clippy::result_large_err)]
fn translate_anthropic_messages_to_openai(
    payload: &AnthropicMessagesPayload,
    model_id: &str,
    capabilities: &TranslationCapabilities,
) -> Result<Vec<Message>, AppError> {
    let mut out = handle_system_prompt(payload.system.as_ref())?;
    for (message_index, message) in payload.messages.iter().enumerate() {
        let path = format!("messages[{message_index}]");
        if message.role == "user" {
            out.extend(handle_user_message(message, capabilities, &path)?);
        } else {
            out.extend(handle_assistant_message(
                message,
                model_id,
                capabilities,
                &path,
            )?);
        }
    }
    Ok(out)
}

/// Mirrors `handleSystemPrompt`.
#[allow(clippy::result_large_err)]
fn handle_system_prompt(system: Option<&Value>) -> Result<Vec<Message>, AppError> {
    let system = match system {
        Some(v) if !v.is_null() => v,
        _ => return Ok(Vec::new()),
    };

    match system {
        Value::String(s) => {
            // Empty string is falsy in the TS `if (!system)` check.
            if s.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![text_message("system", s.clone())])
        }
        Value::Array(blocks) => {
            let mut preserve_parts = false;
            for (index, block) in blocks.iter().enumerate() {
                let path = format!("system[{index}]");
                let object = block
                    .as_object()
                    .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
                let extensions = collect_open_object_extensions(
                    object,
                    &["type", "text", "cache_control"],
                    &["type", "text"],
                    &path,
                )?;
                preserve_parts |= !extensions.is_empty()
                    || object
                        .get("cache_control")
                        .is_some_and(|value| !value.is_null());
            }
            if preserve_parts {
                let parts = blocks
                    .iter()
                    .enumerate()
                    .map(|(index, block)| create_chat_text_part(block, &format!("system[{index}]")))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(vec![Message {
                    role: "system".to_string(),
                    content: Some(Value::Array(parts)),
                    extra: Map::new(),
                }]);
            }
            let system_text = blocks
                .iter()
                .map(|block| {
                    block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            Ok(vec![text_message("system", system_text)])
        }
        _ => Err(AppError::BadRequest(
            "system must be a string, array, or null".to_string(),
        )),
    }
}

#[allow(clippy::result_large_err)]
fn create_chat_text_part(block: &Value, path: &str) -> Result<Value, AppError> {
    let source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    let text = source
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("{path}.text must be a string")))?;
    let extensions = collect_open_object_extensions(
        source,
        &["type", "text", "cache_control"],
        &["type", "text"],
        path,
    )?;
    let mut target = Map::from_iter([
        ("type".to_string(), json!("text")),
        ("text".to_string(), json!(text)),
    ]);
    if let Some(cache_control) = source.get("cache_control").filter(|value| !value.is_null()) {
        target.insert("cache_control".to_string(), cache_control.clone());
    }
    target.extend(extensions);
    Ok(Value::Object(target))
}

/// Mirrors `handleUserMessage`. Tool results MUST come first to maintain the
/// protocol ordering: tool_use -> tool_result -> user.
#[allow(clippy::result_large_err)]
fn handle_user_message(
    message: &AnthropicInputMessage,
    capabilities: &TranslationCapabilities,
    path: &str,
) -> Result<Vec<Message>, AppError> {
    let message_extensions =
        collect_open_object_extensions(&message.extra, &[], CHAT_MESSAGE_CANONICAL_FIELDS, path)?;
    let mut new_messages: Vec<Message> = Vec::new();

    if let Value::Array(blocks) = &message.content {
        let tool_result_blocks: Vec<(usize, &Value)> = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block_type(block) == Some("tool_result"))
            .collect();
        let other_blocks: Vec<(usize, &Value)> = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block_type(block) != Some("tool_result"))
            .collect();

        let mut moved_tool_result_user_messages: Vec<Message> = Vec::new();
        for (block_index, block) in tool_result_blocks {
            let result = handle_tool_result_block(
                block,
                capabilities,
                &format!("{path}.content[{block_index}]"),
            )?;
            new_messages.push(result.tool_message);
            if let Some(moved) = result.moved_user_message {
                moved_tool_result_user_messages.push(moved);
            }
        }
        if !other_blocks.is_empty() {
            new_messages.extend(moved_tool_result_user_messages);
            let mapped = map_content_blocks(
                other_blocks.into_iter(),
                capabilities.support_pdf,
                &format!("{path}.content"),
            )?;
            new_messages.push(Message {
                role: "user".to_string(),
                content: mapped,
                extra: message_extensions,
            });
        } else if !message_extensions.is_empty() {
            if moved_tool_result_user_messages.len() != 1 {
                return Err(AppError::BadRequest(format!(
                    "{path}: message extensions cannot be represented unambiguously when a user message contains only tool results"
                )));
            }
            let mut moved = moved_tool_result_user_messages
                .pop()
                .expect("one moved user message");
            moved.extra.extend(message_extensions);
            new_messages.push(moved);
        } else {
            new_messages.extend(moved_tool_result_user_messages);
        }
    } else {
        new_messages.push(Message {
            role: "user".to_string(),
            content: map_content(&message.content, false, &format!("{path}.content"))?,
            extra: message_extensions,
        });
    }

    Ok(new_messages)
}

/// Mirrors `handleToolResultBlock`.
#[allow(clippy::result_large_err)]
fn handle_tool_result_block(
    block: &Value,
    capabilities: &TranslationCapabilities,
    path: &str,
) -> Result<ToolResultMessages, AppError> {
    let content = block.get("content");

    // String content -> straight tool message.
    if let Some(Value::String(s)) = content {
        return Ok(ToolResultMessages {
            moved_user_message: None,
            tool_message: create_tool_message(block, Some(Value::String(s.clone())), path)?,
        });
    }

    // Non-array (and non-string) content -> empty tool message.
    let blocks = match content {
        Some(Value::Array(a)) => a,
        _ => {
            return Ok(ToolResultMessages {
                moved_user_message: None,
                tool_message: create_tool_message(block, Some(Value::String(String::new())), path)?,
            });
        }
    };

    let support = get_tool_content_support(capabilities);
    let has_image = blocks.iter().any(|b| block_type(b) == Some("image"));
    let has_document = blocks.iter().any(|b| block_type(b) == Some("document"));
    let content_value = map_content(
        &Value::Array(blocks.clone()),
        capabilities.support_pdf,
        &format!("{path}.content"),
    )?;

    let has_pdf_file = has_document && capabilities.support_pdf;
    let should_move_image_to_user_message = has_image && !support.image;
    let should_move_pdf_to_user_message = has_pdf_file && !support.pdf;

    if should_move_image_to_user_message || should_move_pdf_to_user_message {
        let text = get_text_tool_content(&content_value);
        let tool_text = if text.is_empty() {
            RICH_TOOL_RESULT_MOVED_TEXT.to_string()
        } else {
            text
        };
        return Ok(ToolResultMessages {
            moved_user_message: Some(create_tool_result_user_message(
                block,
                capabilities.support_pdf,
                path,
            )?),
            tool_message: create_tool_message(block, Some(Value::String(tool_text)), path)?,
        });
    }

    let has_rich_content = has_image || has_pdf_file;
    if support.array || has_rich_content {
        return Ok(ToolResultMessages {
            moved_user_message: None,
            tool_message: create_tool_message(block, content_value, path)?,
        });
    }
    if chat_text_content_has_extensions(&content_value) {
        return Err(AppError::BadRequest(format!(
            "{path}.content: extensions cannot be represented when this Chat provider requires scalar tool content"
        )));
    }

    Ok(ToolResultMessages {
        moved_user_message: None,
        tool_message: create_tool_message(
            block,
            Some(Value::String(get_text_tool_content(&content_value))),
            path,
        )?,
    })
}

fn chat_text_content_has_extensions(content: &Option<Value>) -> bool {
    let Some(Value::Array(parts)) = content else {
        return false;
    };
    parts.iter().any(|part| {
        part.as_object().is_some_and(|part| {
            part.keys()
                .any(|key| !matches!(key.as_str(), "type" | "text"))
        })
    })
}

/// Mirrors `getTextToolContent`.
fn get_text_tool_content(content: &Option<Value>) -> String {
    match content {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                let is_text = part.get("type").and_then(|t| t.as_str()) == Some("text");
                let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if is_text && !text.is_empty() {
                    Some(text.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => s.clone(),
        // `content ?? ""` for non-array, non-string (null / undefined / other).
        _ => String::new(),
    }
}

/// Mirrors `getToolContentSupport`.
fn get_tool_content_support(capabilities: &TranslationCapabilities) -> ToolContentSupport {
    let includes = |v: &str| {
        capabilities
            .tool_content_support_type
            .iter()
            .any(|t| t == v)
    };
    ToolContentSupport {
        array: includes("array"),
        image: includes("image"),
        pdf: capabilities.support_pdf && includes("pdf"),
    }
}

/// Mirrors `createToolMessage`.
#[allow(clippy::result_large_err)]
fn create_tool_message(
    block: &Value,
    content: Option<Value>,
    path: &str,
) -> Result<Message, AppError> {
    let source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    let tool_call_id = source
        .get("tool_use_id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("{path}.tool_use_id must be a string")))?;
    let mut extra = Map::new();
    extra.insert("tool_call_id".to_string(), json!(tool_call_id));
    if let Some(is_error) = source.get("is_error").filter(|value| !value.is_null()) {
        extra.insert("is_error".to_string(), is_error.clone());
    }
    if let Some(cache_control) = source.get("cache_control").filter(|value| !value.is_null()) {
        extra.insert("cache_control".to_string(), cache_control.clone());
    }
    let extensions = collect_open_object_extensions(
        source,
        &[
            "type",
            "tool_use_id",
            "content",
            "is_error",
            "cache_control",
        ],
        CHAT_MESSAGE_CANONICAL_FIELDS,
        path,
    )?;
    extra.extend(extensions);
    Ok(Message {
        role: "tool".to_string(),
        content,
        extra,
    })
}

/// Mirrors `createToolResultUserMessage`.
#[allow(clippy::result_large_err)]
fn create_tool_result_user_message(
    block: &Value,
    support_pdf: bool,
    path: &str,
) -> Result<Message, AppError> {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let prefix = json!({
        "type": "text",
        "text": format!("Tool result for {tool_use_id}:"),
    });
    let content = map_content(
        block.get("content").unwrap_or(&Value::Null),
        support_pdf,
        &format!("{path}.content"),
    )?;

    let parts = match content {
        Some(Value::Array(mut arr)) => {
            let mut out = vec![prefix];
            out.append(&mut arr);
            out
        }
        Some(Value::String(s)) => {
            vec![prefix, json!({ "type": "text", "text": s })]
        }
        // content ?? "" for null / undefined.
        _ => vec![prefix, json!({ "type": "text", "text": "" })],
    };

    Ok(Message {
        role: "user".to_string(),
        content: Some(Value::Array(parts)),
        extra: Map::new(),
    })
}

/// Mirrors `handleAssistantMessage`.
#[allow(clippy::result_large_err)]
fn handle_assistant_message(
    message: &AnthropicInputMessage,
    model_id: &str,
    capabilities: &TranslationCapabilities,
    path: &str,
) -> Result<Vec<Message>, AppError> {
    let message_extensions =
        collect_open_object_extensions(&message.extra, &[], CHAT_MESSAGE_CANONICAL_FIELDS, path)?;
    let blocks = match &message.content {
        Value::Array(a) => a,
        _ => {
            return Ok(vec![Message {
                role: "assistant".to_string(),
                content: map_content(&message.content, false, &format!("{path}.content"))?,
                extra: message_extensions,
            }]);
        }
    };

    let tool_use_blocks: Vec<(usize, &Value)> = blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block_type(block) == Some("tool_use"))
        .collect();

    let mut thinking_blocks: Vec<(usize, &Value)> = blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block_type(block) == Some("thinking"))
        .collect();
    for (block_index, block) in &thinking_blocks {
        validate_chat_thinking_block_extensions(block, &format!("{path}.content[{block_index}]"))?;
    }

    if model_id.starts_with("claude") {
        thinking_blocks.retain(|(_, block)| {
            let b = *block;
            let thinking = b.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
            let signature = b.get("signature").and_then(|s| s.as_str()).unwrap_or("");
            !thinking.is_empty()
                && thinking != THINKING_TEXT
                && !signature.is_empty()
                // gpt signature has @ in it, so filter those out for claude models
                && !signature.contains('@')
        });
    }

    let thinking_contents: Vec<String> = thinking_blocks
        .iter()
        .filter_map(|(_, block)| {
            let b = *block;
            let thinking = b.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
            if !thinking.is_empty() && thinking != THINKING_TEXT {
                Some(thinking.to_string())
            } else {
                None
            }
        })
        .collect();

    let all_thinking_content = if !thinking_contents.is_empty() {
        Some(thinking_contents.join("\n\n"))
    } else {
        None
    };

    let signature = thinking_blocks.iter().find_map(|(_, block)| {
        let b = *block;
        let s = b.get("signature").and_then(|s| s.as_str()).unwrap_or("");
        if !s.is_empty() {
            Some(decode_chat_reasoning_signature(s).unwrap_or_else(|| s.to_string()))
        } else {
            None
        }
    });

    let mut extra = Map::new();
    if let Some(rt) = all_thinking_content {
        extra.insert("reasoning_text".to_string(), json!(rt));
    }
    if let Some(sig) = signature {
        extra.insert("reasoning_opaque".to_string(), json!(sig));
    }
    if !tool_use_blocks.is_empty() {
        let tool_calls: Vec<Value> = tool_use_blocks
            .iter()
            .map(|(block_index, tool_use)| {
                create_chat_tool_call(tool_use, &format!("{path}.content[{block_index}]"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        extra.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    merge_open_object_extensions(&message.extra, &[], &mut extra, path)?;

    Ok(vec![Message {
        role: "assistant".to_string(),
        content: map_content(
            &message.content,
            capabilities.support_pdf,
            &format!("{path}.content"),
        )?,
        extra,
    }])
}

#[allow(clippy::result_large_err)]
fn validate_chat_thinking_block_extensions(block: &Value, path: &str) -> Result<(), AppError> {
    let source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    if source
        .get("cache_control")
        .is_some_and(|value| !value.is_null())
    {
        return Err(AppError::BadRequest(format!(
            "{path}.cache_control cannot be represented by a Chat assistant message"
        )));
    }
    let extensions = collect_open_object_extensions(
        source,
        &["type", "thinking", "signature", "cache_control"],
        &["reasoning_text", "reasoning_opaque"],
        path,
    )?;
    reject_unrepresentable_extensions(&extensions, path, "the scalar Chat reasoning fields")
}

#[allow(clippy::result_large_err)]
fn create_chat_tool_call(block: &Value, path: &str) -> Result<Value, AppError> {
    let source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    let id = source
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("{path}.id must be a string")))?;
    let name = source
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("{path}.name must be a string")))?;
    let input = source
        .get("input")
        .ok_or_else(|| AppError::BadRequest(format!("{path}.input is required")))?;
    let arguments = serde_json::to_string(input)
        .map_err(|error| AppError::Other(anyhow::anyhow!("{error}")))?;
    let function = Map::from_iter([
        ("name".to_string(), json!(name)),
        ("arguments".to_string(), json!(arguments)),
    ]);
    let mut target = Map::from_iter([
        ("id".to_string(), json!(id)),
        ("type".to_string(), json!("function")),
        ("function".to_string(), Value::Object(function)),
    ]);
    if let Some(cache_control) = source.get("cache_control").filter(|value| !value.is_null()) {
        target.insert("cache_control".to_string(), cache_control.clone());
    }
    merge_open_object_extensions(
        source,
        &["type", "id", "name", "input", "cache_control"],
        &mut target,
        path,
    )?;
    Ok(Value::Object(target))
}

/// Mirrors `mapContent`. Returns `Some(string)` / `Some(array)` / `None`,
/// matching the TS `string | Array<ContentPart> | null`.
#[allow(clippy::result_large_err)]
fn map_content(content: &Value, support_pdf: bool, path: &str) -> Result<Option<Value>, AppError> {
    if let Value::String(s) = content {
        return Ok(Some(Value::String(s.clone())));
    }
    let blocks = match content {
        Value::Array(a) => a,
        _ => return Ok(None),
    };
    map_content_blocks(blocks.iter().enumerate(), support_pdf, path)
}

#[allow(clippy::result_large_err)]
fn map_content_blocks<'a>(
    blocks: impl IntoIterator<Item = (usize, &'a Value)>,
    support_pdf: bool,
    path: &str,
) -> Result<Option<Value>, AppError> {
    let mut content_parts: Vec<Value> = Vec::new();
    for (block_index, block) in blocks {
        let block_path = format!("{path}[{block_index}]");
        match block_type(block) {
            Some("text") => {
                content_parts.push(create_chat_text_part(block, &block_path)?);
            }
            Some("image") => {
                content_parts.push(create_chat_image_part(block, &block_path)?);
            }
            Some("document") => {
                content_parts.push(create_chat_document_part(block, support_pdf, &block_path)?);
            }
            Some("tool_reference") => {
                validate_unrepresentable_tool_reference_extensions(block, &block_path)?;
                let tool_name = block
                    .get("tool_name")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                content_parts.push(json!({
                    "type": "text",
                    "text": format!("Tool {tool_name} loaded"),
                }));
            }
            // These assistant-only blocks are represented by message-level
            // reasoning/tool-call fields in `handle_assistant_message`.
            Some("thinking" | "tool_use") => {}
            Some(other) => {
                return Err(AppError::BadRequest(format!(
                    "{block_path}.type \"{other}\" is not supported by Chat Completions"
                )));
            }
            None => {
                return Err(AppError::BadRequest(format!(
                    "{block_path}.type must be a non-empty string"
                )));
            }
        }
    }

    if content_parts.is_empty() {
        return Ok(Some(Value::String(String::new())));
    }
    Ok(Some(Value::Array(content_parts)))
}

#[allow(clippy::result_large_err)]
fn create_chat_image_part(block: &Value, path: &str) -> Result<Value, AppError> {
    let block_source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    let source_path = format!("{path}.source");
    let source = block_source
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::BadRequest(format!("{source_path} must be an object")))?;
    validate_source_field_usage(source, &source_path)?;
    let source_extensions = collect_open_object_extensions(
        source,
        &["type", "media_type", "data", "url", "file_id"],
        &["url"],
        &source_path,
    )?;
    let mut image_url = Map::from_iter([(
        "url".to_string(),
        json!(image_url_from_source(block_source.get("source"))?),
    )]);
    image_url.extend(source_extensions);

    let block_extensions = collect_open_object_extensions(
        block_source,
        &["type", "source", "cache_control"],
        &["type", "image_url"],
        path,
    )?;
    let mut target = Map::from_iter([
        ("type".to_string(), json!("image_url")),
        ("image_url".to_string(), Value::Object(image_url)),
    ]);
    if let Some(cache_control) = block_source
        .get("cache_control")
        .filter(|value| !value.is_null())
    {
        target.insert("cache_control".to_string(), cache_control.clone());
    }
    target.extend(block_extensions);
    Ok(Value::Object(target))
}

#[allow(clippy::result_large_err)]
fn create_chat_document_part(
    block: &Value,
    support_pdf: bool,
    path: &str,
) -> Result<Value, AppError> {
    let block_source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    let source_path = format!("{path}.source");
    let source = block_source
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::BadRequest(format!("{source_path} must be an object")))?;
    validate_source_field_usage(source, &source_path)?;
    let source_extensions = collect_open_object_extensions(
        source,
        &["type", "media_type", "data", "url", "file_id"],
        &["file_data", "filename"],
        &source_path,
    )?;
    let block_extensions = collect_open_object_extensions(
        block_source,
        &["type", "source", "title", "cache_control"],
        &["type", "file"],
        path,
    )?;
    let has_cache_control = block_source
        .get("cache_control")
        .is_some_and(|value| !value.is_null());

    if !support_pdf {
        if has_cache_control || !source_extensions.is_empty() || !block_extensions.is_empty() {
            return Err(AppError::BadRequest(format!(
                "{path}: document extensions cannot be represented by the Chat text fallback"
            )));
        }
        return Ok(create_document_text_part());
    }

    let filename = block_source
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("document.pdf");
    let mut file = Map::from_iter([
        (
            "file_data".to_string(),
            json!(image_url_from_source(block_source.get("source"))?),
        ),
        ("filename".to_string(), json!(filename)),
    ]);
    file.extend(source_extensions);
    let mut target = Map::from_iter([
        ("type".to_string(), json!("file")),
        ("file".to_string(), Value::Object(file)),
    ]);
    if let Some(cache_control) = block_source
        .get("cache_control")
        .filter(|value| !value.is_null())
    {
        target.insert("cache_control".to_string(), cache_control.clone());
    }
    target.extend(block_extensions);
    Ok(Value::Object(target))
}

#[allow(clippy::result_large_err)]
fn validate_source_field_usage(source: &Map<String, Value>, path: &str) -> Result<(), AppError> {
    let source_type = source
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("base64");
    let unused_fields: &[&str] = match source_type {
        "base64" => &["url", "file_id"],
        "url" => &["media_type", "data", "file_id"],
        _ => &[],
    };
    if let Some(field) = unused_fields
        .iter()
        .find(|field| source.get(**field).is_some_and(|value| !value.is_null()))
    {
        return Err(AppError::BadRequest(format!(
            "{path}.{field} is not valid for a {source_type} source"
        )));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn validate_unrepresentable_tool_reference_extensions(
    block: &Value,
    path: &str,
) -> Result<(), AppError> {
    let source = block
        .as_object()
        .ok_or_else(|| AppError::BadRequest(format!("{path} must be an object")))?;
    let extensions = collect_open_object_extensions(
        source,
        &["type", "tool_name", "cache_control"],
        &["type", "text"],
        path,
    )?;
    if source
        .get("cache_control")
        .is_some_and(|value| !value.is_null())
        || !extensions.is_empty()
    {
        return Err(AppError::BadRequest(format!(
            "{path}: tool_reference extensions cannot be represented after Chat text conversion"
        )));
    }
    Ok(())
}

/// Resolves an Anthropic image `source` object into the `image_url.url` string
/// expected by Chat Completions.
///
/// Anthropic image sources are tagged by `source.type`:
/// - `base64` builds a `data:` URL from `media_type` + `data`.
/// - `url` passes the remote URL straight through.
/// - `file` references a Files-API id, which this Chat Completions upstream
///   cannot resolve; surface a 400 instead of emitting a blank `data:` URL.
#[allow(clippy::result_large_err)]
fn image_url_from_source(source: Option<&Value>) -> Result<String, AppError> {
    let source_type = source
        .and_then(|s| s.get("type"))
        .and_then(|t| t.as_str())
        // Anthropic historically omitted `type` for base64 sources; default to it.
        .unwrap_or("base64");

    match source_type {
        "url" => {
            let url = source
                .and_then(|s| s.get("url"))
                .and_then(|u| u.as_str())
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .ok_or_else(|| {
                    AppError::BadRequest(
                        "Image source of type \"url\" is missing \"url\"".to_string(),
                    )
                })?;
            Ok(url.to_string())
        }
        "file" => Err(AppError::BadRequest(
            "Image source of type \"file\" (Files API ids) is not supported".to_string(),
        )),
        "base64" => {
            let media_type = source
                .and_then(|s| s.get("media_type"))
                .and_then(|m| m.as_str())
                .filter(|m| !m.is_empty());
            let data = source
                .and_then(|s| s.get("data"))
                .and_then(|d| d.as_str())
                .filter(|d| !d.is_empty());
            match (media_type, data) {
                (Some(media_type), Some(data)) => Ok(format!("data:{media_type};base64,{data}")),
                _ => Err(AppError::BadRequest(
                    "Base64 image source is missing \"media_type\" or \"data\"".to_string(),
                )),
            }
        }
        other => Err(AppError::BadRequest(format!(
            "Unsupported image source type \"{other}\""
        ))),
    }
}

/// Mirrors `createDocumentTextPart`.
fn create_document_text_part() -> Value {
    json!({
        "type": "text",
        "text": "PDF/document content is not supported by this Chat Completions upstream. Use the available text extracted from the document.",
    })
}

/// Mirrors `translateAnthropicToolsToOpenAI`.
#[allow(clippy::result_large_err)]
fn translate_anthropic_tools_to_openai(
    anthropic_tools: Option<&Vec<AnthropicTool>>,
) -> Result<Option<Vec<Value>>, AppError> {
    let Some(tools) = anthropic_tools else {
        return Ok(None);
    };
    let mut translated = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let path = format!("tools[{index}]");
        validate_chat_tool_controls(tool, &path)?;
        let mut function = Map::new();
        function.insert("name".to_string(), json!(tool.name));
        if let Some(description) = &tool.description {
            function.insert("description".to_string(), json!(description));
        }
        function.insert(
            "parameters".to_string(),
            normalize_tool_schema(tool.input_schema.as_ref()),
        );
        if let Some(strict) = tool.strict {
            function.insert("strict".to_string(), json!(strict));
        }
        let mut target = Map::from_iter([
            ("type".to_string(), json!("function")),
            ("function".to_string(), Value::Object(function)),
        ]);
        if let Some(cache_control) = tool.cache_control.as_ref().filter(|value| !value.is_null()) {
            target.insert("cache_control".to_string(), cache_control.clone());
        }
        merge_open_object_extensions(&tool.extra, &[], &mut target, &path)?;
        translated.push(Value::Object(target));
    }
    Ok(Some(translated))
}

#[allow(clippy::result_large_err)]
fn validate_chat_tool_controls(tool: &AnthropicTool, path: &str) -> Result<(), AppError> {
    if tool.input_schema.is_none() {
        return Err(AppError::BadRequest(format!(
            "{path}: server tools cannot be represented by Chat function tools"
        )));
    }
    if tool.defer_loading == Some(true) {
        return Err(AppError::BadRequest(format!(
            "{path}.defer_loading cannot be represented by Chat Completions"
        )));
    }
    for (field, present) in [
        ("allowed_domains", tool.allowed_domains.is_some()),
        ("blocked_domains", tool.blocked_domains.is_some()),
        ("user_location", tool.user_location.is_some()),
        ("allowed_callers", tool.allowed_callers.is_some()),
        ("response_inclusion", tool.response_inclusion.is_some()),
        ("max_uses", tool.max_uses.is_some()),
    ] {
        if present {
            return Err(AppError::BadRequest(format!(
                "{path}.{field} cannot be represented by a Chat function tool"
            )));
        }
    }
    Ok(())
}

/// Mirrors `normalizeToolSchema`: ensures a `type: "object"` schema has a
/// `properties` field (OpenAI rejects object schemas without it).
fn normalize_tool_schema(schema: Option<&Value>) -> Value {
    match schema {
        Some(Value::Object(map)) => {
            let is_object = map.get("type").and_then(|t| t.as_str()) == Some("object");
            if is_object && !map.contains_key("properties") {
                let mut new_map = map.clone();
                new_map.insert("properties".to_string(), Value::Object(Map::new()));
                Value::Object(new_map)
            } else {
                Value::Object(map.clone())
            }
        }
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

/// Mirrors `translateAnthropicToolChoiceToOpenAI`.
#[allow(clippy::result_large_err)]
fn translate_anthropic_tool_choice_to_openai(
    anthropic_tool_choice: Option<&AnthropicToolChoice>,
) -> Result<Option<Value>, AppError> {
    let Some(choice) = anthropic_tool_choice else {
        return Ok(None);
    };
    let scalar = |value: Value| -> Result<Option<Value>, AppError> {
        reject_unrepresentable_extensions(
            &choice.extra,
            "tool_choice",
            "a scalar Chat tool_choice",
        )?;
        Ok(Some(value))
    };
    match choice.kind.as_str() {
        "auto" => scalar(json!("auto")),
        "any" => scalar(json!("required")),
        "tool" => {
            let Some(name) = choice.name.as_ref() else {
                return Ok(None);
            };
            let mut target = Map::from_iter([
                ("type".to_string(), json!("function")),
                ("function".to_string(), json!({ "name": name })),
            ]);
            merge_open_object_extensions(&choice.extra, &[], &mut target, "tool_choice")?;
            Ok(Some(Value::Object(target)))
        }
        "none" => scalar(json!("none")),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// DIRECTION B: OpenAI ChatCompletion response -> Anthropic response
// ---------------------------------------------------------------------------

/// Strictly translate one completed OpenAI Chat Completion into an Anthropic
/// message. Any malformed consumed field is an upstream protocol failure, never
/// an empty/defaulted success.
#[allow(clippy::result_large_err)]
pub fn translate_to_anthropic(response: &Value) -> Result<AnthropicResponse, HttpError> {
    let object = response
        .as_object()
        .ok_or_else(|| malformed_chat_response("response was not an object"))?;
    if let Some(error) = object.get("error").filter(|error| !error.is_null()) {
        return Err(chat_body_error(error));
    }

    let id = required_nonempty_chat_string(object, "id", "response.id")?;
    let model = required_nonempty_chat_string(object, "model", "response.model")?;
    match object.get("object") {
        Some(Value::String(value)) if value == "chat.completion" => {}
        _ => return Err(malformed_chat_response("response.object was invalid")),
    }
    let created = object
        .get("created")
        .and_then(nonnegative_i64)
        .ok_or_else(|| malformed_chat_response("response.created was invalid"))?;

    validate_optional_chat_string(
        object.get("system_fingerprint"),
        "response.system_fingerprint",
    )?;
    let top_service_tier =
        parse_chat_service_tier(object.get("service_tier"), "response.service_tier")?;

    let choices = object
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed_chat_response("response.choices was not an array"))?;
    if choices.len() != 1 {
        return Err(malformed_chat_response(
            "response.choices must contain exactly one choice",
        ));
    }
    let choice = choices[0]
        .as_object()
        .ok_or_else(|| malformed_chat_response("response.choices[0] was not an object"))?;
    if choice.get("index").and_then(nonnegative_i64) != Some(0) {
        return Err(malformed_chat_response(
            "response.choices[0].index was not zero",
        ));
    }
    if choice.get("logprobs").is_some_and(|value| !value.is_null()) {
        return Err(malformed_chat_response(
            "response.choices[0].logprobs cannot be represented by Anthropic Messages",
        ));
    }

    let finish_reason = required_nonempty_chat_string(
        choice,
        "finish_reason",
        "response.choices[0].finish_reason",
    )?;
    let stop_reason = map_openai_stop_reason_to_anthropic(Some(finish_reason))
        .ok_or_else(|| malformed_chat_response("finish_reason was unsupported"))?
        .to_string();
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed_chat_response("response.choices[0].message was malformed"))?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(malformed_chat_response(
            "response.choices[0].message.role was not assistant",
        ));
    }
    validate_chat_message_known_extras(message)?;

    let mut text_content = translate_chat_response_content(
        message.get("content"),
        "response.choices[0].message.content",
    )?;
    let refusal = message
        .get("refusal")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    if let Some(refusal) = refusal {
        if text_content.is_empty() {
            text_content.push(json!({"type":"text","text":refusal}));
        } else if text_content.len() != 1
            || text_content[0].get("text").and_then(Value::as_str) != Some(refusal)
        {
            return Err(malformed_chat_response(
                "message refusal conflicted with assistant content",
            ));
        }
        if finish_reason != "content_filter" {
            return Err(malformed_chat_response(
                "message refusal conflicted with finish_reason",
            ));
        }
    }
    let mut content = translate_chat_reasoning(message)?;
    content.extend(text_content);
    let tool_blocks = translate_chat_tool_calls(message.get("tool_calls"))?;
    let has_tool_calls = !tool_blocks.is_empty();
    content.extend(tool_blocks);

    if finish_reason == "tool_calls" && !has_tool_calls {
        return Err(malformed_chat_response(
            "tool_calls finish_reason had no tool calls",
        ));
    }
    if finish_reason != "tool_calls" && has_tool_calls {
        return Err(malformed_chat_response(
            "tool calls conflicted with finish_reason",
        ));
    }
    let refusal_present = refusal.is_some();
    if content.is_empty() && !(finish_reason == "content_filter" && refusal_present) {
        return Err(malformed_chat_response(
            "completed choice contained no representable output",
        ));
    }

    let mut extra = collect_open_object_extensions(
        object,
        &["id", "model", "choices", "usage", "error"],
        ANTHROPIC_RESPONSE_CANONICAL_FIELDS,
        "response",
    )
    .map_err(chat_extension_error)?;
    // `created` was validated above and remains in source order in `extra`.
    debug_assert_eq!(extra.get("created").and_then(Value::as_i64), Some(created));
    let choice_extensions = collect_open_object_extensions(
        choice,
        &["index", "message", "finish_reason", "logprobs"],
        &[],
        "response.choices[0]",
    )
    .map_err(chat_extension_error)?;
    if !choice_extensions.is_empty() {
        extra.insert(
            "chat_choice_extensions".to_string(),
            Value::Object(choice_extensions),
        );
    }
    let message_extensions = collect_open_object_extensions(
        message,
        &[
            "role",
            "content",
            "tool_calls",
            "reasoning_text",
            "reasoning_content",
            "reasoning_opaque",
            "function_call",
            "refusal",
        ],
        &[],
        "response.choices[0].message",
    )
    .map_err(chat_extension_error)?;
    if !message_extensions.is_empty() {
        extra.insert(
            "chat_message_extensions".to_string(),
            Value::Object(message_extensions),
        );
    }

    let usage = match object.get("usage") {
        None | Some(Value::Null) => empty_chat_completion_usage(top_service_tier.as_deref()),
        Some(usage) => map_openai_chat_completion_usage(usage, top_service_tier.as_deref())?,
    };
    Ok(AnthropicResponse {
        id: id.to_string(),
        kind: "message".to_string(),
        role: "assistant".to_string(),
        model: model.to_string(),
        content,
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        usage,
        extra,
    })
}

fn malformed_chat_response(detail: &str) -> HttpError {
    tracing::warn!(detail, "malformed upstream Chat Completions response");
    HttpError::bad_gateway("The upstream Chat Completions response was malformed.")
}

fn chat_extension_error(error: AppError) -> HttpError {
    tracing::warn!(error = %error, "unsafe upstream Chat extension");
    HttpError::bad_gateway("The upstream Chat Completions response contained conflicting fields.")
}

fn chat_body_error(error: &Value) -> HttpError {
    let upstream_type =
        safe_upstream_error_type(error.get("type")).unwrap_or_else(|| "api_error".to_string());
    let message = safe_upstream_error_message(error.get("message"))
        .unwrap_or_else(|| "The upstream Chat Completions service reported an error.".to_string());
    let mut result =
        HttpError::bad_gateway("The upstream Chat Completions service reported an error.");
    result.body = json!({
        "type":"error",
        "error":{
            "type":"api_error",
            "message":message,
            "upstream_type":upstream_type
        }
    })
    .to_string();
    result
}

#[allow(clippy::result_large_err)]
fn required_nonempty_chat_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    detail: &str,
) -> Result<&'a str, HttpError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| malformed_chat_response(detail))
}

#[allow(clippy::result_large_err)]
fn validate_optional_chat_string(value: Option<&Value>, detail: &str) -> Result<(), HttpError> {
    match value {
        None | Some(Value::Null | Value::String(_)) => Ok(()),
        Some(_) => Err(malformed_chat_response(detail)),
    }
}

#[allow(clippy::result_large_err)]
fn optional_chat_string(value: Option<&Value>, detail: &str) -> Result<Option<String>, HttpError> {
    validate_optional_chat_string(value, detail)?;
    Ok(value.and_then(Value::as_str).map(str::to_string))
}

#[allow(clippy::result_large_err)]
pub(crate) fn parse_chat_service_tier(
    value: Option<&Value>,
    detail: &str,
) -> Result<Option<String>, HttpError> {
    let tier = optional_chat_string(value, detail)?;
    if let Some(tier) = tier.as_deref() {
        if !matches!(tier, "auto" | "default" | "flex" | "scale" | "priority") {
            return Err(malformed_chat_response(detail));
        }
    }
    Ok(tier)
}

#[allow(clippy::result_large_err)]
fn validate_chat_message_known_extras(message: &Map<String, Value>) -> Result<(), HttpError> {
    for (field, expected_object) in [("annotations", false), ("audio", true)] {
        if let Some(value) = message.get(field).filter(|value| !value.is_null()) {
            let valid = if expected_object {
                value.is_object()
            } else {
                value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_object))
            };
            if !valid {
                return Err(malformed_chat_response(&format!(
                    "response.choices[0].message.{field} was malformed"
                )));
            }
        }
    }
    validate_optional_chat_string(
        message.get("refusal"),
        "response.choices[0].message.refusal was malformed",
    )?;
    validate_optional_chat_string(
        message.get("name"),
        "response.choices[0].message.name was malformed",
    )?;
    if message
        .get("function_call")
        .is_some_and(|value| !value.is_null())
    {
        return Err(malformed_chat_response(
            "legacy message.function_call is unsupported",
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn translate_chat_response_content(
    content: Option<&Value>,
    path: &str,
) -> Result<Vec<Value>, HttpError> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => {
            if text.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![json!({"type":"text","text":text})])
            }
        }
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for (index, part) in parts.iter().enumerate() {
                let part_path = format!("{path}[{index}]");
                let source = part.as_object().ok_or_else(|| {
                    malformed_chat_response(&format!("{part_path} was not an object"))
                })?;
                if source.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(malformed_chat_response(&format!(
                        "{part_path}.type was unsupported"
                    )));
                }
                let text = source.get("text").and_then(Value::as_str).ok_or_else(|| {
                    malformed_chat_response(&format!("{part_path}.text was invalid"))
                })?;
                if text.is_empty() {
                    return Err(malformed_chat_response(&format!(
                        "{part_path}.text was empty"
                    )));
                }
                let extensions = collect_open_object_extensions(
                    source,
                    &["type", "text"],
                    &["type", "text"],
                    &part_path,
                )
                .map_err(chat_extension_error)?;
                let mut target = Map::from_iter([
                    ("type".to_string(), json!("text")),
                    ("text".to_string(), json!(text)),
                ]);
                target.extend(extensions);
                blocks.push(Value::Object(target));
            }
            Ok(blocks)
        }
        Some(_) => Err(malformed_chat_response(&format!(
            "{path} had an unsupported type"
        ))),
    }
}

#[allow(clippy::result_large_err)]
fn translate_chat_reasoning(message: &Map<String, Value>) -> Result<Vec<Value>, HttpError> {
    let reasoning_text = optional_chat_string(
        message.get("reasoning_text"),
        "reasoning_text was malformed",
    )?;
    let reasoning_content = optional_chat_string(
        message.get("reasoning_content"),
        "reasoning_content was malformed",
    )?;
    if let (Some(left), Some(right)) = (&reasoning_text, &reasoning_content) {
        if left != right {
            return Err(malformed_chat_response(
                "reasoning_text and reasoning_content conflicted",
            ));
        }
    }
    let reasoning = reasoning_text.or(reasoning_content);
    let opaque = optional_chat_string(
        message.get("reasoning_opaque"),
        "reasoning_opaque was malformed",
    )?;
    match (
        reasoning.filter(|value| !value.is_empty()),
        opaque.filter(|value| !value.is_empty()),
    ) {
        (Some(thinking), Some(signature)) => Ok(vec![json!({
            "type":"thinking",
            "thinking":thinking,
            "signature":encode_chat_reasoning_signature(&signature)
        })]),
        (None, Some(signature)) => Ok(vec![json!({
            "type":"thinking",
            "thinking":THINKING_TEXT,
            "signature":encode_chat_reasoning_signature(&signature)
        })]),
        (Some(_), None) => Err(malformed_chat_response(
            "reasoning text was missing its opaque signature",
        )),
        (None, None) => Ok(Vec::new()),
    }
}

#[allow(clippy::result_large_err)]
fn translate_chat_tool_calls(tool_calls: Option<&Value>) -> Result<Vec<Value>, HttpError> {
    let calls = match tool_calls {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(calls)) => calls,
        Some(_) => {
            return Err(malformed_chat_response(
                "message.tool_calls was not an array",
            ))
        }
    };
    let mut blocks = Vec::with_capacity(calls.len());
    for (index, tool_call) in calls.iter().enumerate() {
        let path = format!("response.choices[0].message.tool_calls[{index}]");
        let source = tool_call
            .as_object()
            .ok_or_else(|| malformed_chat_response(&format!("{path} was not an object")))?;
        if source.get("type").and_then(Value::as_str) != Some("function") {
            return Err(malformed_chat_response(&format!(
                "{path}.type was not function"
            )));
        }
        let id = required_nonempty_chat_string(source, "id", &format!("{path}.id was invalid"))?;
        if let Some(value) = source.get("index") {
            nonnegative_i64(value)
                .ok_or_else(|| malformed_chat_response(&format!("{path}.index was invalid")))?;
        }
        let function = source
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| malformed_chat_response(&format!("{path}.function was malformed")))?;
        let name = required_nonempty_chat_string(
            function,
            "name",
            &format!("{path}.function.name was invalid"),
        )?;
        let arguments = required_nonempty_chat_string(
            function,
            "arguments",
            &format!("{path}.function.arguments was invalid"),
        )?;
        let input: Value = serde_json::from_str(arguments)
            .map_err(|_| malformed_chat_response("tool arguments were not valid JSON"))?;
        if !input.is_object() {
            return Err(malformed_chat_response(
                "tool arguments JSON was not an object",
            ));
        }
        let function_extensions = collect_open_object_extensions(
            function,
            &["name", "arguments"],
            &[],
            &format!("{path}.function"),
        )
        .map_err(chat_extension_error)?;
        let extensions = collect_open_object_extensions(
            source,
            &["type", "id", "function"],
            ANTHROPIC_TOOL_USE_CANONICAL_FIELDS,
            &path,
        )
        .map_err(chat_extension_error)?;
        let mut target = Map::from_iter([
            ("type".to_string(), json!("tool_use")),
            ("id".to_string(), json!(id)),
            ("name".to_string(), json!(name)),
            ("input".to_string(), input),
        ]);
        if !function_extensions.is_empty() {
            target.insert(
                "chat_function_extensions".to_string(),
                Value::Object(function_extensions),
            );
        }
        target.extend(extensions);
        blocks.push(Value::Object(target));
    }
    Ok(blocks)
}

#[allow(clippy::result_large_err)]
pub(crate) fn map_openai_chat_completion_usage(
    usage: &Value,
    top_service_tier: Option<&str>,
) -> Result<AnthropicUsage, HttpError> {
    let source = usage
        .as_object()
        .ok_or_else(|| malformed_chat_response("response.usage was not an object"))?;
    let prompt_tokens = required_chat_count(source, "prompt_tokens", "usage.prompt_tokens")?;
    let completion_tokens =
        required_chat_count(source, "completion_tokens", "usage.completion_tokens")?;
    let total_tokens = required_chat_count(source, "total_tokens", "usage.total_tokens")?;
    if prompt_tokens.checked_add(completion_tokens) != Some(total_tokens) {
        return Err(malformed_chat_response(
            "usage.total_tokens was inconsistent",
        ));
    }

    let mut cached_tokens = None;
    let mut cache_creation_tokens = None;
    let mut prompt_detail_extension = None;
    if let Some(details) = source.get("prompt_tokens_details") {
        match details {
            Value::Null => prompt_detail_extension = Some(Value::Null),
            Value::Object(details) => {
                cached_tokens = optional_chat_count(details.get("cached_tokens"), "cached_tokens")?;
                cache_creation_tokens = optional_chat_count(
                    details.get("cache_creation_input_tokens"),
                    "cache_creation_input_tokens",
                )?;
                for field in ["audio_tokens"] {
                    if let Some(value) =
                        optional_chat_count(details.get(field), &format!("prompt {field}"))?
                    {
                        if value > prompt_tokens {
                            return Err(malformed_chat_response(
                                "prompt token details exceeded prompt_tokens",
                            ));
                        }
                    }
                }
                let detail_total = cached_tokens
                    .unwrap_or(0)
                    .checked_add(cache_creation_tokens.unwrap_or(0))
                    .ok_or_else(|| malformed_chat_response("prompt token details overflowed"))?;
                if detail_total > prompt_tokens {
                    return Err(malformed_chat_response(
                        "cache token details exceeded prompt_tokens",
                    ));
                }
                let extensions = collect_open_object_extensions(
                    details,
                    &["cached_tokens", "cache_creation_input_tokens"],
                    &[],
                    "response.usage.prompt_tokens_details",
                )
                .map_err(chat_extension_error)?;
                if !extensions.is_empty() {
                    prompt_detail_extension = Some(Value::Object(extensions));
                }
            }
            _ => {
                return Err(malformed_chat_response(
                    "usage.prompt_tokens_details was malformed",
                ))
            }
        }
    }
    if let Some(details) = source
        .get("completion_tokens_details")
        .filter(|details| !details.is_null())
    {
        let details = details.as_object().ok_or_else(|| {
            malformed_chat_response("usage.completion_tokens_details was malformed")
        })?;
        for field in [
            "reasoning_tokens",
            "audio_tokens",
            "accepted_prediction_tokens",
            "rejected_prediction_tokens",
        ] {
            if let Some(value) =
                optional_chat_count(details.get(field), &format!("completion {field}"))?
            {
                if value > completion_tokens {
                    return Err(malformed_chat_response(
                        "completion token details exceeded completion_tokens",
                    ));
                }
            }
        }
        let accepted = optional_chat_count(
            details.get("accepted_prediction_tokens"),
            "accepted_prediction_tokens",
        )?
        .unwrap_or(0);
        let rejected = optional_chat_count(
            details.get("rejected_prediction_tokens"),
            "rejected_prediction_tokens",
        )?
        .unwrap_or(0);
        if accepted
            .checked_add(rejected)
            .is_none_or(|sum| sum > completion_tokens)
        {
            return Err(malformed_chat_response(
                "prediction token details were inconsistent",
            ));
        }
    }

    let usage_service_tier = parse_chat_service_tier(
        source.get("service_tier"),
        "usage.service_tier was malformed",
    )?;
    if let (Some(top), Some(nested)) = (top_service_tier, usage_service_tier.as_deref()) {
        if top != nested {
            return Err(malformed_chat_response("service_tier values conflicted"));
        }
    }
    let mut extra = collect_open_object_extensions(
        source,
        &[
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "prompt_tokens_details",
            "service_tier",
        ],
        ANTHROPIC_USAGE_CANONICAL_FIELDS,
        "response.usage",
    )
    .map_err(chat_extension_error)?;
    if let Some(details) = prompt_detail_extension {
        extra.insert("chat_prompt_tokens_details".to_string(), details);
    }
    let cache_total = cached_tokens
        .unwrap_or(0)
        .checked_add(cache_creation_tokens.unwrap_or(0))
        .ok_or_else(|| malformed_chat_response("cache token details overflowed"))?;
    let input_tokens = prompt_tokens
        .checked_sub(cache_total)
        .ok_or_else(|| malformed_chat_response("cache token details were inconsistent"))?;
    Ok(AnthropicUsage {
        input_tokens,
        output_tokens: completion_tokens,
        cache_creation_input_tokens: cache_creation_tokens,
        cache_read_input_tokens: cached_tokens,
        service_tier: usage_service_tier.or_else(|| top_service_tier.map(str::to_string)),
        extra,
    })
}

pub(crate) fn empty_chat_completion_usage(service_tier: Option<&str>) -> AnthropicUsage {
    AnthropicUsage {
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        service_tier: service_tier.map(str::to_string),
        extra: Map::new(),
    }
}

#[allow(clippy::result_large_err)]
fn required_chat_count(
    object: &Map<String, Value>,
    field: &str,
    detail: &str,
) -> Result<i64, HttpError> {
    object
        .get(field)
        .and_then(nonnegative_i64)
        .ok_or_else(|| malformed_chat_response(detail))
}

#[allow(clippy::result_large_err)]
fn optional_chat_count(value: Option<&Value>, detail: &str) -> Result<Option<i64>, HttpError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => nonnegative_i64(value)
            .map(Some)
            .ok_or_else(|| malformed_chat_response(detail)),
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Reads `block.type` as a `&str` for the block-kind switches.
fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(|t| t.as_str())
}

/// Builds a simple `{ role, content }` message with string content.
fn text_message(role: &str, content: String) -> Message {
    Message {
        role: role.to_string(),
        content: Some(Value::String(content)),
        extra: Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::messages::anthropic_types::AnthropicInputMessage;

    fn user_message(content: Value) -> AnthropicInputMessage {
        AnthropicInputMessage {
            role: "user".to_string(),
            content,
            extra: Map::new(),
        }
    }

    #[test]
    fn chat_completions_preserves_stop_sequence_order_and_duplicates() {
        let payload = AnthropicMessagesPayload {
            model: "gpt-4o".to_string(),
            messages: vec![user_message(json!("hello"))],
            max_tokens: Some(100),
            stop_sequences: Some(vec![
                "z-stop".to_string(),
                "a-stop".to_string(),
                "z-stop".to_string(),
            ]),
            ..Default::default()
        };
        let translated = translate_to_openai(&payload).expect("translate chat request");
        let value = serde_json::to_value(translated).expect("serialize chat request");
        assert_eq!(value["stop"], json!(["z-stop", "a-stop", "z-stop"]));
    }

    #[test]
    fn tool_result_message_comes_before_user_message() {
        // A user message carrying a tool_result block (string content) plus a
        // trailing text block must translate to [tool message, user message].
        let payload = AnthropicMessagesPayload {
            model: "gpt-4o".to_string(),
            messages: vec![user_message(json!([
                { "type": "tool_result", "tool_use_id": "call_1", "content": "result text" },
                { "type": "text", "text": "follow up" },
            ]))],
            max_tokens: Some(100),
            ..Default::default()
        };

        let out = translate_to_openai(&payload).unwrap();
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[0].role, "tool");
        assert_eq!(
            out.messages[0]
                .extra
                .get("tool_call_id")
                .and_then(|v| v.as_str()),
            Some("call_1")
        );
        assert_eq!(
            out.messages[0].content,
            Some(Value::String("result text".to_string()))
        );
        assert_eq!(out.messages[1].role, "user");
    }

    #[test]
    fn tool_result_image_moves_to_user_message_when_unsupported() {
        // Direct helper test: image content + capabilities without "image"
        // support must move the rich content to a user message and leave a
        // text placeholder in the tool message.
        let caps = TranslationCapabilities {
            support_pdf: false,
            tool_content_support_type: vec!["array".to_string()], // no "image"
        };
        let block = json!({
            "type": "tool_result",
            "tool_use_id": "call_img",
            "content": [
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
            ],
        });

        let result = handle_tool_result_block(&block, &caps, "messages[0].content[0]").unwrap();
        assert!(result.moved_user_message.is_some());
        // No text in the original content -> placeholder text used.
        assert_eq!(
            result.tool_message.content,
            Some(Value::String(RICH_TOOL_RESULT_MOVED_TEXT.to_string()))
        );
        let moved = result.moved_user_message.unwrap();
        assert_eq!(moved.role, "user");
        // Prefix text part + the image part.
        if let Some(Value::Array(parts)) = &moved.content {
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0].get("type").and_then(|t| t.as_str()), Some("text"));
            assert_eq!(
                parts[1].get("type").and_then(|t| t.as_str()),
                Some("image_url")
            );
        } else {
            panic!("expected array content");
        }
    }

    #[test]
    fn image_url_source_passes_through_unchanged() {
        // A {"type":"image","source":{"type":"url",...}} block must keep the
        // remote URL verbatim, NOT be wrapped in a blank "data:;base64," URL.
        let payload = AnthropicMessagesPayload {
            model: "gpt-4o".to_string(),
            messages: vec![user_message(json!([
                {
                    "type": "image",
                    "source": { "type": "url", "url": "https://example.com/cat.png" },
                },
            ]))],
            max_tokens: Some(100),
            ..Default::default()
        };

        let out = translate_to_openai(&payload).unwrap();
        let parts = out.messages[0].content.as_ref().unwrap();
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["url"], "https://example.com/cat.png");
    }

    #[test]
    fn image_base64_source_still_builds_data_url() {
        // Regression guard: the base64 path must keep building the data: URL.
        let payload = AnthropicMessagesPayload {
            model: "gpt-4o".to_string(),
            messages: vec![user_message(json!([
                {
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" },
                },
            ]))],
            max_tokens: Some(100),
            ..Default::default()
        };

        let out = translate_to_openai(&payload).unwrap();
        let parts = out.messages[0].content.as_ref().unwrap();
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn image_base64_missing_or_empty_fields_is_rejected() {
        // Empty media_type/data (or a blank url) must 400 instead of building a
        // corrupt `data:;base64,` URL — the whole point of this fix.
        for source in [
            json!({ "type": "base64", "media_type": "image/png" }),
            json!({ "type": "base64", "media_type": "", "data": "AAAA" }),
            json!({ "type": "url", "url": "" }),
        ] {
            let payload = AnthropicMessagesPayload {
                model: "gpt-4o".to_string(),
                messages: vec![user_message(json!([
                    { "type": "image", "source": source },
                ]))],
                max_tokens: Some(100),
                ..Default::default()
            };
            let err = translate_to_openai(&payload).unwrap_err();
            assert!(matches!(err, AppError::BadRequest(_)), "source rejected");
        }
    }

    #[test]
    fn image_file_source_is_rejected_with_bad_request() {
        // Files-API ids can't be resolved by the Chat Completions upstream, so
        // they must surface a 400 instead of a corrupt blank image.
        let payload = AnthropicMessagesPayload {
            model: "gpt-4o".to_string(),
            messages: vec![user_message(json!([
                {
                    "type": "image",
                    "source": { "type": "file", "file_id": "file_abc123" },
                },
            ]))],
            max_tokens: Some(100),
            ..Default::default()
        };

        let err = translate_to_openai(&payload).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn system_prompt_array_is_joined() {
        let system = json!([
            { "type": "text", "text": "first" },
            { "type": "text", "text": "second" },
        ]);
        let messages = handle_system_prompt(Some(&system)).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "system");
        assert_eq!(
            messages[0].content,
            Some(Value::String("first\n\nsecond".to_string()))
        );
    }

    #[test]
    fn system_prompt_string_passthrough() {
        let system = json!("you are helpful");
        let messages = handle_system_prompt(Some(&system)).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].content,
            Some(Value::String("you are helpful".to_string()))
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn thinking_budget_clamps_within_min_and_max() {
        let mut model = Model::default();
        model.id = "m".to_string();
        model.capabilities.supports.max_thinking_budget = Some(8000);
        model.capabilities.supports.min_thinking_budget = Some(1024);
        model.capabilities.limits.max_output_tokens = Some(5000);

        // maxThinkingBudget = min(8000, 5000 - 1) = 4999
        // requested budget 10000 -> clamped to 4999, then max(4999, 1024) = 4999
        let thinking = AnthropicThinkingConfig {
            kind: "enabled".to_string(),
            budget_tokens: Some(10000).into(),
            display: Default::default(),
            extra: Default::default(),
        };
        assert_eq!(
            thinking_budget_from_model(Some(&thinking), Some(&model)),
            Some(4999)
        );

        // requested budget 100 -> below min -> raised to 1024
        let thinking_low = AnthropicThinkingConfig {
            kind: "enabled".to_string(),
            budget_tokens: Some(100).into(),
            display: Default::default(),
            extra: Default::default(),
        };
        assert_eq!(
            thinking_budget_from_model(Some(&thinking_low), Some(&model)),
            Some(1024)
        );

        let disabled = AnthropicThinkingConfig {
            kind: "disabled".to_string(),
            ..Default::default()
        };
        assert_eq!(
            thinking_budget_from_model(Some(&disabled), Some(&model)),
            None
        );
    }

    #[test]
    fn thinking_budget_none_when_max_not_positive() {
        let mut model = Model::default();
        model.capabilities.supports.max_thinking_budget = Some(0);
        model.capabilities.limits.max_output_tokens = Some(0);
        let thinking = AnthropicThinkingConfig {
            kind: "enabled".to_string(),
            budget_tokens: Some(500).into(),
            display: Default::default(),
            extra: Default::default(),
        };
        assert_eq!(
            thinking_budget_from_model(Some(&thinking), Some(&model)),
            None
        );
        // No thinking config -> None.
        assert_eq!(thinking_budget_from_model(None, Some(&model)), None);
    }

    #[test]
    fn translate_to_anthropic_with_tool_use_response() {
        let response = json!({
            "id": "chatcmpl-1",
            "object":"chat.completion",
            "created":1,
            "model": "gpt-4o",
            "choices": [
                {
                    "index": 0,
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": "let me check",
                        "tool_calls": [
                            {
                                "id": "call_a",
                                "type": "function",
                                "function": { "name": "get_weather", "arguments": "{\"city\":\"SF\"}" }
                            }
                        ]
                    }
                }
            ],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        });

        let out = translate_to_anthropic(&response).unwrap();
        assert_eq!(out.id, "chatcmpl-1");
        assert_eq!(out.model, "gpt-4o");
        assert_eq!(out.stop_reason.as_deref(), Some("tool_use"));
        // text block then tool_use block.
        assert_eq!(out.content.len(), 2);
        assert_eq!(
            out.content[0].get("type").and_then(|t| t.as_str()),
            Some("text")
        );
        assert_eq!(
            out.content[0].get("text").and_then(|t| t.as_str()),
            Some("let me check")
        );
        assert_eq!(
            out.content[1].get("type").and_then(|t| t.as_str()),
            Some("tool_use")
        );
        assert_eq!(
            out.content[1].get("name").and_then(|t| t.as_str()),
            Some("get_weather")
        );
        assert_eq!(
            out.content[1]
                .get("input")
                .and_then(|i| i.get("city"))
                .and_then(|c| c.as_str()),
            Some("SF")
        );
        assert_eq!(out.usage.input_tokens, 10);
        assert_eq!(out.usage.output_tokens, 5);
    }

    #[test]
    fn translate_tools_normalizes_object_schema_without_properties() {
        let tools = vec![AnthropicTool {
            name: Some("foo".to_string()),
            description: Some("bar".to_string()),
            input_schema: Some(json!({ "type": "object" })),
            ..Default::default()
        }];
        let out = translate_anthropic_tools_to_openai(Some(&tools))
            .unwrap()
            .unwrap();
        assert_eq!(out.len(), 1);
        let params = out[0]
            .get("function")
            .and_then(|f| f.get("parameters"))
            .unwrap();
        assert!(params.get("properties").is_some());
    }

    #[test]
    fn tool_choice_maps_variants() {
        let auto = AnthropicToolChoice {
            kind: "auto".to_string(),
            name: None,
            extra: Default::default(),
        };
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(Some(&auto)).unwrap(),
            Some(json!("auto"))
        );
        let any = AnthropicToolChoice {
            kind: "any".to_string(),
            name: None,
            extra: Default::default(),
        };
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(Some(&any)).unwrap(),
            Some(json!("required"))
        );
        let tool = AnthropicToolChoice {
            kind: "tool".to_string(),
            name: Some("t".to_string()),
            extra: Default::default(),
        };
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(Some(&tool)).unwrap(),
            Some(json!({ "type": "function", "function": { "name": "t" } }))
        );
        let none = AnthropicToolChoice {
            kind: "none".to_string(),
            name: None,
            extra: Default::default(),
        };
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(Some(&none)).unwrap(),
            Some(json!("none"))
        );
    }

    #[test]
    fn translate_options_thread_support_pdf() {
        // A user message carrying a document block: with support_pdf=false the
        // document is replaced by a text placeholder; with support_pdf=true it
        // becomes a `file` content part.
        let payload = AnthropicMessagesPayload {
            model: "gpt-4o".to_string(),
            messages: vec![user_message(json!([
                {
                    "type": "document",
                    "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "JVBER0=",
                    },
                },
            ]))],
            max_tokens: Some(100),
            ..Default::default()
        };

        // Default options: PDF unsupported -> text placeholder.
        let default_out =
            translate_to_openai_with_options(&payload, &TranslateToOpenAiOptions::default())
                .unwrap();
        let default_parts = default_out.messages[0].content.as_ref().unwrap();
        assert_eq!(default_parts[0]["type"], "text");

        // support_pdf=true -> file part.
        let pdf_out = translate_to_openai_with_options(
            &payload,
            &TranslateToOpenAiOptions {
                support_pdf: true,
                tool_content_support_type: vec!["array".to_string(), "image".to_string()],
            },
        )
        .unwrap();
        let pdf_parts = pdf_out.messages[0].content.as_ref().unwrap();
        assert_eq!(pdf_parts[0]["type"], "file");
    }
}
