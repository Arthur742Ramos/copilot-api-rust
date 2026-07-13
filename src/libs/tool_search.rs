use std::collections::HashSet;

use indexmap::IndexSet;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

// Mirrors src/lib/tool-search.ts.

pub const BRIDGE_TOOL_SEARCH_NAME: &str = "mcp__tool_search__search";
pub const BRIDGE_TOOL_SEARCH_ALIASES: [&str; 3] = [
    BRIDGE_TOOL_SEARCH_NAME,
    "tool_search_search",
    "mcp__plugin_tool-search_tool_search__search",
];
pub const MCP_TOOL_SEARCH_SENTINEL_TYPE: &str = "copilot_api_tool_search";

pub const ALWAYS_LOADED_TOOL_NAMES: [&str; 25] = [
    "Agent",
    "AskUserQuestion",
    "Bash",
    "Edit",
    "EnterPlanMode",
    "ExitPlanMode",
    "Glob",
    "Grep",
    "Read",
    "Skill",
    "TodoWrite",
    "ToolSearch",
    "WebFetch",
    "Write",
    "apply_patch",
    "bash",
    "glob",
    "grep",
    "plan_exit",
    "question",
    "read",
    "skill",
    "task",
    "todowrite",
    "webfetch",
];

static ALWAYS_LOADED_TOOL_NAME_SET: Lazy<HashSet<&'static str>> =
    Lazy::new(|| ALWAYS_LOADED_TOOL_NAMES.iter().copied().collect());

static BRIDGE_TOOL_SEARCH_NAME_SET: Lazy<HashSet<&'static str>> =
    Lazy::new(|| BRIDGE_TOOL_SEARCH_ALIASES.iter().copied().collect());

static SUPPORTS_MODEL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^gpt-(\d+)(?:\.(\d+))?").expect("valid regex"));

/// Mirrors `McpToolSearchSentinel`. Field order (`type` then `names`) matters
/// for byte-compatible serialization.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolSearchSentinel {
    #[serde(rename = "type")]
    pub r#type: String,
    pub names: Vec<String>,
}

pub fn is_bridge_tool_search_name(name: &str) -> bool {
    BRIDGE_TOOL_SEARCH_NAME_SET.contains(name)
}

pub fn is_always_loaded_tool_name(name: &str) -> bool {
    ALWAYS_LOADED_TOOL_NAME_SET.contains(name)
}

pub fn is_deferred_tool_name(name: &str) -> bool {
    !is_bridge_tool_search_name(name) && !is_always_loaded_tool_name(name)
}

pub fn supports_responses_tool_search_model(model: &str) -> bool {
    let Some(caps) = SUPPORTS_MODEL_RE.captures(model) else {
        return false;
    };

    let major: i64 = match caps.get(1).and_then(|m| m.as_str().parse().ok()) {
        Some(v) => v,
        None => return false,
    };
    let minor: i64 = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);

    major > 5 || (major == 5 && minor >= 4)
}

/// Extracts a `name` string from a loosely-typed tool entry.
fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name").and_then(Value::as_str)
}

fn is_deferred_tool_definition(tool: &Value) -> bool {
    tool_name(tool).is_some_and(is_deferred_tool_name)
        && tool.get("defer_loading") == Some(&Value::Bool(true))
}

pub fn has_bridge_tool_search_tool(tools: Option<&[Value]>) -> bool {
    tools.is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool_name(tool).is_some_and(is_bridge_tool_search_name))
    })
}

pub fn resolve_bridge_tool_search_name(tools: Option<&[Value]>) -> String {
    let Some(tools) = tools else {
        return BRIDGE_TOOL_SEARCH_NAME.to_string();
    };

    tools
        .iter()
        .filter_map(tool_name)
        .find(|name| is_bridge_tool_search_name(name))
        .unwrap_or(BRIDGE_TOOL_SEARCH_NAME)
        .to_string()
}

pub fn has_deferred_tool_candidate(tools: Option<&[Value]>) -> bool {
    tools.is_some_and(|tools| tools.iter().any(is_deferred_tool_definition))
}

pub fn should_enable_responses_tool_search(model: &str, tools: Option<&[Value]>) -> bool {
    supports_responses_tool_search_model(model)
        && has_bridge_tool_search_tool(tools)
        && has_deferred_tool_candidate(tools)
}

pub fn has_deferred_namespace_tool(tools: Option<&[Value]>) -> bool {
    tools.is_some_and(|tools| {
        tools.iter().any(|tool| {
            if !tool.is_object() {
                return false;
            }

            if tool.get("type").and_then(Value::as_str) != Some("namespace") {
                return false;
            }

            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                return false;
            };

            if !is_deferred_tool_name(name) {
                return false;
            }

            tool.get("tools")
                .and_then(Value::as_array)
                .is_some_and(|namespace_tools| {
                    namespace_tools.iter().any(|entry| {
                        entry.is_object() && entry.get("defer_loading") == Some(&Value::Bool(true))
                    })
                })
        })
    })
}

pub fn list_deferred_tool_names(tools: &[Value]) -> Vec<String> {
    let mut set: IndexSet<String> = IndexSet::new();
    for tool in tools {
        if is_deferred_tool_definition(tool) {
            if let Some(name) = tool_name(tool) {
                set.insert(name.to_string());
            }
        }
    }
    set.into_iter().collect()
}

/// Mirrors `extractDeferredToolNamesSource`: record.names ?? record.query ?? record.paths.
fn extract_deferred_tool_names_source(record: &Value) -> Value {
    for key in ["names", "query", "paths"] {
        match record.get(key) {
            Some(v) if !v.is_null() => return v.clone(),
            _ => {}
        }
    }
    Value::Null
}

pub fn parse_deferred_tool_names(names: &Value) -> Vec<String> {
    let mut raw_names: Vec<String> = Vec::new();

    match names {
        Value::String(s) => {
            raw_names.extend(s.split(',').map(|p| p.to_string()));
        }
        Value::Array(arr) => {
            for name in arr {
                if let Value::String(s) = name {
                    raw_names.extend(s.split(',').map(|p| p.to_string()));
                }
            }
        }
        _ => {}
    }

    let mut set: IndexSet<String> = IndexSet::new();
    for name in raw_names {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }
    set.into_iter().collect()
}

pub fn create_mcp_tool_search_sentinel(names: &Value) -> String {
    let sentinel = McpToolSearchSentinel {
        r#type: MCP_TOOL_SEARCH_SENTINEL_TYPE.to_string(),
        names: parse_deferred_tool_names(names),
    };
    serde_json::to_string(&sentinel).unwrap_or_default()
}

pub fn parse_mcp_tool_search_sentinel(text: &str) -> Option<McpToolSearchSentinel> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    if !parsed.is_object() {
        return None;
    }

    if parsed.get("type").and_then(Value::as_str) != Some(MCP_TOOL_SEARCH_SENTINEL_TYPE) {
        return None;
    }

    let names = parse_deferred_tool_names(&extract_deferred_tool_names_source(&parsed));
    if names.is_empty() {
        return None;
    }

    Some(McpToolSearchSentinel {
        r#type: MCP_TOOL_SEARCH_SENTINEL_TYPE.to_string(),
        names,
    })
}

pub fn normalize_tool_search_bridge_arguments(arguments_value: &Value) -> Value {
    if let Value::String(s) = arguments_value {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            if parsed.is_object() {
                let names = parse_deferred_tool_names(&extract_deferred_tool_names_source(&parsed));
                return if names.is_empty() {
                    json!({})
                } else {
                    json!({ "names": names })
                };
            }
        }

        // Treat a raw string as the comma-separated protocol payload.
        let names = parse_deferred_tool_names(arguments_value);
        return if names.is_empty() {
            json!({})
        } else {
            json!({ "names": names })
        };
    }

    let names = parse_deferred_tool_names(&extract_deferred_tool_names_source(arguments_value));
    if names.is_empty() {
        json!({})
    } else {
        json!({ "names": names })
    }
}

pub fn format_tool_search_bridge_arguments(arguments_value: &Value) -> Value {
    let normalized = normalize_tool_search_bridge_arguments(arguments_value);
    let names = normalized.get("names").and_then(Value::as_array);

    match names {
        Some(names) if !names.is_empty() => {
            let joined = names
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",");
            json!({ "names": joined })
        }
        _ => json!({}),
    }
}

pub fn select_deferred_tools_by_names(names: &Value, tools: &[Value]) -> Vec<Value> {
    let requested_names = parse_deferred_tool_names(names);
    if requested_names.is_empty() {
        return Vec::new();
    }

    let mut deferred_tool_by_name: indexmap::IndexMap<String, &Value> = indexmap::IndexMap::new();
    for tool in tools {
        if let Some(name) = tool_name(tool) {
            if is_deferred_tool_name(name) {
                deferred_tool_by_name.insert(name.to_string(), tool);
            }
        }
    }

    requested_names
        .iter()
        .filter_map(|name| deferred_tool_by_name.get(name).map(|t| (*t).clone()))
        .collect()
}

/// Alias for `hasDeferredMcpNamespaceTool` in the TS module.
pub fn has_deferred_mcp_namespace_tool(tools: Option<&[Value]>) -> bool {
    has_deferred_namespace_tool(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_round_trip() {
        let serialized = create_mcp_tool_search_sentinel(&json!("foo,bar"));
        assert_eq!(
            serialized,
            r#"{"type":"copilot_api_tool_search","names":["foo","bar"]}"#
        );

        let parsed = parse_mcp_tool_search_sentinel(&serialized).expect("should parse");
        assert_eq!(parsed.r#type, MCP_TOOL_SEARCH_SENTINEL_TYPE);
        assert_eq!(parsed.names, vec!["foo".to_string(), "bar".to_string()]);
    }

    #[test]
    fn parse_mcp_tool_search_sentinel_rejects_empty() {
        assert!(parse_mcp_tool_search_sentinel(r#"{"type":"copilot_api_tool_search"}"#).is_none());
        assert!(parse_mcp_tool_search_sentinel(r#"{"type":"other","names":["a"]}"#).is_none());
        assert!(parse_mcp_tool_search_sentinel("not json").is_none());
    }

    #[test]
    fn parse_deferred_tool_names_dedup() {
        let names = parse_deferred_tool_names(&json!(" a , b ,a, ,c,b"));
        assert_eq!(
            names,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );

        let from_array = parse_deferred_tool_names(&json!(["a,b", "b", "c", 5]));
        assert_eq!(
            from_array,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn supports_responses_tool_search_model_boundaries() {
        assert!(!supports_responses_tool_search_model("gpt-5.3"));
        assert!(supports_responses_tool_search_model("gpt-5.4"));
        assert!(supports_responses_tool_search_model("gpt-6"));
        assert!(!supports_responses_tool_search_model("gpt-4"));
    }
}
