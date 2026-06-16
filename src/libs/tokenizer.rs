//! Port of `src/lib/tokenizer.ts`.
//!
//! GPT-style token estimation used by the `/v1/messages/count_tokens` endpoints.
//! The TS port walks dynamic objects (`Object.entries`) over the OpenAI
//! chat-completions payload; we mirror that by serializing the typed `Message`
//! values to `serde_json::Value` and walking the resulting maps.
//!
//! Encoders come from `tiktoken-rs`. `gpt-tokenizer` (the TS dependency) and
//! `tiktoken` share the same BPE tables, so the encodings line up:
//! `o200k_base`, `cl100k_base`, `p50k_base`, `p50k_edit`, `r50k_base`, with
//! `o200k_base` as the fallback for unknown names.

use once_cell::sync::Lazy;
use serde_json::Value;
use tiktoken_rs::CoreBPE;

use crate::services::copilot::create_chat_completions::ChatCompletionsPayload;
use crate::services::copilot::get_models::Model;

// --- Encoder cache ---------------------------------------------------------

static O200K_BASE: Lazy<CoreBPE> =
    Lazy::new(|| tiktoken_rs::o200k_base().expect("o200k_base encoder"));
static CL100K_BASE: Lazy<CoreBPE> =
    Lazy::new(|| tiktoken_rs::cl100k_base().expect("cl100k_base encoder"));
static P50K_BASE: Lazy<CoreBPE> =
    Lazy::new(|| tiktoken_rs::p50k_base().expect("p50k_base encoder"));
static P50K_EDIT: Lazy<CoreBPE> =
    Lazy::new(|| tiktoken_rs::p50k_edit().expect("p50k_edit encoder"));
static R50K_BASE: Lazy<CoreBPE> =
    Lazy::new(|| tiktoken_rs::r50k_base().expect("r50k_base encoder"));

/// Resolve an encoding name to its `CoreBPE`, mirroring `getEncodeChatFunction`
/// (unknown encodings fall back to `o200k_base`).
fn get_encoder(encoding: &str) -> &'static CoreBPE {
    match encoding {
        "o200k_base" => &O200K_BASE,
        "cl100k_base" => &CL100K_BASE,
        "p50k_base" => &P50K_BASE,
        "p50k_edit" => &P50K_EDIT,
        "r50k_base" => &R50K_BASE,
        _ => &O200K_BASE,
    }
}

/// `encoder.encode(text).length` — count tokens, including special tokens, the
/// way `gpt-tokenizer`'s `encode` does.
fn encode_len(encoder: &CoreBPE, text: &str) -> i64 {
    encoder.encode_with_special_tokens(text).len() as i64
}

// --- Constants -------------------------------------------------------------

/// Mirrors the object returned by `getModelConstants`.
#[derive(Debug, Clone, Copy)]
pub struct Constants {
    pub func_init: i64,
    pub prop_init: i64,
    pub prop_key: i64,
    pub enum_init: i64,
    pub enum_item: i64,
    pub func_end: i64,
    pub is_gpt: bool,
}

/// Mirrors `getModelConstants`.
pub fn get_model_constants(model: &Model) -> Constants {
    if model.id == "gpt-3.5-turbo" || model.id == "gpt-4" {
        Constants {
            func_init: 10,
            prop_init: 3,
            prop_key: 3,
            enum_init: -3,
            enum_item: 3,
            func_end: 12,
            is_gpt: true,
        }
    } else {
        Constants {
            func_init: 7,
            prop_init: 3,
            prop_key: 3,
            enum_init: -3,
            enum_item: 3,
            func_end: 12,
            is_gpt: model.id.starts_with("gpt-"),
        }
    }
}

/// Mirrors `getTokenizerFromModel`.
pub fn get_tokenizer_from_model(model: &Model) -> String {
    if model.capabilities.tokenizer.is_empty() {
        "o200k_base".to_string()
    } else {
        model.capabilities.tokenizer.clone()
    }
}

// --- JS string coercion ----------------------------------------------------

/// Emulate JS `String(value)` for the values we encounter (enum items).
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        // Objects/arrays are unusual here; fall back to JSON for determinism.
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Emulate `typeof value === "string" ? value : JSON.stringify(value)`.
fn string_or_json(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// --- Tool-call tokens ------------------------------------------------------

/// Mirrors `calculateToolCallsTokens`. `tool_calls` is the dynamic array value
/// from the message.
fn calculate_tool_calls_tokens(tool_calls: &[Value], encoder: &CoreBPE, c: &Constants) -> i64 {
    let mut tokens = 0;
    for tool_call in tool_calls {
        tokens += c.func_init;
        let id = tool_call.get("id").and_then(Value::as_str).unwrap_or("");
        tokens += encode_len(encoder, id);
        let func = tool_call.get("function");
        let name = func
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        tokens += encode_len(encoder, name);
        let arguments = func
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("");
        tokens += encode_len(encoder, arguments);
    }
    tokens += c.func_end;
    tokens
}

// --- Content-part tokens ---------------------------------------------------

/// Mirrors `calculateContentPartsTokens`.
fn calculate_content_parts_tokens(content_parts: &[Value], encoder: &CoreBPE) -> i64 {
    let mut tokens = 0;
    for part in content_parts {
        let part_type = part.get("type").and_then(Value::as_str);
        if part_type == Some("image_url") {
            let url = part
                .get("image_url")
                .and_then(|i| i.get("url"))
                .and_then(Value::as_str)
                .unwrap_or("");
            tokens += encode_len(encoder, url) + 85;
        } else if part_type == Some("file") {
            let file = part.get("file");
            let file_data = file
                .and_then(|f| f.get("file_data"))
                .and_then(Value::as_str)
                .unwrap_or("");
            tokens += encode_len(encoder, file_data);
            if let Some(filename) = file.and_then(|f| f.get("filename")).and_then(Value::as_str) {
                tokens += encode_len(encoder, filename);
            }
        } else if let Some(text) = part.get("text").and_then(Value::as_str) {
            tokens += encode_len(encoder, text);
        }
    }
    tokens
}

// --- Message tokens --------------------------------------------------------

/// Mirrors `calculateMessageTokens`. `message` is the message object serialized
/// to a JSON map so we can walk `Object.entries` like the TS port.
fn calculate_message_tokens(
    message: &serde_json::Map<String, Value>,
    encoder: &CoreBPE,
    c: &Constants,
) -> i64 {
    let tokens_per_message = 3;
    let tokens_per_name = 1;
    let mut tokens = tokens_per_message;
    for (key, value) in message {
        if key == "reasoning_opaque" {
            continue;
        }
        if let Value::String(s) = value {
            tokens += encode_len(encoder, s);
        }
        if key == "name" {
            tokens += tokens_per_name;
        }
        if key == "tool_calls" {
            if let Value::Array(arr) = value {
                tokens += calculate_tool_calls_tokens(arr, encoder, c);
            }
        }
        if key == "content" {
            if let Value::Array(arr) = value {
                tokens += calculate_content_parts_tokens(arr, encoder);
            }
        }
    }
    tokens
}

/// Mirrors `calculateTokens`.
fn calculate_tokens(
    messages: &[serde_json::Map<String, Value>],
    encoder: &CoreBPE,
    c: &Constants,
) -> i64 {
    if messages.is_empty() {
        return 0;
    }
    let mut num_tokens = 0;
    for message in messages {
        num_tokens += calculate_message_tokens(message, encoder, c);
    }
    // every reply is primed with <|start|>assistant<|message|>
    num_tokens += 3;
    num_tokens
}

// --- Tool / parameter tokens -----------------------------------------------

/// Mirrors `calculateParameterTokens`.
fn calculate_parameter_tokens(key: &str, prop: &Value, encoder: &CoreBPE, c: &Constants) -> i64 {
    let mut tokens = c.prop_key;

    let Value::Object(param) = prop else {
        return tokens;
    };

    let param_name = key;
    let param_type = param
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string");
    let mut param_desc = param
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Handle enum values
    if let Some(Value::Array(items)) = param.get("enum") {
        tokens += c.enum_init;
        for item in items {
            tokens += c.enum_item;
            tokens += encode_len(encoder, &js_string(item));
        }
    }

    // Clean up description
    if param_desc.ends_with('.') {
        param_desc.pop();
    }

    let line = format!("{param_name}:{param_type}:{param_desc}");
    tokens += encode_len(encoder, &line);

    if param.get("type").and_then(Value::as_str) == Some("array") {
        if let Some(items) = param.get("items") {
            tokens += calculate_parameters_tokens(items, encoder, c);
        }
    }

    // Handle additional properties (excluding standard ones)
    for (property_name, property_value) in param {
        if matches!(
            property_name.as_str(),
            "type" | "description" | "enum" | "items"
        ) {
            continue;
        }
        let property_text = string_or_json(property_value);
        tokens += encode_len(encoder, &format!("{property_name}:{property_text}"));
    }

    tokens
}

/// Mirrors `calculatePropertiesTokens`.
fn calculate_properties_tokens(
    properties: &serde_json::Map<String, Value>,
    encoder: &CoreBPE,
    c: &Constants,
) -> i64 {
    let mut tokens = 0;
    if !properties.is_empty() {
        tokens += c.prop_init;
        for (prop_key, value) in properties {
            tokens += calculate_parameter_tokens(prop_key, value, encoder, c);
        }
    }
    tokens
}

/// Mirrors `calculateParametersTokens`.
fn calculate_parameters_tokens(parameters: &Value, encoder: &CoreBPE, c: &Constants) -> i64 {
    let Value::Object(params) = parameters else {
        return 0;
    };

    let mut tokens = 0;
    for (key, value) in params {
        if matches!(key.as_str(), "$schema" | "additionalProperties") {
            continue;
        }
        if key == "properties" {
            if let Value::Object(props) = value {
                tokens += calculate_properties_tokens(props, encoder, c);
            }
            // A non-object `properties` value contributes nothing, matching the
            // TS cast that would yield an empty `Object.keys`.
        } else {
            let param_text = string_or_json(value);
            tokens += encode_len(encoder, &format!("{key}:{param_text}"));
        }
    }

    tokens
}

/// Mirrors `calculateToolTokens`. `tool` is the dynamic tool object.
fn calculate_tool_tokens(tool: &Value, encoder: &CoreBPE, c: &Constants) -> i64 {
    let mut tokens = c.func_init;
    let func = tool.get("function");
    let f_name = func
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut f_desc = func
        .and_then(|f| f.get("description"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if f_desc.ends_with('.') {
        f_desc.pop();
    }
    let line = format!("{f_name}:{f_desc}");
    tokens += encode_len(encoder, &line);
    if let Some(parameters) = func.and_then(|f| f.get("parameters")) {
        if parameters.is_object() {
            tokens += calculate_parameters_tokens(parameters, encoder, c);
        }
    }
    tokens
}

/// Mirrors `numTokensForTools`. `tools` is the dynamic tools array.
pub fn num_tokens_for_tools(tools: &[Value], encoder: &CoreBPE, c: &Constants) -> i64 {
    let mut func_token_count = 0;
    if c.is_gpt {
        for tool in tools {
            func_token_count += calculate_tool_tokens(tool, encoder, c);
        }
        func_token_count += c.func_end;
    } else {
        for tool in tools {
            let json = serde_json::to_string(tool).unwrap_or_default();
            func_token_count += encode_len(encoder, &json);
        }
    }
    func_token_count
}

// --- Public entrypoint -----------------------------------------------------

/// Mirrors `getTokenCount`. Synchronous — tiktoken encoding is CPU-bound and
/// needs no async. Returns `(input, output)` token counts.
pub fn get_token_count(payload: &ChatCompletionsPayload, model: &Model) -> (i64, i64) {
    let tokenizer = get_tokenizer_from_model(model);
    let encoder = get_encoder(&tokenizer);
    let constants = get_model_constants(model);

    // Serialize each message to a JSON object so we can walk its entries like
    // the TS `Object.entries(message)`.
    let mut input_messages: Vec<serde_json::Map<String, Value>> = Vec::new();
    let mut output_messages: Vec<serde_json::Map<String, Value>> = Vec::new();
    for message in &payload.messages {
        let obj = match serde_json::to_value(message) {
            Ok(Value::Object(map)) => map,
            _ => continue,
        };
        if message.role == "assistant" {
            output_messages.push(obj);
        } else {
            input_messages.push(obj);
        }
    }

    let mut input_tokens = calculate_tokens(&input_messages, encoder, &constants);

    if let Some(Value::Array(tools)) = payload.extra.get("tools") {
        if !tools.is_empty() {
            input_tokens += num_tokens_for_tools(tools, encoder, &constants);
        }
    }

    let output_tokens = calculate_tokens(&output_messages, encoder, &constants);

    (input_tokens, output_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::copilot::create_chat_completions::Message;
    use serde_json::json;

    fn model_with_tokenizer(id: &str, tokenizer: &str) -> Model {
        let mut m = Model {
            id: id.to_string(),
            ..Default::default()
        };
        m.capabilities.tokenizer = tokenizer.to_string();
        m
    }

    fn msg(role: &str, content: Value) -> Message {
        Message {
            role: role.to_string(),
            content: Some(content),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn tokenizer_defaults_to_o200k() {
        let m = Model::default();
        assert_eq!(get_tokenizer_from_model(&m), "o200k_base");
    }

    #[test]
    fn constants_for_gpt4_and_others() {
        let gpt4 = model_with_tokenizer("gpt-4", "o200k_base");
        let c = get_model_constants(&gpt4);
        assert_eq!(c.func_init, 10);
        assert!(c.is_gpt);

        let gpt5 = model_with_tokenizer("gpt-5", "o200k_base");
        let c2 = get_model_constants(&gpt5);
        assert_eq!(c2.func_init, 7);
        assert!(c2.is_gpt);

        let claude = model_with_tokenizer("claude-opus-4.1", "o200k_base");
        let c3 = get_model_constants(&claude);
        assert!(!c3.is_gpt);
    }

    #[test]
    fn input_and_output_split_by_role() {
        let payload = ChatCompletionsPayload {
            messages: vec![
                msg("user", json!("hello world")),
                msg("assistant", json!("hi there")),
            ],
            model: "gpt-5".to_string(),
            max_tokens: None,
            stream: None,
            extra: serde_json::Map::new(),
        };
        let model = model_with_tokenizer("gpt-5", "o200k_base");
        let (input, output) = get_token_count(&payload, &model);
        // Each non-empty side primes 3 (per message) + 3 (reply prime) + content.
        assert!(input > 3);
        assert!(output > 3);
    }

    #[test]
    fn empty_messages_zero() {
        let payload = ChatCompletionsPayload {
            messages: vec![],
            model: "gpt-5".to_string(),
            max_tokens: None,
            stream: None,
            extra: serde_json::Map::new(),
        };
        let model = model_with_tokenizer("gpt-5", "o200k_base");
        let (input, output) = get_token_count(&payload, &model);
        assert_eq!(input, 0);
        assert_eq!(output, 0);
    }

    #[test]
    fn tools_add_tokens_for_gpt() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "tools".to_string(),
            json!([
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get the weather.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "location": { "type": "string", "description": "City name" }
                            }
                        }
                    }
                }
            ]),
        );
        let payload = ChatCompletionsPayload {
            messages: vec![msg("user", json!("hi"))],
            model: "gpt-5".to_string(),
            max_tokens: None,
            stream: None,
            extra,
        };
        let model = model_with_tokenizer("gpt-5", "o200k_base");
        let (input_with_tools, _) = get_token_count(&payload, &model);

        let mut no_tools = payload.clone();
        no_tools.extra.clear();
        let (input_without_tools, _) = get_token_count(&no_tools, &model);

        assert!(input_with_tools > input_without_tools);
    }

    #[test]
    fn content_parts_image_adds_85() {
        let payload = ChatCompletionsPayload {
            messages: vec![msg(
                "user",
                json!([{ "type": "image_url", "image_url": { "url": "x" } }]),
            )],
            model: "gpt-5".to_string(),
            max_tokens: None,
            stream: None,
            extra: serde_json::Map::new(),
        };
        let model = model_with_tokenizer("gpt-5", "o200k_base");
        let (input, _) = get_token_count(&payload, &model);
        // 3 (per message) + 3 (reply prime) + encode("x") + 85.
        assert!(input >= 3 + 3 + 85);
    }
}
