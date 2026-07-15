//! Validation for known Anthropic request shapes that are otherwise carried as
//! `serde_json::Value`.
//!
//! The Messages translators intentionally keep open objects so unknown keys can
//! survive. Known fields must still fail closed before admission/provider
//! dispatch instead of being dropped by `as_*`, `filter_map`, or defaults.

#![allow(clippy::result_large_err)]

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::libs::config::resolve_mapped_model;
use crate::libs::error::AppError;
use crate::libs::provider_model::parse_provider_model_alias;
use crate::libs::tool_search::{is_bridge_tool_search_name, supports_responses_tool_search_model};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCatalogKind {
    Function { deferred: bool },
    BridgeSearch,
    Server,
}

type ToolCatalog = HashMap<String, ToolCatalogKind>;
type ToolUseCatalog = HashMap<String, String>;

pub const MAX_REQUEST_SCHEMA_DEPTH: usize = 64;
pub const MAX_REQUEST_SCHEMA_NODES: usize = 4096;
pub const MAX_REQUEST_SCHEMA_COLLECTION_ITEMS: usize = 4096;

const JSON_SCHEMA_TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "string", "integer",
];

pub(crate) const ANTHROPIC_TOOL_KNOWN_FIELDS: &[&str] = &[
    "name",
    "description",
    "input_schema",
    "defer_loading",
    "cache_control",
    "allowed_domains",
    "blocked_domains",
    "user_location",
    "allowed_callers",
    "response_inclusion",
    "max_uses",
    "strict",
    "type",
];

pub(crate) fn merge_open_object_extensions(
    source: &Map<String, Value>,
    known_source_fields: &[&str],
    target: &mut Map<String, Value>,
    path: &str,
) -> Result<(), AppError> {
    for (key, value) in source {
        if known_source_fields.contains(&key.as_str()) {
            continue;
        }
        if target.contains_key(key) {
            return Err(invalid(
                &format!("{path}.{key}"),
                "extension collides with a canonical Responses field",
            ));
        }
        target.insert(key.clone(), value.clone());
    }
    Ok(())
}

pub(crate) fn collect_open_object_extensions(
    source: &Map<String, Value>,
    known_source_fields: &[&str],
    canonical_target_fields: &[&str],
    path: &str,
) -> Result<Map<String, Value>, AppError> {
    let mut extensions = Map::new();
    for (key, value) in source {
        if known_source_fields.contains(&key.as_str()) {
            continue;
        }
        if canonical_target_fields.contains(&key.as_str()) {
            return Err(invalid(
                &format!("{path}.{key}"),
                "extension collides with a canonical Responses field",
            ));
        }
        extensions.insert(key.clone(), value.clone());
    }
    Ok(extensions)
}

fn validate_extension_collisions(
    source: &Map<String, Value>,
    known_source_fields: &[&str],
    canonical_target_fields: &[&str],
    path: &str,
) -> Result<(), AppError> {
    collect_open_object_extensions(source, known_source_fields, canonical_target_fields, path)
        .map(|_| ())
}

fn invalid(path: &str, expectation: &str) -> AppError {
    AppError::BadRequest(format!("{path}: {expectation}"))
}

pub(crate) fn validate_translated_context_management(
    value: Option<&Value>,
    target: &str,
) -> Result<(), AppError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let object = value.as_object().ok_or_else(|| {
        AppError::BadRequest("context_management must be an object when provided".to_string())
    })?;
    if object.keys().any(|key| key != "edits") {
        return Err(AppError::BadRequest(format!(
            "context_management contains fields that cannot be represented by {target}"
        )));
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
                "context_management.edits[{index}] cannot be represented by {target}; only clear_thinking_20251015 with keep=\"all\" is a safe no-op"
            )));
        }
    }
    Ok(())
}

/// Require the model identifier shared by generation and token-count routes.
/// This must run before model mapping, provider resolution, or estimation so an
/// empty identifier cannot silently select a fallback model.
pub fn validate_required_model(payload: &Value) -> Result<(), AppError> {
    match payload.get("model") {
        Some(Value::String(model)) => validate_required_model_id(model),
        _ => Err(invalid(
            "model",
            "field required and must be a non-empty string",
        )),
    }
}

pub fn validate_required_model_id(model: &str) -> Result<(), AppError> {
    if model.trim().is_empty() {
        Err(invalid(
            "model",
            "field required and must be a non-empty string",
        ))
    } else {
        Ok(())
    }
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

#[derive(Default)]
struct SchemaValidationBudget {
    nodes: usize,
}

fn validate_schema_collection_bound(length: usize, path: &str) -> Result<(), AppError> {
    if length > MAX_REQUEST_SCHEMA_COLLECTION_ITEMS {
        return Err(invalid(
            path,
            &format!(
                "contains {length} entries, exceeding the {} entry limit",
                MAX_REQUEST_SCHEMA_COLLECTION_ITEMS
            ),
        ));
    }
    Ok(())
}

fn validate_schema_name_array(value: &Value, path: &str) -> Result<(), AppError> {
    let array = value
        .as_array()
        .ok_or_else(|| invalid(path, "must be an array of property-name strings"))?;
    // JSON Schema's shared `stringArray` meta-schema has `default: []` and
    // `uniqueItems: true`, but no `minItems`. Empty `required`,
    // `dependentRequired`, and legacy dependency arrays are therefore valid
    // no-op constraints; only entries that are present need name validation.
    validate_schema_collection_bound(array.len(), path)?;
    let mut seen = HashSet::new();
    for (index, value) in array.iter().enumerate() {
        let name = value
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                invalid(
                    &format!("{path}[{index}]"),
                    "must be a non-empty property-name string",
                )
            })?;
        if !seen.insert(name) {
            return Err(invalid(
                &format!("{path}[{index}]"),
                "property names must be unique",
            ));
        }
    }
    Ok(())
}

fn validate_schema_array(
    value: &Value,
    path: &str,
    depth: usize,
    budget: &mut SchemaValidationBudget,
) -> Result<(), AppError> {
    let schemas = value
        .as_array()
        .ok_or_else(|| invalid(path, "must be an array of schema nodes"))?;
    validate_schema_collection_bound(schemas.len(), path)?;
    for (index, schema) in schemas.iter().enumerate() {
        validate_json_schema_node(schema, &format!("{path}[{index}]"), depth + 1, budget)?;
    }
    Ok(())
}

fn validate_schema_map(
    value: &Value,
    path: &str,
    depth: usize,
    budget: &mut SchemaValidationBudget,
) -> Result<(), AppError> {
    let schemas = value
        .as_object()
        .ok_or_else(|| invalid(path, "must be an object of schema nodes"))?;
    validate_schema_collection_bound(schemas.len(), path)?;
    for (name, schema) in schemas {
        validate_json_schema_node(schema, &format!("{path}.{name}"), depth + 1, budget)?;
    }
    Ok(())
}

fn validate_json_schema_node(
    value: &Value,
    path: &str,
    depth: usize,
    budget: &mut SchemaValidationBudget,
) -> Result<(), AppError> {
    if depth > MAX_REQUEST_SCHEMA_DEPTH {
        return Err(invalid(
            path,
            &format!(
                "exceeds the maximum schema depth of {}",
                MAX_REQUEST_SCHEMA_DEPTH
            ),
        ));
    }
    budget.nodes = budget.nodes.saturating_add(1);
    if budget.nodes > MAX_REQUEST_SCHEMA_NODES {
        return Err(invalid(
            path,
            &format!(
                "exceeds the maximum schema node count of {}",
                MAX_REQUEST_SCHEMA_NODES
            ),
        ));
    }
    if value.is_boolean() {
        return Ok(());
    }
    let object = value
        .as_object()
        .ok_or_else(|| invalid(path, "schema node must be an object or boolean"))?;
    validate_schema_collection_bound(object.len(), path)?;

    if let Some(schema_type) = object.get("type") {
        match schema_type {
            Value::String(schema_type) if JSON_SCHEMA_TYPES.contains(&schema_type.as_str()) => {}
            Value::Array(types) => {
                validate_schema_collection_bound(types.len(), &format!("{path}.type"))?;
                if types.is_empty() {
                    return Err(invalid(
                        &format!("{path}.type"),
                        "must be a permitted type string or a non-empty unique array of permitted type strings",
                    ));
                }
                let mut seen = HashSet::new();
                for (index, schema_type) in types.iter().enumerate() {
                    let schema_type = schema_type.as_str().ok_or_else(|| {
                        invalid(
                            &format!("{path}.type[{index}]"),
                            "must be a permitted type string",
                        )
                    })?;
                    if !JSON_SCHEMA_TYPES.contains(&schema_type) {
                        return Err(invalid(
                            &format!("{path}.type[{index}]"),
                            "must be one of null, boolean, object, array, number, string, or integer",
                        ));
                    }
                    if !seen.insert(schema_type) {
                        return Err(invalid(
                            &format!("{path}.type[{index}]"),
                            "schema type values must be unique",
                        ));
                    }
                }
            }
            _ => {
                return Err(invalid(
                    &format!("{path}.type"),
                    "must be one of null, boolean, object, array, number, string, or integer, or a non-empty unique array of those values",
                ))
            }
        }
    }

    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(value) = object.get(keyword) {
            validate_schema_map(value, &format!("{path}.{keyword}"), depth, budget)?;
        }
    }

    if let Some(items) = object.get("items") {
        if items.is_array() {
            validate_schema_array(items, &format!("{path}.items"), depth, budget)?;
        } else {
            validate_json_schema_node(items, &format!("{path}.items"), depth + 1, budget)?;
        }
    }
    for keyword in ["prefixItems", "allOf", "anyOf", "oneOf"] {
        if let Some(value) = object.get(keyword) {
            validate_schema_array(value, &format!("{path}.{keyword}"), depth, budget)?;
        }
    }
    for keyword in [
        "additionalProperties",
        "unevaluatedProperties",
        "not",
        "if",
        "then",
        "else",
        "contains",
        "propertyNames",
    ] {
        if let Some(value) = object.get(keyword) {
            validate_json_schema_node(value, &format!("{path}.{keyword}"), depth + 1, budget)?;
        }
    }
    if let Some(required) = object.get("required") {
        validate_schema_name_array(required, &format!("{path}.required"))?;
    }
    if let Some(dependent_required) = object.get("dependentRequired") {
        let dependencies = dependent_required.as_object().ok_or_else(|| {
            invalid(
                &format!("{path}.dependentRequired"),
                "must be an object of string arrays",
            )
        })?;
        validate_schema_collection_bound(dependencies.len(), &format!("{path}.dependentRequired"))?;
        for (name, required) in dependencies {
            validate_schema_name_array(required, &format!("{path}.dependentRequired.{name}"))?;
        }
    }
    if let Some(dependencies) = object.get("dependencies") {
        let dependencies = dependencies.as_object().ok_or_else(|| {
            invalid(
                &format!("{path}.dependencies"),
                "must be an object of schemas or string arrays",
            )
        })?;
        validate_schema_collection_bound(dependencies.len(), &format!("{path}.dependencies"))?;
        for (name, dependency) in dependencies {
            let dependency_path = format!("{path}.dependencies.{name}");
            if dependency.is_array() {
                validate_schema_name_array(dependency, &dependency_path)?;
            } else {
                validate_json_schema_node(dependency, &dependency_path, depth + 1, budget)?;
            }
        }
    }
    if let Some(reference) = object.get("$ref") {
        if !reference.is_string() {
            return Err(invalid(&format!("{path}.$ref"), "must be a string"));
        }
    }
    Ok(())
}

fn validate_json_schema(value: &Value, path: &str) -> Result<(), AppError> {
    validate_json_schema_node(value, path, 0, &mut SchemaValidationBudget::default())
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
        let catalog_entry = if kind.is_some_and(|kind| kind.starts_with("web_search")) {
            validate_web_search_tool(tool, &path)?;
            validate_extension_collisions(
                tool,
                ANTHROPIC_TOOL_KNOWN_FIELDS,
                &["type", "filters", "user_location"],
                &path,
            )?;
            Some((
                required_nonempty_string(tool, "name", &path)?.to_string(),
                ToolCatalogKind::Server,
            ))
        } else if kind.is_none() || tool.get("input_schema").is_some() {
            let name = required_nonempty_string(tool, "name", &path)?;
            let schema = tool.get("input_schema").ok_or_else(|| {
                invalid(
                    &format!("{path}.input_schema"),
                    "field required for a custom tool",
                )
            })?;
            validate_json_schema(schema, &format!("{path}.input_schema"))?;
            let catalog_kind = if is_bridge_tool_search_name(name) {
                ToolCatalogKind::BridgeSearch
            } else {
                ToolCatalogKind::Function {
                    deferred: defer_loading,
                }
            };
            let canonical_fields: &[&str] = match catalog_kind {
                ToolCatalogKind::BridgeSearch => {
                    &["type", "execution", "description", "parameters"]
                }
                ToolCatalogKind::Function { deferred: true } => {
                    &["type", "name", "description", "tools"]
                }
                ToolCatalogKind::Function { deferred: false } => {
                    &["type", "name", "description", "parameters", "strict"]
                }
                ToolCatalogKind::Server => unreachable!(),
            };
            validate_extension_collisions(
                tool,
                ANTHROPIC_TOOL_KNOWN_FIELDS,
                canonical_fields,
                &path,
            )?;
            Some((name.to_string(), catalog_kind))
        } else {
            validate_optional_string(tool, "name", &path, true)?;
            tool.get("name")
                .and_then(Value::as_str)
                .map(|name| (name.to_string(), ToolCatalogKind::Server))
        };
        if let Some((name, kind)) = catalog_entry {
            if catalog.insert(name, kind).is_some() {
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
        "text" => {
            let media_type = required_nonempty_string(source, "media_type", &source_path)?;
            if media_type != "text/plain" {
                return Err(invalid(
                    &format!("{source_path}.media_type"),
                    "text sources require media_type \"text/plain\"",
                ));
            }
            required_nonempty_string(source, "data", &source_path)?;
        }
        "file" => {
            required_nonempty_string(source, "file_id", &source_path)?;
        }
        unsupported => {
            return Err(invalid(
                &format!("{source_path}.type"),
                &format!("unsupported source type \"{unsupported}\""),
            ))
        }
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
        Some(ToolCatalogKind::Function { deferred: true }) => {}
        Some(_) => {
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
    validate_extension_collisions(
        block,
        &["type", "tool_name", "cache_control"],
        &["type", "text"],
        path,
    )?;
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
    bridge_enabled: bool,
) -> Result<(), AppError> {
    let block = required_object(block, path)?;
    let block_type = required_nonempty_string(block, "type", path)?;
    validate_cache_control_field(block, path)?;
    match block_type {
        "text" => {
            validate_extension_collisions(
                block,
                &["type", "text", "cache_control"],
                &["type", "text"],
                path,
            )?;
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
            if block_type == "image" {
                validate_extension_collisions(
                    block,
                    &["type", "source", "cache_control"],
                    &[
                        "type",
                        "image_url",
                        "file_id",
                        "detail",
                        "anthropic_source_extensions",
                    ],
                    path,
                )?;
            } else {
                validate_extension_collisions(
                    block,
                    &["type", "source", "title", "cache_control"],
                    &[
                        "type",
                        "file_data",
                        "file_id",
                        "filename",
                        "anthropic_source_extensions",
                    ],
                    path,
                )?;
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
            let canonical_fields: &[&str] = if bridge_enabled
                && matches!(tools.get(name), Some(ToolCatalogKind::BridgeSearch))
            {
                &["type", "call_id", "arguments", "execution", "status"]
            } else {
                &[
                    "type",
                    "call_id",
                    "name",
                    "arguments",
                    "status",
                    "namespace",
                ]
            };
            validate_extension_collisions(
                block,
                &["type", "id", "name", "input", "cache_control"],
                canonical_fields,
                path,
            )?;
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
            let tool_use_name = tool_uses.get(tool_use_id).ok_or_else(|| {
                invalid(
                    &format!("{path}.tool_use_id"),
                    "must reference an earlier tool_use block",
                )
            })?;
            let canonical_fields: &[&str] = if bridge_enabled
                && matches!(
                    tools.get(tool_use_name),
                    Some(ToolCatalogKind::BridgeSearch)
                ) {
                &[
                    "type",
                    "call_id",
                    "tools",
                    "execution",
                    "status",
                    "anthropic_tool_reference_extensions",
                ]
            } else {
                &["type", "call_id", "output", "status"]
            };
            validate_extension_collisions(
                block,
                &[
                    "type",
                    "tool_use_id",
                    "content",
                    "is_error",
                    "cache_control",
                ],
                canonical_fields,
                path,
            )?;
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
            validate_extension_collisions(
                block,
                &["type", "thinking", "signature", "cache_control"],
                &["type", "id", "summary", "encrypted_content"],
                path,
            )?;
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

fn validate_system_content(value: &Value, path: &str, allow_null: bool) -> Result<(), AppError> {
    match value {
        Value::Null if allow_null => Ok(()),
        Value::String(_) => Ok(()),
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                let path = format!("{path}[{index}]");
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
        _ => Err(invalid(
            path,
            if allow_null {
                "must be a string, array, or null"
            } else {
                "must be a string or array"
            },
        )),
    }
}

fn validate_messages(
    payload: &Map<String, Value>,
    tools: &ToolCatalog,
    bridge_enabled: bool,
) -> Result<(), AppError> {
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("messages", "field required and must be an array"))?;
    let mut tool_uses = ToolUseCatalog::new();
    for (message_index, message) in messages.iter().enumerate() {
        let path = format!("messages[{message_index}]");
        let message = required_object(message, &path)?;
        let role = required_nonempty_string(message, "role", &path)?;
        if !matches!(role, "user" | "assistant" | "system") {
            return Err(invalid(
                &format!("{path}.role"),
                "must equal \"user\", \"assistant\", or \"system\"",
            ));
        }
        let content = message.get("content").ok_or_else(|| {
            invalid(
                &format!("{path}.content"),
                "field required and must be a string or array",
            )
        })?;
        if role == "system" {
            validate_system_content(content, &format!("{path}.content"), false)?;
            continue;
        }
        match content {
            Value::String(_) => {}
            Value::Array(blocks) => {
                for (block_index, block) in blocks.iter().enumerate() {
                    validate_content_block(
                        block,
                        &format!("{path}.content[{block_index}]"),
                        role,
                        tools,
                        &mut tool_uses,
                        bridge_enabled,
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
    validate_system_content(system, "system", true)
}

fn effective_responses_model(payload: &Map<String, Value>) -> Option<String> {
    let model = payload.get("model").and_then(Value::as_str)?;
    let mapped = resolve_mapped_model(model);
    Some(
        parse_provider_model_alias(&mapped)
            .map(|alias| alias.model)
            .unwrap_or(mapped),
    )
}

fn tool_search_bridge_enabled(payload: &Map<String, Value>, tools: &ToolCatalog) -> bool {
    effective_responses_model(payload)
        .as_deref()
        .is_some_and(supports_responses_tool_search_model)
        && tools
            .values()
            .any(|kind| matches!(kind, ToolCatalogKind::BridgeSearch))
        && tools
            .values()
            .any(|kind| matches!(kind, ToolCatalogKind::Function { deferred: true }))
}

fn validate_tool_choice(payload: &Map<String, Value>, tools: &ToolCatalog) -> Result<(), AppError> {
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
    if kind != "tool" {
        if choice.get("name").is_some_and(|name| !name.is_null()) {
            return Err(invalid(
                "tool_choice.name",
                "is only valid when tool_choice.type is \"tool\"",
            ));
        }
        if let Some((key, _)) = choice
            .iter()
            .find(|(key, _)| !matches!(key.as_str(), "type" | "name"))
        {
            return Err(invalid(
                &format!("tool_choice.{key}"),
                "cannot be represented by the scalar Responses tool choice",
            ));
        }
        return Ok(());
    }

    let name = required_nonempty_string(choice, "name", "tool_choice")?;
    match tools.get(name) {
        Some(ToolCatalogKind::Function { deferred: false }) => Ok(()),
        Some(ToolCatalogKind::Function { deferred: true }) => Err(invalid(
            "tool_choice.name",
            "cannot directly select a deferred tool; select the declared tool-search bridge",
        )),
        Some(ToolCatalogKind::Server) => Err(invalid(
            "tool_choice.name",
            "must reference a compatible custom function tool",
        )),
        Some(ToolCatalogKind::BridgeSearch) => {
            if let Some((key, _)) = choice
                .iter()
                .find(|(key, _)| !matches!(key.as_str(), "type" | "name"))
            {
                return Err(invalid(
                    &format!("tool_choice.{key}"),
                    "cannot be represented when the tool-search bridge maps to auto",
                ));
            }
            if tool_search_bridge_enabled(payload, tools) {
                Ok(())
            } else {
                Err(invalid(
                    "tool_choice.name",
                    "tool-search bridge selection requires a supported model and a declared deferred tool",
                ))
            }
        }
        None => Err(invalid(
            "tool_choice.name",
            "must reference exactly one declared compatible tool",
        )),
    }
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
    if !matches!(kind, "enabled" | "adaptive" | "disabled") {
        return Err(invalid(
            "thinking.type",
            "must equal \"enabled\", \"adaptive\", or \"disabled\"",
        ));
    }
    match kind {
        "enabled" => {
            match thinking.get("budget_tokens") {
                Some(value) if value.as_i64().is_some_and(|value| value > 0) => {}
                _ => return Err(invalid(
                    "thinking.budget_tokens",
                    "field required and must be a positive integer when thinking.type is enabled",
                )),
            }
        }
        "adaptive" => {
            if thinking.contains_key("budget_tokens") {
                return Err(invalid(
                    "thinking.budget_tokens",
                    "is not permitted when thinking.type is adaptive",
                ));
            }
        }
        "disabled" => {
            if thinking.contains_key("budget_tokens") {
                return Err(invalid(
                    "thinking.budget_tokens",
                    "is not permitted when thinking.type is disabled",
                ));
            }
            if thinking.contains_key("display") {
                return Err(invalid(
                    "thinking.display",
                    "is not permitted when thinking.type is disabled",
                ));
            }
        }
        _ => unreachable!(),
    }
    validate_optional_string(thinking, "display", "thinking", true)?;
    validate_extension_collisions(
        thinking,
        &["type", "budget_tokens", "display"],
        &["effort", "summary"],
        "thinking",
    )
}

fn validate_output_config(payload: &Map<String, Value>) -> Result<(), AppError> {
    let Some(config) = payload.get("output_config") else {
        return Ok(());
    };
    if config.is_null() {
        return Ok(());
    }
    let config = required_object(config, "output_config")?;
    validate_optional_string(config, "effort", "output_config", true)?;
    validate_extension_collisions(config, &["effort"], &["effort", "summary"], "output_config")
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
    validate_messages(payload, &tools, tool_search_bridge_enabled(payload, &tools))?;
    validate_system(payload)?;
    validate_tool_choice(payload, &tools)?;
    validate_metadata(payload)?;
    validate_thinking(payload)?;
    validate_output_config(payload)?;
    validate_optional_string_list(payload, "stop_sequences")?;
    if let Some(cache_control) = payload.get("cache_control") {
        validate_cache_control(cache_control, "cache_control")?;
    }
    Ok(())
}
