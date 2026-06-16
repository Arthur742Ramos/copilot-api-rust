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

use crate::libs::state::with_state;
use crate::routes::messages::anthropic_types::{
    AnthropicMessagesPayload, AnthropicResponse, AnthropicThinkingConfig, AnthropicTool,
    AnthropicToolChoice, AnthropicUsage,
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
pub const THINKING_TEXT: &str = "Thinking...";

/// Inserted into the tool message when rich content had to be relocated to a
/// user message because the upstream does not support it in tool messages.
pub const RICH_TOOL_RESULT_MOVED_TEXT: &str =
    "Rich tool result content was moved to a user message because this upstream does not support it in tool messages.";

/// `COPILOT_TOOL_CONTENT_SUPPORT_TYPE = ["array", "image"]`.
const COPILOT_TOOL_CONTENT_SUPPORT_TYPE: [&str; 2] = ["array", "image"];

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
/// default capabilities apply.
pub fn translate_to_openai(payload: &AnthropicMessagesPayload) -> ChatCompletionsPayload {
    let model_id = payload.model.clone();
    let thinking_budget = get_thinking_budget(payload);
    let capabilities = TranslationCapabilities::default();

    let messages = translate_anthropic_messages_to_openai(payload, &model_id, &capabilities);

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
    if let Some(user) = payload.metadata.as_ref().and_then(|m| m.user_id.clone()) {
        extra.insert("user".to_string(), json!(user));
    }
    if let Some(tools) = translate_anthropic_tools_to_openai(payload.tools.as_ref()) {
        extra.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) =
        translate_anthropic_tool_choice_to_openai(payload.tool_choice.as_ref())
    {
        extra.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(budget) = thinking_budget {
        extra.insert("thinking_budget".to_string(), json!(budget));
    }

    ChatCompletionsPayload {
        messages,
        model: model_id,
        max_tokens: Some(payload.max_tokens),
        stream: payload.stream,
        extra,
    }
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

    let supports = &model.capabilities.supports;
    let limits = &model.capabilities.limits;

    let max_thinking_budget = std::cmp::min(
        supports.max_thinking_budget.unwrap_or(0),
        limits.max_output_tokens.unwrap_or(0) - 1,
    );

    // thinking.budget_tokens ??= maxThinkingBudget
    let budget_tokens = thinking.budget_tokens.unwrap_or(max_thinking_budget);

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
fn translate_anthropic_messages_to_openai(
    payload: &AnthropicMessagesPayload,
    model_id: &str,
    capabilities: &TranslationCapabilities,
) -> Vec<Message> {
    let mut out = handle_system_prompt(payload.system.as_ref());
    for message in &payload.messages {
        if message.role == "user" {
            out.extend(handle_user_message(&message.content, capabilities));
        } else {
            out.extend(handle_assistant_message(
                &message.content,
                model_id,
                capabilities,
            ));
        }
    }
    out
}

/// Mirrors `handleSystemPrompt`.
fn handle_system_prompt(system: Option<&Value>) -> Vec<Message> {
    let system = match system {
        Some(v) if !v.is_null() => v,
        _ => return Vec::new(),
    };

    match system {
        Value::String(s) => {
            // Empty string is falsy in the TS `if (!system)` check.
            if s.is_empty() {
                return Vec::new();
            }
            vec![text_message("system", s.clone())]
        }
        Value::Array(blocks) => {
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
            vec![text_message("system", system_text)]
        }
        _ => Vec::new(),
    }
}

/// Mirrors `handleUserMessage`. Tool results MUST come first to maintain the
/// protocol ordering: tool_use -> tool_result -> user.
fn handle_user_message(content: &Value, capabilities: &TranslationCapabilities) -> Vec<Message> {
    let mut new_messages: Vec<Message> = Vec::new();

    if let Value::Array(blocks) = content {
        let tool_result_blocks: Vec<&Value> = blocks
            .iter()
            .filter(|b| block_type(b) == Some("tool_result"))
            .collect();
        let other_blocks: Vec<Value> = blocks
            .iter()
            .filter(|b| block_type(b) != Some("tool_result"))
            .cloned()
            .collect();

        let mut moved_tool_result_user_messages: Vec<Message> = Vec::new();
        for block in tool_result_blocks {
            let result = handle_tool_result_block(block, capabilities);
            new_messages.push(result.tool_message);
            if let Some(moved) = result.moved_user_message {
                moved_tool_result_user_messages.push(moved);
            }
        }
        new_messages.extend(moved_tool_result_user_messages);

        if !other_blocks.is_empty() {
            let mapped = map_content(&Value::Array(other_blocks), capabilities.support_pdf);
            new_messages.push(Message {
                role: "user".to_string(),
                content: mapped,
                extra: Map::new(),
            });
        }
    } else {
        new_messages.push(Message {
            role: "user".to_string(),
            content: map_content(content, false),
            extra: Map::new(),
        });
    }

    new_messages
}

/// Mirrors `handleToolResultBlock`.
fn handle_tool_result_block(
    block: &Value,
    capabilities: &TranslationCapabilities,
) -> ToolResultMessages {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content = block.get("content");

    // String content -> straight tool message.
    if let Some(Value::String(s)) = content {
        return ToolResultMessages {
            moved_user_message: None,
            tool_message: create_tool_message(&tool_use_id, Some(Value::String(s.clone()))),
        };
    }

    // Non-array (and non-string) content -> empty tool message.
    let blocks = match content {
        Some(Value::Array(a)) => a,
        _ => {
            return ToolResultMessages {
                moved_user_message: None,
                tool_message: create_tool_message(&tool_use_id, Some(Value::String(String::new()))),
            };
        }
    };

    let support = get_tool_content_support(capabilities);
    let has_image = blocks.iter().any(|b| block_type(b) == Some("image"));
    let has_document = blocks.iter().any(|b| block_type(b) == Some("document"));
    let content_value = map_content(&Value::Array(blocks.clone()), capabilities.support_pdf);

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
        return ToolResultMessages {
            moved_user_message: Some(create_tool_result_user_message(
                block,
                capabilities.support_pdf,
            )),
            tool_message: create_tool_message(&tool_use_id, Some(Value::String(tool_text))),
        };
    }

    let has_rich_content = has_image || has_pdf_file;
    if support.array || has_rich_content {
        return ToolResultMessages {
            moved_user_message: None,
            tool_message: create_tool_message(&tool_use_id, content_value),
        };
    }

    ToolResultMessages {
        moved_user_message: None,
        tool_message: create_tool_message(
            &tool_use_id,
            Some(Value::String(get_text_tool_content(&content_value))),
        ),
    }
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
fn create_tool_message(tool_call_id: &str, content: Option<Value>) -> Message {
    let mut extra = Map::new();
    extra.insert("tool_call_id".to_string(), json!(tool_call_id));
    Message {
        role: "tool".to_string(),
        content,
        extra,
    }
}

/// Mirrors `createToolResultUserMessage`.
fn create_tool_result_user_message(block: &Value, support_pdf: bool) -> Message {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let prefix = json!({
        "type": "text",
        "text": format!("Tool result for {tool_use_id}:"),
    });
    let content = map_content(block.get("content").unwrap_or(&Value::Null), support_pdf);

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

    Message {
        role: "user".to_string(),
        content: Some(Value::Array(parts)),
        extra: Map::new(),
    }
}

/// Mirrors `handleAssistantMessage`.
fn handle_assistant_message(
    content: &Value,
    model_id: &str,
    capabilities: &TranslationCapabilities,
) -> Vec<Message> {
    let blocks = match content {
        Value::Array(a) => a,
        _ => {
            return vec![Message {
                role: "assistant".to_string(),
                content: map_content(content, false),
                extra: Map::new(),
            }];
        }
    };

    let tool_use_blocks: Vec<&Value> = blocks
        .iter()
        .filter(|b| block_type(b) == Some("tool_use"))
        .collect();

    let mut thinking_blocks: Vec<&Value> = blocks
        .iter()
        .filter(|b| block_type(b) == Some("thinking"))
        .collect();

    if model_id.starts_with("claude") {
        thinking_blocks.retain(|b| {
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
        .filter_map(|b| {
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

    let signature = thinking_blocks.iter().find_map(|b| {
        let s = b.get("signature").and_then(|s| s.as_str()).unwrap_or("");
        if !s.is_empty() {
            Some(s.to_string())
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
            .map(|tool_use| {
                let id = tool_use.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = tool_use.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = tool_use.get("input").cloned().unwrap_or_else(|| json!({}));
                let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    },
                })
            })
            .collect();
        extra.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    vec![Message {
        role: "assistant".to_string(),
        content: map_content(content, capabilities.support_pdf),
        extra,
    }]
}

/// Mirrors `mapContent`. Returns `Some(string)` / `Some(array)` / `None`,
/// matching the TS `string | Array<ContentPart> | null`.
fn map_content(content: &Value, support_pdf: bool) -> Option<Value> {
    if let Value::String(s) = content {
        return Some(Value::String(s.clone()));
    }
    let blocks = match content {
        Value::Array(a) => a,
        _ => return None,
    };

    let mut content_parts: Vec<Value> = Vec::new();
    for block in blocks {
        match block_type(block) {
            Some("text") => {
                content_parts.push(json!({
                    "type": "text",
                    "text": block.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                }));
            }
            Some("image") => {
                let media_type = block
                    .get("source")
                    .and_then(|s| s.get("media_type"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                let data = block
                    .get("source")
                    .and_then(|s| s.get("data"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                content_parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{media_type};base64,{data}"),
                    },
                }));
            }
            Some("document") => {
                if support_pdf {
                    content_parts.push(create_document_file_part(block));
                } else {
                    content_parts.push(create_document_text_part());
                }
            }
            Some("tool_reference") => {
                let tool_name = block
                    .get("tool_name")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                content_parts.push(json!({
                    "type": "text",
                    "text": format!("Tool {tool_name} loaded"),
                }));
            }
            // No default
            _ => {}
        }
    }

    if content_parts.is_empty() {
        return Some(Value::String(String::new()));
    }
    Some(Value::Array(content_parts))
}

/// Mirrors `createDocumentTextPart`.
fn create_document_text_part() -> Value {
    json!({
        "type": "text",
        "text": "PDF/document content is not supported by this Chat Completions upstream. Use the available text extracted from the document.",
    })
}

/// Mirrors `createDocumentFilePart`.
fn create_document_file_part(block: &Value) -> Value {
    let media_type = block
        .get("source")
        .and_then(|s| s.get("media_type"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let data = block
        .get("source")
        .and_then(|s| s.get("data"))
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let filename = block
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("document.pdf");
    json!({
        "type": "file",
        "file": {
            "file_data": format!("data:{media_type};base64,{data}"),
            "filename": filename,
        },
    })
}

/// Mirrors `translateAnthropicToolsToOpenAI`.
fn translate_anthropic_tools_to_openai(
    anthropic_tools: Option<&Vec<AnthropicTool>>,
) -> Option<Vec<Value>> {
    let tools = anthropic_tools?;
    Some(
        tools
            .iter()
            .map(|tool| {
                let mut function = Map::new();
                function.insert("name".to_string(), json!(tool.name));
                if let Some(description) = &tool.description {
                    function.insert("description".to_string(), json!(description));
                }
                function.insert(
                    "parameters".to_string(),
                    normalize_tool_schema(tool.input_schema.as_ref()),
                );
                json!({
                    "type": "function",
                    "function": Value::Object(function),
                })
            })
            .collect(),
    )
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
fn translate_anthropic_tool_choice_to_openai(
    anthropic_tool_choice: Option<&AnthropicToolChoice>,
) -> Option<Value> {
    let choice = anthropic_tool_choice?;
    match choice.kind.as_str() {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "tool" => choice.name.as_ref().map(|name| {
            json!({
                "type": "function",
                "function": { "name": name },
            })
        }),
        "none" => Some(json!("none")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// DIRECTION B: OpenAI ChatCompletion response -> Anthropic response
// ---------------------------------------------------------------------------

/// Mirrors `translateToAnthropic`. The OpenAI response is dynamic, so it is read
/// from a `serde_json::Value` (matching the existing crate style).
pub fn translate_to_anthropic(response: &Value) -> AnthropicResponse {
    let empty: Vec<Value> = Vec::new();
    let choices = response
        .get("choices")
        .and_then(|c| c.as_array())
        .unwrap_or(&empty);

    let mut assistant_content_blocks: Vec<Value> = Vec::new();
    // stopReason = response.choices[0]?.finish_reason ?? null
    let mut stop_reason: Option<String> = choices
        .first()
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .map(|s| s.to_string());

    for choice in choices {
        let message = choice.get("message").unwrap_or(&Value::Null);

        let text_blocks = get_anthropic_text_blocks(message.get("content"));
        let reasoning_text = get_openai_reasoning_text(message);
        let reasoning_opaque = message
            .get("reasoning_opaque")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string());
        let think_blocks =
            get_anthropic_think_blocks(reasoning_text.as_deref(), reasoning_opaque.as_deref());
        let tool_use_blocks = get_anthropic_tool_use_blocks(message.get("tool_calls"));

        assistant_content_blocks.extend(think_blocks);
        assistant_content_blocks.extend(text_blocks);
        assistant_content_blocks.extend(tool_use_blocks);

        // Use the finish_reason from the first choice, or prioritize tool_calls.
        let finish_reason = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());
        if finish_reason.as_deref() == Some("tool_calls") || stop_reason.as_deref() == Some("stop")
        {
            stop_reason = finish_reason;
        }
    }

    AnthropicResponse {
        id: response
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        kind: "message".to_string(),
        role: "assistant".to_string(),
        model: response
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        content: assistant_content_blocks,
        stop_reason: map_openai_stop_reason_to_anthropic(stop_reason.as_deref())
            .map(|s| s.to_string()),
        stop_sequence: None,
        usage: map_openai_chat_completion_usage(response),
    }
}

/// Mirrors `mapOpenAIChatCompletionUsage`.
fn map_openai_chat_completion_usage(response: &Value) -> AnthropicUsage {
    let usage = response.get("usage");
    let prompt_details = usage.and_then(|u| u.get("prompt_tokens_details"));
    let prompt_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cached_tokens = prompt_details
        .and_then(|p| p.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cache_creation_tokens = prompt_details
        .and_then(|p| p.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let mut usage_out = AnthropicUsage {
        input_tokens: std::cmp::max(0, prompt_tokens - cached_tokens - cache_creation_tokens),
        output_tokens: usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        service_tier: None,
    };

    // `!== undefined` checks: present (even if 0) -> emit.
    if prompt_details
        .and_then(|p| p.get("cache_creation_input_tokens"))
        .is_some()
    {
        usage_out.cache_creation_input_tokens = Some(cache_creation_tokens);
    }
    if prompt_details
        .and_then(|p| p.get("cached_tokens"))
        .is_some()
    {
        usage_out.cache_read_input_tokens = Some(cached_tokens);
    }

    usage_out
}

/// Mirrors `getOpenAIReasoningText`: `reasoning_text ?? reasoning_content`.
fn get_openai_reasoning_text(message: &Value) -> Option<String> {
    if let Some(s) = message.get("reasoning_text").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    message
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Mirrors `getAnthropicTextBlocks`.
fn get_anthropic_text_blocks(message_content: Option<&Value>) -> Vec<Value> {
    match message_content {
        Some(Value::String(s)) if !s.is_empty() => {
            vec![json!({ "type": "text", "text": s })]
        }
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("text"))
            .map(|part| {
                json!({
                    "type": "text",
                    "text": part.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Mirrors `getAnthropicThinkBlocks`.
fn get_anthropic_think_blocks(
    reasoning_text: Option<&str>,
    reasoning_opaque: Option<&str>,
) -> Vec<Value> {
    if let Some(rt) = reasoning_text {
        if !rt.is_empty() {
            return vec![json!({
                "type": "thinking",
                "thinking": rt,
                "signature": reasoning_opaque.unwrap_or(""),
            })];
        }
    }
    if let Some(ro) = reasoning_opaque {
        if !ro.is_empty() {
            return vec![json!({
                "type": "thinking",
                // Compatible with opencode (filters empty thinking text).
                "thinking": THINKING_TEXT,
                "signature": ro,
            })];
        }
    }
    Vec::new()
}

/// Mirrors `getAnthropicToolUseBlocks`.
fn get_anthropic_tool_use_blocks(tool_calls: Option<&Value>) -> Vec<Value> {
    let calls = match tool_calls.and_then(|v| v.as_array()) {
        Some(c) => c,
        None => return Vec::new(),
    };
    calls
        .iter()
        .map(|tool_call| {
            let id = tool_call.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = tool_call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let arguments = tool_call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let input: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
            json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })
        })
        .collect()
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
    fn tool_result_message_comes_before_user_message() {
        // A user message carrying a tool_result block (string content) plus a
        // trailing text block must translate to [tool message, user message].
        let payload = AnthropicMessagesPayload {
            model: "gpt-4o".to_string(),
            messages: vec![user_message(json!([
                { "type": "tool_result", "tool_use_id": "call_1", "content": "result text" },
                { "type": "text", "text": "follow up" },
            ]))],
            max_tokens: 100,
            ..Default::default()
        };

        let out = translate_to_openai(&payload);
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

        let result = handle_tool_result_block(&block, &caps);
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
    fn system_prompt_array_is_joined() {
        let system = json!([
            { "type": "text", "text": "first" },
            { "type": "text", "text": "second" },
        ]);
        let messages = handle_system_prompt(Some(&system));
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
        let messages = handle_system_prompt(Some(&system));
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
            budget_tokens: Some(10000),
            display: None,
        };
        assert_eq!(
            thinking_budget_from_model(Some(&thinking), Some(&model)),
            Some(4999)
        );

        // requested budget 100 -> below min -> raised to 1024
        let thinking_low = AnthropicThinkingConfig {
            kind: "enabled".to_string(),
            budget_tokens: Some(100),
            display: None,
        };
        assert_eq!(
            thinking_budget_from_model(Some(&thinking_low), Some(&model)),
            Some(1024)
        );
    }

    #[test]
    fn thinking_budget_none_when_max_not_positive() {
        let mut model = Model::default();
        model.capabilities.supports.max_thinking_budget = Some(0);
        model.capabilities.limits.max_output_tokens = Some(0);
        let thinking = AnthropicThinkingConfig {
            kind: "enabled".to_string(),
            budget_tokens: Some(500),
            display: None,
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

        let out = translate_to_anthropic(&response);
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
        let out = translate_anthropic_tools_to_openai(Some(&tools)).unwrap();
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
        };
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(Some(&auto)),
            Some(json!("auto"))
        );
        let any = AnthropicToolChoice {
            kind: "any".to_string(),
            name: None,
        };
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(Some(&any)),
            Some(json!("required"))
        );
        let tool = AnthropicToolChoice {
            kind: "tool".to_string(),
            name: Some("t".to_string()),
        };
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(Some(&tool)),
            Some(json!({ "type": "function", "function": { "name": "t" } }))
        );
        let none = AnthropicToolChoice {
            kind: "none".to_string(),
            name: None,
        };
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(Some(&none)),
            Some(json!("none"))
        );
    }
}
