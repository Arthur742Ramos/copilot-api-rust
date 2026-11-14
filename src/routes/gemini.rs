//! Gemini Developer API compatibility routes for Gemini CLI.
//!
//! Requests are translated to the Anthropic Messages shape and dispatched
//! through the `/v1/messages` handler, which already routes each model to the
//! Copilot Messages, Responses, or Chat Completions backend it supports.

use std::collections::{BTreeMap, HashMap, VecDeque};

use axum::body::{Body, Bytes};
use axum::extract::OriginalUri;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{json, Map, Value};

use crate::libs::config::resolve_mapped_model;
use crate::libs::error::AppError;
use crate::routes::messages::count_tokens_handler::handle_count_tokens;
use crate::routes::messages::handler::handle_completion;
use crate::routes::parse_json_body;

/// Used when the model catalogue does not advertise an output limit.
const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 8192;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Generate,
    Stream,
    CountTokens,
}

pub async fn post_model_operation(
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (model, operation) = match parse_model_operation(uri.path()) {
        Ok(parsed) => parsed,
        Err(message) => return gemini_error(StatusCode::NOT_FOUND, message),
    };
    let body = match parse_json_body(&body) {
        Ok(body) => body,
        Err(error) => return app_error(error),
    };

    if operation == Operation::CountTokens {
        // countTokens accepts either a bare request or one wrapped in
        // `generateContentRequest`.
        let source = body.get("generateContentRequest").unwrap_or(&body);
        return match translate_request(&model, source, None) {
            Ok((request, _)) => count_tokens(request, headers).await,
            Err(message) => gemini_error(StatusCode::BAD_REQUEST, message),
        };
    }

    let stream = operation == Operation::Stream;
    let (request, include_thoughts) = match translate_request(&model, &body, Some(stream)) {
        Ok(translated) => translated,
        Err(message) => return gemini_error(StatusCode::BAD_REQUEST, message),
    };
    let response = match handle_completion(request, headers).await {
        Ok(response) => response,
        Err(error) => return app_error(error),
    };
    if !response.status().is_success() {
        return translate_error_response(response).await;
    }
    if stream {
        stream_response(response, include_thoughts)
    } else {
        translate_response(response, include_thoughts).await
    }
}

pub async fn get_models() -> Response {
    if crate::libs::state::with_state(|state| state.models.is_none()) {
        if let Err(error) = crate::libs::utils::cache_models().await {
            return gemini_error(StatusCode::BAD_GATEWAY, error.to_string());
        }
    }
    let models = crate::libs::state::with_state(|state| state.models.clone());
    let Some(models) = models else {
        return gemini_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Model catalogue is unavailable",
        );
    };
    let output: Vec<Value> = models
        .data
        .iter()
        .map(|model| {
            let mut item = json!({
                "name": format!("models/{}", model.id),
                "displayName": model.name,
                "supportedGenerationMethods": ["generateContent", "countTokens"],
            });
            if let Some(input_limit) = model.capabilities.limits.max_context_window_tokens {
                item["inputTokenLimit"] = json!(input_limit);
            }
            if let Some(output_limit) = model.capabilities.limits.max_output_tokens {
                item["outputTokenLimit"] = json!(output_limit);
            }
            item
        })
        .collect();
    Json(json!({"models": output})).into_response()
}

fn parse_model_operation(path: &str) -> Result<(String, Operation), &'static str> {
    let suffix = path
        .strip_prefix("/v1beta/models/")
        .or_else(|| path.strip_prefix("/v1/models/"))
        .ok_or("Expected a versioned Gemini models endpoint")?;
    for (operation_suffix, operation) in [
        (":streamGenerateContent", Operation::Stream),
        (":generateContent", Operation::Generate),
        (":countTokens", Operation::CountTokens),
    ] {
        if let Some(model) = suffix.strip_suffix(operation_suffix) {
            let model = model.strip_prefix("models/").unwrap_or(model);
            if model.is_empty() || model.contains('/') {
                return Err("Invalid Gemini model name");
            }
            return Ok((model.to_string(), operation));
        }
    }
    Err("Unsupported Gemini model operation")
}

/// Translates a Gemini request into an Anthropic Messages request. `stream` is
/// `None` for token counting, which omits generation-only controls. Returns
/// whether the client asked to receive thought parts.
fn translate_request(
    model: &str,
    body: &Value,
    stream: Option<bool>,
) -> Result<(Value, bool), String> {
    let contents = body
        .get("contents")
        .and_then(Value::as_array)
        .ok_or("contents must be an array")?;
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut pending_calls: HashMap<String, VecDeque<String>> = HashMap::new();
    let mut generated_ids = 0usize;

    for content in contents {
        let role = match content
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
        {
            "user" => "user",
            "model" => "assistant",
            _ => return Err("contents[].role must be user or model".to_string()),
        };
        let parts = content
            .get("parts")
            .and_then(Value::as_array)
            .ok_or("contents[].parts must be an array")?;

        let mut tool_results = Vec::new();
        let mut blocks = Vec::new();
        let mut thought: Option<(String, Option<String>)> = None;
        for part in parts {
            if !part.is_object()
                || !["text", "inlineData", "functionCall", "functionResponse"]
                    .iter()
                    .any(|key| part.get(key).is_some())
            {
                return Err(if part.get("fileData").is_some() {
                    "fileData parts are not supported; inline the file data".to_string()
                } else {
                    "Unsupported Gemini content part".to_string()
                });
            }
            let is_thought = part.get("thought").and_then(Value::as_bool) == Some(true);
            if is_thought && role == "assistant" {
                // Streamed thoughts arrive as several parts; the signature
                // belongs to the whole run.
                let entry = thought.get_or_insert_with(Default::default);
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    entry.0.push_str(text);
                }
                if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                    entry.1 = Some(signature.to_string());
                }
                continue;
            }
            flush_thought(&mut thought, &mut blocks);
            if is_thought {
                continue;
            }
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            if let Some(inline_data) = part.get("inlineData") {
                blocks.push(translate_inline_data(inline_data)?);
            }
            if let Some(call) = part.get("functionCall") {
                if role != "assistant" {
                    return Err("functionCall parts must use the model role".to_string());
                }
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or("functionCall.name must be a non-empty string")?;
                let id = match call.get("id").and_then(Value::as_str) {
                    Some(id) if !id.is_empty() => id.to_string(),
                    _ => {
                        generated_ids += 1;
                        format!("gemini_call_{generated_ids}")
                    }
                };
                let input = call.get("args").cloned().unwrap_or_else(|| json!({}));
                if !input.is_object() {
                    return Err("functionCall.args must be an object".to_string());
                }
                pending_calls
                    .entry(name.to_string())
                    .or_default()
                    .push_back(id.clone());
                blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
            }
            if let Some(response) = part.get("functionResponse") {
                if role != "user" {
                    return Err("functionResponse parts must use the user role".to_string());
                }
                tool_results.push(translate_function_response(response, &mut pending_calls)?);
            }
        }
        flush_thought(&mut thought, &mut blocks);

        // Anthropic requires tool results to lead the user turn that answers
        // the preceding tool calls.
        tool_results.extend(blocks);
        if tool_results.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => {
                last_blocks.extend(tool_results)
            }
            _ => messages.push((role, tool_results)),
        }
    }

    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    let mut request = json!({"model": model, "messages": messages});
    if let Some(system) = body.get("systemInstruction") {
        let text = translate_system_instruction(system.get("parts"))?;
        if !text.is_empty() {
            request["system"] = json!(text);
        }
    }
    if let Some(tools) = body.get("tools") {
        request["tools"] = translate_tools(tools)?;
    }
    if let Some(tool_config) = body.get("toolConfig") {
        translate_tool_config(tool_config, &mut request)?;
    }
    if body
        .get("safetySettings")
        .and_then(Value::as_array)
        .is_some_and(|settings| !settings.is_empty())
    {
        return Err("Gemini safetySettings cannot be represented by this backend".to_string());
    }
    if body.get("cachedContent").is_some() {
        return Err("Gemini cachedContent is not supported".to_string());
    }

    let mut include_thoughts = false;
    if let Some(stream) = stream {
        request["stream"] = json!(stream);
        if let Some(config) = body.get("generationConfig") {
            include_thoughts = translate_generation_config(config, &mut request)?;
        }
        let (max_output_tokens, endpoints) = catalogue_entry(model);
        if request.get("max_tokens").is_none() {
            request["max_tokens"] = json!(max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS));
        }
        // Gemini CLI always sends topP; Responses-only reasoning models reject it.
        if endpoints.is_some_and(|endpoints| endpoints.iter().all(|e| e.contains("/responses"))) {
            if let Some(request) = request.as_object_mut() {
                request.remove("top_p");
            }
        }
    }
    Ok((request, include_thoughts))
}

fn flush_thought(thought: &mut Option<(String, Option<String>)>, blocks: &mut Vec<Value>) {
    // Unsigned thoughts cannot be replayed to the backend, so they are dropped.
    if let Some((text, Some(signature))) = thought.take() {
        blocks.push(json!({"type": "thinking", "thinking": text, "signature": signature}));
    }
}

fn translate_inline_data(inline_data: &Value) -> Result<Value, String> {
    let mime_type = inline_data
        .get("mimeType")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("inlineData.mimeType must be a non-empty string")?;
    let data = inline_data
        .get("data")
        .and_then(Value::as_str)
        .ok_or("inlineData.data must be a base64 string")?;
    let kind = if mime_type.starts_with("image/") {
        "image"
    } else if mime_type == "application/pdf" {
        "document"
    } else {
        return Err(format!("inlineData mimeType {mime_type} is not supported"));
    };
    Ok(json!({
        "type": kind,
        "source": {"type": "base64", "media_type": mime_type, "data": data}
    }))
}

fn translate_function_response(
    response: &Value,
    pending_calls: &mut HashMap<String, VecDeque<String>>,
) -> Result<Value, String> {
    let name = response
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or("functionResponse.name must be a non-empty string")?;
    let queued = pending_calls.get_mut(name).and_then(VecDeque::pop_front);
    let id = response
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or(queued)
        .ok_or_else(|| format!("functionResponse {name} has no matching functionCall"))?;
    let result = response
        .get("response")
        .cloned()
        .unwrap_or_else(|| json!({}));
    // Gemini CLI wraps tool output as `{output}` or `{error}`; unwrap those so
    // the model sees the tool text rather than a JSON envelope.
    let (text, is_error) = match result.as_object() {
        Some(object) if object.len() == 1 && object.get("output").is_some_and(Value::is_string) => {
            (
                object["output"].as_str().unwrap_or_default().to_string(),
                false,
            )
        }
        Some(object) if object.len() == 1 && object.get("error").is_some_and(Value::is_string) => (
            object["error"].as_str().unwrap_or_default().to_string(),
            true,
        ),
        _ => (
            serde_json::to_string(&result).map_err(|error| error.to_string())?,
            false,
        ),
    };
    let mut block = json!({"type": "tool_result", "tool_use_id": id, "content": text});
    if is_error {
        block["is_error"] = json!(true);
    }
    Ok(block)
}

fn translate_system_instruction(parts: Option<&Value>) -> Result<String, String> {
    let Some(parts) = parts.and_then(Value::as_array) else {
        return Ok(String::new());
    };
    let mut text = Vec::new();
    for part in parts {
        let Some(value) = part.get("text").and_then(Value::as_str) else {
            return Err("System instructions only support text parts".to_string());
        };
        text.push(value);
    }
    Ok(text.join(""))
}

fn translate_tools(tools: &Value) -> Result<Value, String> {
    let tools = tools.as_array().ok_or("tools must be an array")?;
    let mut translated = Vec::new();
    for tool in tools {
        let declarations = tool
            .get("functionDeclarations")
            .and_then(Value::as_array)
            .ok_or("Only functionDeclarations tools are supported")?;
        for declaration in declarations {
            let name = declaration
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or("functionDeclarations[].name must be a non-empty string")?;
            let input_schema = match declaration.get("parametersJsonSchema") {
                Some(schema) => schema.clone(),
                None => declaration
                    .get("parameters")
                    .map(openapi_schema_to_json_schema)
                    .unwrap_or_else(|| json!({"type": "object"})),
            };
            let mut translated_tool = json!({"name": name, "input_schema": input_schema});
            if let Some(description) = declaration.get("description") {
                translated_tool["description"] = description.clone();
            }
            translated.push(translated_tool);
        }
    }
    Ok(Value::Array(translated))
}

/// Gemini's OpenAPI-subset `Schema` spells types in upper case (`OBJECT`);
/// JSON Schema requires lower case.
fn openapi_schema_to_json_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = match (key.as_str(), value) {
                        ("type", Value::String(kind)) => json!(kind.to_ascii_lowercase()),
                        _ => openapi_schema_to_json_schema(value),
                    };
                    (key.clone(), value)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(openapi_schema_to_json_schema).collect())
        }
        other => other.clone(),
    }
}

fn translate_tool_config(config: &Value, request: &mut Value) -> Result<(), String> {
    let Some(function_config) = config.get("functionCallingConfig") else {
        return Err("Only functionCallingConfig is supported in toolConfig".to_string());
    };
    if let Some(names) = function_config
        .get("allowedFunctionNames")
        .and_then(Value::as_array)
    {
        let allowed: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
        if let Some(tools) = request["tools"].as_array_mut() {
            tools.retain(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| allowed.contains(&name))
            });
        }
    }
    let mode = function_config
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("AUTO");
    request["tool_choice"] = match mode {
        "AUTO" | "VALIDATED" => json!({"type": "auto"}),
        "ANY" => json!({"type": "any"}),
        "NONE" => json!({"type": "none"}),
        _ => return Err("Unsupported functionCallingConfig.mode".to_string()),
    };
    Ok(())
}

/// Applies `generationConfig` and returns whether thought parts were requested.
fn translate_generation_config(config: &Value, request: &mut Value) -> Result<bool, String> {
    let config = config
        .as_object()
        .ok_or("generationConfig must be an object")?;
    for (source, target) in [
        ("temperature", "temperature"),
        ("topP", "top_p"),
        ("stopSequences", "stop_sequences"),
        ("maxOutputTokens", "max_tokens"),
    ] {
        if let Some(value) = config.get(source) {
            request[target] = value.clone();
        }
    }
    if config
        .get("candidateCount")
        .and_then(Value::as_u64)
        .is_some_and(|count| count != 1)
    {
        return Err("candidateCount must be 1".to_string());
    }
    if let Some(mime_type) = config.get("responseMimeType").and_then(Value::as_str) {
        match mime_type {
            "text/plain" => {}
            "application/json" => {
                let schema = config
                    .get("responseJsonSchema")
                    .cloned()
                    .or_else(|| {
                        config
                            .get("responseSchema")
                            .map(openapi_schema_to_json_schema)
                    })
                    .ok_or("application/json responses require responseJsonSchema")?;
                request["output_config"]["format"] =
                    json!({"type": "json_schema", "schema": schema});
            }
            _ => return Err("Unsupported generationConfig.responseMimeType".to_string()),
        }
    }
    let mut include_thoughts = false;
    if let Some(thinking) = config.get("thinkingConfig") {
        include_thoughts = thinking.get("includeThoughts").and_then(Value::as_bool) == Some(true);
        if let Some(level) = thinking.get("thinkingLevel").and_then(Value::as_str) {
            let effort = level.to_ascii_lowercase();
            if !matches!(effort.as_str(), "minimal" | "low" | "medium" | "high") {
                return Err("Unsupported generationConfig.thinkingConfig.thinkingLevel".to_string());
            }
            request["output_config"]["effort"] = json!(effort);
        } else if let Some(budget) = thinking.get("thinkingBudget").and_then(Value::as_i64) {
            // -1 asks the model to choose its own budget.
            match budget {
                0 => request["thinking"] = json!({"type": "disabled"}),
                1..=2048 => request["output_config"]["effort"] = json!("low"),
                2049..=8192 => request["output_config"]["effort"] = json!("medium"),
                8193.. => request["output_config"]["effort"] = json!("high"),
                _ => {}
            }
        }
    }
    // topK is a sampling hint Gemini CLI always sends; the Responses backend
    // rejects it, so it is ignored for every model.
    let supported = [
        "temperature",
        "topP",
        "topK",
        "stopSequences",
        "candidateCount",
        "maxOutputTokens",
        "responseMimeType",
        "responseJsonSchema",
        "responseSchema",
        "thinkingConfig",
    ];
    if let Some(field) = config
        .keys()
        .find(|field| !supported.contains(&field.as_str()))
    {
        return Err(format!("Unsupported generationConfig field: {field}"));
    }
    Ok(include_thoughts)
}

/// Returns the catalogued output limit and supported endpoints for a model.
fn catalogue_entry(model: &str) -> (Option<i64>, Option<Vec<String>>) {
    let model = resolve_mapped_model(model);
    crate::libs::state::with_state(|state| {
        let entry = state
            .models
            .as_ref()?
            .data
            .iter()
            .find(|candidate| candidate.id == model)?;
        Some((
            entry.capabilities.limits.max_output_tokens,
            entry.supported_endpoints.clone(),
        ))
    })
    .unwrap_or_default()
}

async fn count_tokens(request: Value, headers: HeaderMap) -> Response {
    let response = match handle_count_tokens(request, headers).await {
        Ok(response) => response,
        Err(error) => return app_error(error),
    };
    if !response.status().is_success() {
        return translate_error_response(response).await;
    }
    match read_json(response).await {
        Ok(body) => match body.get("input_tokens").and_then(Value::as_i64) {
            Some(total) => Json(json!({"totalTokens": total})).into_response(),
            None => gemini_error(StatusCode::BAD_GATEWAY, "Token count was unavailable"),
        },
        Err(response) => response,
    }
}

async fn read_json(response: Response) -> Result<Value, Response> {
    let body = response.into_body().collect().await.map_err(|error| {
        gemini_error(
            StatusCode::BAD_GATEWAY,
            format!("Unable to read model response: {error}"),
        )
    })?;
    serde_json::from_slice(&body.to_bytes()).map_err(|error| {
        gemini_error(
            StatusCode::BAD_GATEWAY,
            format!("Model returned invalid JSON: {error}"),
        )
    })
}

async fn translate_response(response: Response, include_thoughts: bool) -> Response {
    let message = match read_json(response).await {
        Ok(message) => message,
        Err(response) => return response,
    };
    let mut parts = Vec::new();
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(json!({"text": text}));
                }
            }
            Some("thinking") if include_thoughts => {
                parts.push(thought_part(
                    block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    block.get("signature").and_then(Value::as_str),
                ));
            }
            Some("tool_use") => parts.push(function_call_part(
                block.get("id").and_then(Value::as_str).unwrap_or_default(),
                block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                block.get("input").cloned().unwrap_or_else(|| json!({})),
            )),
            _ => {}
        }
    }
    let mut result = json!({"candidates": [{
        "index": 0,
        "content": {"role": "model", "parts": parts},
        "finishReason": map_stop_reason(message.get("stop_reason").and_then(Value::as_str)),
    }]});
    if let Some(model) = message.get("model") {
        result["modelVersion"] = model.clone();
    }
    if let Some(usage) = translate_usage(message.get("usage")) {
        result["usageMetadata"] = usage;
    }
    Json(result).into_response()
}

fn thought_part(text: &str, signature: Option<&str>) -> Value {
    let mut part = json!({"text": text, "thought": true});
    if let Some(signature) = signature.filter(|signature| !signature.is_empty()) {
        part["thoughtSignature"] = json!(signature);
    }
    part
}

fn function_call_part(id: &str, name: &str, args: Value) -> Value {
    let mut call = json!({"name": name, "args": args});
    if !id.is_empty() {
        call["id"] = json!(id);
    }
    json!({"functionCall": call})
}

fn map_stop_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") => "MAX_TOKENS",
        Some("refusal") => "SAFETY",
        _ => "STOP",
    }
}

/// Anthropic reports cached input separately; Gemini's prompt count includes it.
fn translate_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage?.as_object()?;
    let field = |name: &str| usage.get(name).and_then(Value::as_i64).unwrap_or(0);
    let cached = field("cache_read_input_tokens");
    let prompt = field("input_tokens") + cached + field("cache_creation_input_tokens");
    let output = field("output_tokens");
    let mut metadata = json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": output,
        "totalTokenCount": prompt + output,
    });
    if cached > 0 {
        metadata["cachedContentTokenCount"] = json!(cached);
    }
    Some(metadata)
}

#[derive(Default)]
struct StreamState {
    include_thoughts: bool,
    model: Option<Value>,
    usage: Map<String, Value>,
    stop_reason: Option<String>,
    stopped: bool,
    tools: BTreeMap<u64, (String, String, String)>,
    signatures: BTreeMap<u64, String>,
}

fn stream_response(response: Response, include_thoughts: bool) -> Response {
    let body_stream = response.into_body().into_data_stream();
    let stream = async_stream::stream! {
        futures_util::pin_mut!(body_stream);
        let mut buffer = Vec::new();
        let mut state = StreamState { include_thoughts, ..Default::default() };
        let mut finished = false;
        while !finished {
            let event = match find_event_boundary(&buffer) {
                Some((boundary, delimiter_len)) => {
                    buffer.drain(..boundary + delimiter_len).collect::<Vec<u8>>()
                }
                None => match body_stream.next().await {
                    Some(Ok(bytes)) => {
                        buffer.extend_from_slice(&bytes);
                        continue;
                    }
                    Some(Err(error)) => {
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                    None => {
                        finished = true;
                        std::mem::take(&mut buffer)
                    }
                },
            };
            match process_stream_event(&event, &mut state) {
                Ok(Some(frame)) => yield Ok::<Bytes, std::io::Error>(Bytes::from(frame)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "Gemini stream aborted");
                    yield Err(std::io::Error::other(error));
                    return;
                }
            }
        }
        if !state.stopped {
            tracing::warn!("Gemini stream ended before message_stop");
            yield Err(std::io::Error::other("Model stream ended before message_stop"));
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .expect("valid Gemini stream response")
}

fn process_stream_event(event: &[u8], state: &mut StreamState) -> Result<Option<String>, String> {
    let text = std::str::from_utf8(event).map_err(|error| error.to_string())?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    let event: Value = serde_json::from_str(&data)
        .map_err(|error| format!("Invalid model stream JSON: {error}"))?;
    let index = event
        .get("index")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let mut parts = Vec::new();
    match event.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            if let Some(model) = event.pointer("/message/model") {
                state.model = Some(model.clone());
            }
            if let Some(usage) = event.pointer("/message/usage").and_then(Value::as_object) {
                state.usage.extend(usage.clone());
            }
        }
        Some("content_block_start") => {
            let block = event.get("content_block").unwrap_or(&Value::Null);
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    state
                        .tools
                        .insert(index, (id.to_string(), name.to_string(), String::new()));
                }
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            parts.push(json!({"text": text}));
                        }
                    }
                }
                _ => {}
            }
        }
        Some("content_block_delta") => {
            let delta = event.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        parts.push(json!({"text": text}));
                    }
                }
                Some("thinking_delta") if state.include_thoughts => {
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                        parts.push(thought_part(text, None));
                    }
                }
                Some("signature_delta") => {
                    if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                        state
                            .signatures
                            .entry(index)
                            .or_default()
                            .push_str(signature);
                    }
                }
                Some("input_json_delta") => {
                    if let (Some(tool), Some(json)) = (
                        state.tools.get_mut(&index),
                        delta.get("partial_json").and_then(Value::as_str),
                    ) {
                        tool.2.push_str(json);
                    }
                }
                _ => {}
            }
        }
        Some("content_block_stop") => {
            if let Some((id, name, arguments)) = state.tools.remove(&index) {
                let args = if arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&arguments).map_err(|error| {
                        format!("Model returned invalid function arguments: {error}")
                    })?
                };
                parts.push(function_call_part(&id, &name, args));
            }
            if let Some(signature) = state.signatures.remove(&index) {
                if state.include_thoughts {
                    parts.push(thought_part("", Some(&signature)));
                }
            }
        }
        Some("message_delta") => {
            if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                state.stop_reason = Some(reason.to_string());
            }
            if let Some(usage) = event.get("usage").and_then(Value::as_object) {
                state.usage.extend(usage.clone());
            }
        }
        Some("message_stop") => {
            state.stopped = true;
            let mut chunk = json!({"candidates": [{
                "index": 0,
                "content": {"role": "model", "parts": []},
                "finishReason": map_stop_reason(state.stop_reason.as_deref()),
            }]});
            if let Some(usage) = translate_usage(Some(&Value::Object(state.usage.clone()))) {
                chunk["usageMetadata"] = usage;
            }
            if let Some(model) = &state.model {
                chunk["modelVersion"] = model.clone();
            }
            return Ok(Some(format!("data: {chunk}\n\n")));
        }
        Some("error") => {
            let message = event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Model stream failed");
            return Err(message.to_string());
        }
        _ => {}
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "data: {}\n\n",
        json!({"candidates": [{
            "index": 0,
            "content": {"role": "model", "parts": parts}
        }]})
    )))
}

fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf < lf => Some((crlf, 4)),
        (Some(_), Some(lf)) => Some((lf, 2)),
        (Some(crlf), None) => Some((crlf, 4)),
        (None, Some(lf)) => Some((lf, 2)),
        (None, None) => None,
    }
}

async fn translate_error_response(response: Response) -> Response {
    let status = response.status();
    let message = match response.into_body().collect().await {
        Ok(body) => {
            let bytes = body.to_bytes();
            serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/message")
                        .or_else(|| value.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| {
                    status
                        .canonical_reason()
                        .unwrap_or("Request failed")
                        .to_string()
                })
        }
        Err(error) => format!("Unable to read error response: {error}"),
    };
    gemini_error(status, message)
}

fn app_error(error: AppError) -> Response {
    match error {
        AppError::BadRequest(message) => gemini_error(StatusCode::BAD_REQUEST, message),
        AppError::Http(error) => gemini_error(
            error.status,
            if error.expose_message {
                error.message
            } else {
                "The model request failed".to_string()
            },
        ),
        AppError::Other(_) => {
            gemini_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }
    }
}

fn gemini_error(status: StatusCode, message: impl Into<String>) -> Response {
    crate::libs::error::gemini_error_response(status, message)
}
