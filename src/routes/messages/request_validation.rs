//! Validation for known Anthropic request shapes that are otherwise carried as
//! `serde_json::Value`.
//!
//! The Messages translators intentionally keep open objects so unknown keys can
//! survive. Known fields must still fail closed before admission/provider
//! dispatch instead of being dropped by `as_*`, `filter_map`, or defaults.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::libs::error::AppError;

type ToolCatalog = HashMap<String, bool>;
type ToolUseCatalog = HashMap<String, String>;

fn invalid(path: &str, expectation: &str) -> AppError {
    AppError::BadRequest(format!("{path}: {expectation}"))
}

fn required_object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, AppError> {
    value
        .as_object()
        .ok_or_else(|| invalid(path, "must be an object"))
}

fn required_nonempty_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<&'a str, AppError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid(&format!("{path}.{field}"), "must be a non-empty string"))
}

fn validate_optional_string(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
    nonempty: bool,
) -> Result<(), AppError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(value)) if !nonempty || !value.trim().is_empty() => Ok(()),
        Some(Value::String(_)) => Err(invalid(
            &format!("{path}.{field}"),
            "must be non-empty when provided",
        )),
        Some(_) => Err(invalid(
            &format!("{path}.{field}"),
            "must be a string or null",
        )),
    }
}

fn validate_optional_bool(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), AppError> {
    match object.get(field) {
        None | Some(Value::Null | Value::Bool(_)) => Ok(()),
        Some(_) => Err(invalid(
            &format!("{path}.{field}"),
            "must be a boolean or null",
        )),
    }
}

fn validate_cache_control(value: &Value, path: &str) -> Result<(), AppError> {
    if value.is_null() {
        return Ok(());
    }
    let object = required_object(value, path)?;
    if required_nonempty_string(object, "type", path)? != "ephemeral" {
        return Err(invalid(&format!("{path}.type"), "must equal \"ephemeral\""));
    }
    validate_optional_string(object, "ttl", path, true)?;
    validate_optional_string(object, "scope", path, true)?;
    Ok(())
}

fn validate_cache_control_field(object: &Map<String, Value>, path: &str) -> Result<(), AppError> {
    if let Some(cache_control) = object.get("cache_control") {
        validate_cache_control(cache_control, &format!("{path}.cache_control"))?;
    }
    Ok(())
}

fn validate_string_array(
    value: &Value,
    path: &str,
    allow_empty: bool,
) -> Result<Vec<String>, AppError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let array = value
        .as_array()
        .ok_or_else(|| invalid(path, "must be an array or null"))?;
    if !allow_empty && array.is_empty() {
        return Err(invalid(path, "must contain at least one entry"));
    }
    array
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| invalid(&format!("{path}[{index}]"), "must be a non-empty string"))
        })
        .collect()
}

fn validate_user_location(value: &Value, path: &str) -> Result<(), AppError> {
    if value.is_null() {
        return Ok(());
    }
    let object = required_object(value, path)?;
    if required_nonempty_string(object, "type", path)? != "approximate" {
        return Err(invalid(
            &format!("{path}.type"),
            "must equal \"approximate\"",
        ));
    }
    let mut has_location = false;
    for field in ["city", "region", "country", "timezone"] {
        validate_optional_string(object, field, path, true)?;
        has_location |= object
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
    }
    if !has_location {
        return Err(invalid(
            path,
            "must include at least one of city, region, country, or timezone",
        ));
    }
    if let Some(country) = object.get("country").and_then(Value::as_str) {
        if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err(invalid(
                &format!("{path}.country"),
                "must be a two-letter country code",
            ));
        }
    }
    Ok(())
}

fn validate_json_schema(value: &Value, path: &str) -> Result<(), AppError> {
    let object = required_object(value, path)?;
    validate_optional_string(object, "type", path, true)?;
    if let Some(properties) = object.get("properties") {
        if !properties.is_object() {
            return Err(invalid(&format!("{path}.properties"), "must be an object"));
        }
    }
    if let Some(required) = object.get("required") {
        if required.is_null() {
            return Err(invalid(&format!("{path}.required"), "must be an array"));
        }
        validate_string_array(required, &format!("{path}.required"), true)?;
    }
    Ok(())
}

fn validate_web_search_tool(tool: &Map<String, Value>, path: &str) -> Result<(), AppError> {
    if required_nonempty_string(tool, "name", path)? != "web_search" {
        return Err(invalid(
            &format!("{path}.name"),
            "must equal \"web_search\"",
        ));
    }
    if tool
        .get("input_schema")
        .is_some_and(|schema| !schema.is_null())
    {
        return Err(invalid(
            &format!("{path}.input_schema"),
            "must be omitted or null for a server tool",
        ));
    }

    let allowed = match tool.get("allowed_domains") {
        Some(value) => validate_string_array(value, &format!("{path}.allowed_domains"), true)?,
        None => Vec::new(),
    };
    let blocked = match tool.get("blocked_domains") {
        Some(value) => validate_string_array(value, &format!("{path}.blocked_domains"), true)?,
        None => Vec::new(),
    };
    if !allowed.is_empty() && !blocked.is_empty() {
        return Err(invalid(
            path,
            "allowed_domains and blocked_domains cannot both be non-empty",
        ));
    }
    if let Some(location) = tool.get("user_location") {
        validate_user_location(location, &format!("{path}.user_location"))?;
    }
    match tool.get("max_uses") {
        None | Some(Value::Null) => {}
        Some(value) if value.as_i64().is_some_and(|value| value > 0) => {}
        Some(_) => {
            return Err(invalid(
                &format!("{path}.max_uses"),
                "must be a positive integer or null",
            ))
        }
    }
    if let Some(allowed_callers) = tool.get("allowed_callers") {
        validate_string_array(allowed_callers, &format!("{path}.allowed_callers"), false)?;
    }
    validate_optional_string(tool, "response_inclusion", path, true)?;
    validate_optional_bool(tool, "strict", path)?;
    Ok(())
}

fn validate_tools(payload: &Map<String, Value>) -> Result<ToolCatalog, AppError> {
    let Some(tools) = payload.get("tools") else {
        return Ok(HashMap::new());
    };
    if tools.is_null() {
        return Ok(HashMap::new());
    }
    let tools = tools
        .as_array()
        .ok_or_else(|| invalid("tools", "must be an array or null"))?;
    let mut catalog = HashMap::new();
    for (index, tool) in tools.iter().enumerate() {
        let path = format!("tools[{index}]");
        let tool = required_object(tool, &path)?;
        validate_cache_control_field(tool, &path)?;
        validate_optional_string(tool, "description", &path, false)?;
        let defer_loading = match tool.get("defer_loading") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(invalid(
                    &format!("{path}.defer_loading"),
                    "must be a boolean or null",
                ))
            }
        };
        let kind = match tool.get("type") {
            None | Some(Value::Null) => None,
            Some(Value::String(kind)) if !kind.trim().is_empty() => Some(kind.as_str()),
            Some(Value::String(_)) => {
                return Err(invalid(&format!("{path}.type"), "must be non-empty"))
            }
            Some(_) => return Err(invalid(&format!("{path}.type"), "must be a string or null")),
        };
        if kind.is_some_and(|kind| kind.starts_with("web_search")) {
            validate_web_search_tool(tool, &path)?;
        } else if kind.is_none() || tool.get("input_schema").is_some() {
            let name = required_nonempty_string(tool, "name", &path)?;
            let schema = tool.get("input_schema").ok_or_else(|| {
                invalid(
                    &format!("{path}.input_schema"),
                    "field required for a custom tool",
                )
            })?;
            validate_json_schema(schema, &format!("{path}.input_schema"))?;
            if catalog.insert(name.to_string(), defer_loading).is_some() {
                return Err(invalid(
                    &format!("{path}.name"),
                    "tool names must be unique",
                ));
            }
            continue;
        } else {
            validate_optional_string(tool, "name", &path, true)?;
        }
        if let Some(name) = tool.get("name").and_then(Value::as_str) {
            if catalog.insert(name.to_string(), defer_loading).is_some() {
                return Err(invalid(
                    &format!("{path}.name"),
                    "tool names must be unique",
                ));
            }
        }
    }
    Ok(catalog)
}

fn validate_source(block: &Map<String, Value>, path: &str) -> Result<(), AppError> {
    let source_path = format!("{path}.source");
    let source = block
        .get("source")
        .ok_or_else(|| invalid(&source_path, "field required"))?;
    let source = required_object(source, &source_path)?;
    let source_type = match source.get("type") {
        None | Some(Value::Null) => "base64",
        Some(Value::String(source_type)) if !source_type.trim().is_empty() => source_type,
        Some(Value::String(_)) => {
            return Err(invalid(&format!("{source_path}.type"), "must be non-empty"))
        }
        Some(_) => {
            return Err(invalid(
                &format!("{source_path}.type"),
                "must be a string or null",
            ))
        }
    };
    match source_type {
        "base64" => {
            required_nonempty_string(source, "media_type", &source_path)?;
            required_nonempty_string(source, "data", &source_path)?;
        }
        "url" => {
            required_nonempty_string(source, "url", &source_path)?;
        }
        "file" => {
            required_nonempty_string(source, "file_id", &source_path)?;
        }
        _ => {}
    }
    Ok(())
}

fn validate_tool_reference(
    block: &Map<String, Value>,
    path: &str,
    tools: &ToolCatalog,
) -> Result<(), AppError> {
    let name = required_nonempty_string(block, "tool_name", path)?;
    match tools.get(name) {
        Some(true) => {}
        Some(false) => {
            return Err(invalid(
                &format!("{path}.tool_name"),
                "must reference a tool with defer_loading=true",
            ))
        }
        None => {
            return Err(invalid(
                &format!("{path}.tool_name"),
                "must reference a defined deferred tool",
            ))
        }
    }
    validate_cache_control_field(block, path)
}

fn validate_tool_result_content(
    value: Option<&Value>,
    path: &str,
    tools: &ToolCatalog,
) -> Result<(), AppError> {
    let Some(value) = value else {
        return Ok(());
    };
    match value {
        Value::Null | Value::String(_) => Ok(()),
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                let block_path = format!("{path}[{index}]");
                let block = required_object(block, &block_path)?;
                let block_type = required_nonempty_string(block, "type", &block_path)?;
                match block_type {
                    "text" => {
                        if !block.get("text").is_some_and(Value::is_string) {
                            return Err(invalid(
                                &format!("{block_path}.text"),
                                "field required and must be a string",
                            ));
                        }
                        validate_cache_control_field(block, &block_path)?;
                    }
                    "image" | "document" => {
                        validate_source(block, &block_path)?;
                        validate_optional_string(block, "title", &block_path, false)?;
                        validate_cache_control_field(block, &block_path)?;
                    }
                    "tool_reference" => validate_tool_reference(block, &block_path, tools)?,
                    _ => {
                        return Err(invalid(
                            &format!("{block_path}.type"),
                            "unsupported tool_result content block type",
                        ))
                    }
                }
            }
            Ok(())
        }
        _ => Err(invalid(path, "must be a string, array, or null")),
    }
}

fn validate_content_block(
    block: &Value,
    path: &str,
    role: &str,
    tools: &ToolCatalog,
    tool_uses: &mut ToolUseCatalog,
) -> Result<(), AppError> {
    let block = required_object(block, path)?;
    let block_type = required_nonempty_string(block, "type", path)?;
    validate_cache_control_field(block, path)?;
    match block_type {
        "text" => {
            if !block.get("text").is_some_and(Value::is_string) {
                return Err(invalid(
                    &format!("{path}.text"),
                    "field required and must be a string",
                ));
            }
        }
        "image" | "document" => {
            if role != "user" {
                return Err(invalid(
                    &format!("{path}.type"),
                    "image and document blocks require a user message",
                ));
            }
            validate_source(block, path)?;
            validate_optional_string(block, "title", path, false)?;
        }
        "tool_use" => {
            if role != "assistant" {
                return Err(invalid(
                    &format!("{path}.type"),
                    "tool_use blocks require an assistant message",
                ));
            }
            let id = required_nonempty_string(block, "id", path)?;
            let name = required_nonempty_string(block, "name", path)?;
            if !block.get("input").is_some_and(Value::is_object) {
                return Err(invalid(
                    &format!("{path}.input"),
                    "field required and must be an object",
                ));
            }
            if tool_uses.insert(id.to_string(), name.to_string()).is_some() {
                return Err(invalid(
                    &format!("{path}.id"),
                    "tool_use ids must be unique",
                ));
            }
        }
        "tool_result" => {
            if role != "user" {
                return Err(invalid(
                    &format!("{path}.type"),
                    "tool_result blocks require a user message",
                ));
            }
            let tool_use_id = required_nonempty_string(block, "tool_use_id", path)?;
            if !tool_uses.contains_key(tool_use_id) {
                return Err(invalid(
                    &format!("{path}.tool_use_id"),
                    "must reference an earlier tool_use block",
                ));
            }
            match block.get("is_error") {
                None | Some(Value::Null | Value::Bool(_)) => {}
                Some(_) => {
                    return Err(invalid(
                        &format!("{path}.is_error"),
                        "must be a boolean or null",
                    ))
                }
            }
            validate_tool_result_content(block.get("content"), &format!("{path}.content"), tools)?;
        }
        "thinking" => {
            if role != "assistant" {
                return Err(invalid(
                    &format!("{path}.type"),
                    "thinking blocks require an assistant message",
                ));
            }
            if !block.get("thinking").is_some_and(Value::is_string) {
                return Err(invalid(
                    &format!("{path}.thinking"),
                    "field required and must be a string",
                ));
            }
            required_nonempty_string(block, "signature", path)?;
        }
        "tool_reference" => {
            return Err(invalid(
                &format!("{path}.type"),
                "tool_reference is only valid inside tool_result content",
            ))
        }
        _ => {
            // Open content unions retain future block objects for native
            // Messages. A Responses translator that cannot represent one must
            // reject it explicitly rather than dropping it.
        }
    }
    Ok(())
}

fn validate_messages(payload: &Map<String, Value>, tools: &ToolCatalog) -> Result<(), AppError> {
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("messages", "field required and must be an array"))?;
    let mut tool_uses = ToolUseCatalog::new();
    for (message_index, message) in messages.iter().enumerate() {
        let path = format!("messages[{message_index}]");
        let message = required_object(message, &path)?;
        let role = required_nonempty_string(message, "role", &path)?;
        if !matches!(role, "user" | "assistant") {
            return Err(invalid(
                &format!("{path}.role"),
                "must equal \"user\" or \"assistant\"",
            ));
        }
        match message.get("content") {
            Some(Value::String(_)) => {}
            Some(Value::Array(blocks)) => {
                for (block_index, block) in blocks.iter().enumerate() {
                    validate_content_block(
                        block,
                        &format!("{path}.content[{block_index}]"),
                        role,
                        tools,
                        &mut tool_uses,
                    )?;
                }
            }
            _ => {
                return Err(invalid(
                    &format!("{path}.content"),
                    "field required and must be a string or array",
                ))
            }
        }
    }
    Ok(())
}

fn validate_system(payload: &Map<String, Value>) -> Result<(), AppError> {
    let Some(system) = payload.get("system") else {
        return Ok(());
    };
    match system {
        Value::Null | Value::String(_) => Ok(()),
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                let path = format!("system[{index}]");
                let block = required_object(block, &path)?;
                if required_nonempty_string(block, "type", &path)? != "text" {
                    return Err(invalid(&format!("{path}.type"), "must equal \"text\""));
                }
                if !block.get("text").is_some_and(Value::is_string) {
                    return Err(invalid(
                        &format!("{path}.text"),
                        "field required and must be a string",
                    ));
                }
                validate_cache_control_field(block, &path)?;
            }
            Ok(())
        }
        _ => Err(invalid("system", "must be a string, array, or null")),
    }
}

fn validate_tool_choice(payload: &Map<String, Value>) -> Result<(), AppError> {
    let Some(choice) = payload.get("tool_choice") else {
        return Ok(());
    };
    if choice.is_null() {
        return Ok(());
    }
    let choice = required_object(choice, "tool_choice")?;
    let kind = required_nonempty_string(choice, "type", "tool_choice")?;
    if !matches!(kind, "auto" | "any" | "tool" | "none") {
        return Err(invalid(
            "tool_choice.type",
            "must be one of auto, any, tool, or none",
        ));
    }
    if kind == "tool" {
        required_nonempty_string(choice, "name", "tool_choice")?;
    } else {
        validate_optional_string(choice, "name", "tool_choice", true)?;
    }
    Ok(())
}

fn validate_metadata(payload: &Map<String, Value>) -> Result<(), AppError> {
    let Some(metadata) = payload.get("metadata") else {
        return Ok(());
    };
    if metadata.is_null() {
        return Ok(());
    }
    let metadata = required_object(metadata, "metadata")?;
    validate_optional_string(metadata, "user_id", "metadata", false)
}

fn validate_thinking(payload: &Map<String, Value>) -> Result<(), AppError> {
    let Some(thinking) = payload.get("thinking") else {
        return Ok(());
    };
    if thinking.is_null() {
        return Ok(());
    }
    let thinking = required_object(thinking, "thinking")?;
    let kind = required_nonempty_string(thinking, "type", "thinking")?;
    if !matches!(kind, "enabled" | "adaptive") {
        return Err(invalid(
            "thinking.type",
            "must equal \"enabled\" or \"adaptive\"",
        ));
    }
    match thinking.get("budget_tokens") {
        None | Some(Value::Null) => {}
        Some(value) if value.as_i64().is_some_and(|value| value > 0) => {}
        Some(_) => {
            return Err(invalid(
                "thinking.budget_tokens",
                "must be a positive integer or null",
            ))
        }
    }
    validate_optional_string(thinking, "display", "thinking", true)
}

fn validate_output_config(payload: &Map<String, Value>) -> Result<(), AppError> {
    let Some(config) = payload.get("output_config") else {
        return Ok(());
    };
    if config.is_null() {
        return Ok(());
    }
    let config = required_object(config, "output_config")?;
    validate_optional_string(config, "effort", "output_config", true)
}

fn validate_optional_string_list(
    payload: &Map<String, Value>,
    field: &str,
) -> Result<(), AppError> {
    if let Some(value) = payload.get(field) {
        validate_string_array(value, field, true)?;
    }
    Ok(())
}

/// Validate every known collection/object shape consumed by Messages
/// preprocessing or Responses translation. Unknown object keys remain untouched.
#[allow(clippy::result_large_err)]
pub fn validate_messages_request_shape(payload: &Value) -> Result<(), AppError> {
    let payload = required_object(payload, "request")?;
    let tools = validate_tools(payload)?;
    validate_messages(payload, &tools)?;
    validate_system(payload)?;
    validate_tool_choice(payload)?;
    validate_metadata(payload)?;
    validate_thinking(payload)?;
    validate_output_config(payload)?;
    validate_optional_string_list(payload, "stop_sequences")?;
    if let Some(cache_control) = payload.get("cache_control") {
        validate_cache_control(cache_control, "cache_control")?;
    }
    Ok(())
}
