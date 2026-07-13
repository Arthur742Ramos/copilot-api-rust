//! Web-search backend translation helpers.
//!
//! Ported from `src/routes/messages/web-search/backend.ts`. These are pure
//! transformation functions: build a Responses-API `web_search` tool config from
//! the Anthropic-side config, and parse a Responses `web_search` result back into
//! the answer text / deduped sources / executed queries that the Anthropic
//! `web_search_result` items are assembled from.
//!
//! Crate conventions:
//! - `serde_json` has `preserve_order`, so key insertion order is preserved and
//!   matters; `build_responses_web_search_tool` inserts `type` then `filters`
//!   then `user_location` to match the TS object-literal order exactly.
//! - The tool object and the Responses result items are polymorphic, so the
//!   collectors read `serde_json::Value` the same way the TS code reads loosely
//!   typed objects.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::libs::error::AppError;
use crate::routes::messages::request_validation::merge_open_object_extensions;
use crate::services::copilot::create_responses::ResponsesResult;

/// `WebSearchSource` — a single deduped source extracted from a `url_citation`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebSearchSource {
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_age: Option<String>,
}

/// `WebSearchExtract` — the parsed product of a Responses `web_search` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebSearchExtract {
    /// The grounded answer text produced by the GPT backend (with inline cites).
    pub answer_text: String,
    /// Deduped sources extracted from `url_citation` annotations.
    pub sources: Vec<WebSearchSource>,
    /// Search queries the backend actually ran.
    pub queries: Vec<String>,
    /// Original Responses web-search call id, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

/// `WebSearchToolConfig` — the Anthropic-side configuration that drives the
/// Responses `web_search` tool object. Fields use camelCase on the wire to match
/// the TS interface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebSearchToolConfig {
    #[serde(
        rename = "allowedDomains",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(
        rename = "blockedDomains",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(
        rename = "userLocation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub user_location: Option<Value>,
    #[serde(default, flatten)]
    pub extensions: Map<String, Value>,
}

/// Builds the Responses API `web_search` tool object from the Anthropic config.
///
/// Insertion order (preserved by `preserve_order`): `type`, then `filters`
/// (only when at least one domain filter is present), then `user_location`.
#[allow(clippy::result_large_err)]
pub fn build_responses_web_search_tool(config: &WebSearchToolConfig) -> Result<Value, AppError> {
    let mut tool = Map::new();
    tool.insert("type".to_string(), Value::String("web_search".to_string()));

    let mut filters = Map::new();
    if let Some(allowed) = &config.allowed_domains {
        if !allowed.is_empty() {
            filters.insert(
                "allowed_domains".to_string(),
                Value::Array(allowed.iter().cloned().map(Value::String).collect()),
            );
        }
    }
    if let Some(blocked) = &config.blocked_domains {
        if !blocked.is_empty() {
            filters.insert(
                "blocked_domains".to_string(),
                Value::Array(blocked.iter().cloned().map(Value::String).collect()),
            );
        }
    }
    if !filters.is_empty() {
        tool.insert("filters".to_string(), Value::Object(filters));
    }

    if let Some(user_location) = &config.user_location {
        tool.insert("user_location".to_string(), user_location.clone());
    }

    merge_open_object_extensions(&config.extensions, &[], &mut tool, "web_search tool")?;
    Ok(Value::Object(tool))
}

/// `isValidUrlCitation`: a `url_citation` annotation with a non-empty `url` not
/// already seen.
fn is_valid_url_citation(annotation: &Value, seen_urls: &HashSet<String>) -> bool {
    let is_url_citation = annotation.get("type").and_then(Value::as_str) == Some("url_citation");
    let url = annotation.get("url").and_then(Value::as_str).unwrap_or("");
    is_url_citation && !url.is_empty() && !seen_urls.contains(url)
}

/// `collectTextParts`: gather `output_text` text and deduped `url_citation`
/// sources from a message's content blocks.
fn collect_text_parts(
    blocks: Option<&Vec<Value>>,
    seen_urls: &mut HashSet<String>,
) -> (Vec<String>, Vec<WebSearchSource>) {
    let mut text_parts = Vec::new();
    let mut sources = Vec::new();

    let empty = Vec::new();
    for block in blocks.unwrap_or(&empty) {
        if block.get("type").and_then(Value::as_str) != Some("output_text") {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                text_parts.push(text.to_string());
            }
        }
        if let Some(annotations) = block.get("annotations").and_then(Value::as_array) {
            for annotation in annotations {
                if !is_valid_url_citation(annotation, seen_urls) {
                    continue;
                }
                // url is guaranteed non-empty by is_valid_url_citation.
                let url = annotation
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                seen_urls.insert(url.clone());
                let title = annotation
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| url.clone());
                sources.push(WebSearchSource {
                    url,
                    title,
                    page_age: None,
                });
            }
        }
    }

    (text_parts, sources)
}

/// `collectQuery`: append a `web_search_call` item's executed queries.
///
/// Mirrors the TS: prefer `action.queries` (when non-empty), else fall back to
/// the single `action.query`.
fn collect_query(item: &Value, queries: &mut Vec<String>) {
    let action = item.get("action");
    if let Some(list) = action
        .and_then(|a| a.get("queries"))
        .and_then(Value::as_array)
    {
        if !list.is_empty() {
            for query in list {
                if let Some(query) = query.as_str().filter(|query| !query.is_empty()) {
                    queries.push(query.to_string());
                }
            }
            return;
        }
    }
    if let Some(query) = action
        .and_then(|a| a.get("query"))
        .and_then(Value::as_str)
        .filter(|query| !query.is_empty())
    {
        queries.push(query.to_string());
    }
}

/// Extracts the answer text, deduped sources, and run queries from a GPT
/// `/responses` `web_search` result.
pub fn extract_web_search_result(result: &ResponsesResult) -> WebSearchExtract {
    let mut text_parts: Vec<String> = Vec::new();
    let mut sources: Vec<WebSearchSource> = Vec::new();
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut queries: Vec<String> = Vec::new();
    let mut tool_use_id: Option<String> = None;

    for item in &result.output {
        // Read each output item loosely, matching the TS field access. The
        // typed `ResponseOutputItem` union round-trips losslessly to/from Value.
        let item_val = serde_json::to_value(item).unwrap_or(Value::Null);
        let item_type = item_val.get("type").and_then(Value::as_str);

        if item_type == Some("message") {
            let blocks = item_val.get("content").and_then(Value::as_array);
            let (collected_text, collected_sources) = collect_text_parts(blocks, &mut seen_urls);
            text_parts.extend(collected_text);
            sources.extend(collected_sources);
            continue;
        }
        if item_type == Some("web_search_call") {
            collect_query(&item_val, &mut queries);
            if tool_use_id.is_none() {
                tool_use_id = item_val
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
            }
        }
    }

    let joined = text_parts.join("\n\n");
    let joined_trimmed = joined.trim();
    let answer_text = if !joined_trimmed.is_empty() {
        joined_trimmed.to_string()
    } else {
        match result.output_text.as_deref() {
            Some(output_text) => output_text.trim().to_string(),
            None => String::new(),
        }
    };

    WebSearchExtract {
        answer_text,
        sources,
        queries,
        tool_use_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tool_with_no_filters_or_location() {
        let config = WebSearchToolConfig::default();
        let tool = build_responses_web_search_tool(&config).expect("valid tool");
        assert_eq!(tool, serde_json::json!({ "type": "web_search" }));
    }

    #[test]
    fn build_tool_with_domain_filters_preserves_order() {
        let config = WebSearchToolConfig {
            allowed_domains: Some(vec!["a.com".to_string(), "b.com".to_string()]),
            blocked_domains: Some(vec!["c.com".to_string()]),
            user_location: None,
            extensions: Map::new(),
        };
        let tool = build_responses_web_search_tool(&config).expect("valid tool");
        assert_eq!(
            tool,
            serde_json::json!({
                "type": "web_search",
                "filters": {
                    "allowed_domains": ["a.com", "b.com"],
                    "blocked_domains": ["c.com"]
                }
            })
        );
        // type comes before filters in the serialized output.
        let serialized = serde_json::to_string(&tool).unwrap();
        assert!(serialized.starts_with(r#"{"type":"web_search","filters":"#));
    }

    #[test]
    fn build_tool_empty_domain_arrays_omit_filters() {
        let config = WebSearchToolConfig {
            allowed_domains: Some(vec![]),
            blocked_domains: Some(vec![]),
            user_location: None,
            extensions: Map::new(),
        };
        let tool = build_responses_web_search_tool(&config).expect("valid tool");
        assert_eq!(tool, serde_json::json!({ "type": "web_search" }));
    }

    #[test]
    fn build_tool_with_user_location() {
        let config = WebSearchToolConfig {
            allowed_domains: None,
            blocked_domains: None,
            user_location: Some(serde_json::json!({ "type": "approximate", "country": "US" })),
            extensions: Map::new(),
        };
        let tool = build_responses_web_search_tool(&config).expect("valid tool");
        assert_eq!(
            tool,
            serde_json::json!({
                "type": "web_search",
                "user_location": { "type": "approximate", "country": "US" }
            })
        );
    }

    #[test]
    fn build_tool_with_only_blocked_domains() {
        let config = WebSearchToolConfig {
            allowed_domains: None,
            blocked_domains: Some(vec!["spam.com".to_string()]),
            user_location: None,
            extensions: Map::new(),
        };
        let tool = build_responses_web_search_tool(&config).expect("valid tool");
        assert_eq!(
            tool,
            serde_json::json!({
                "type": "web_search",
                "filters": { "blocked_domains": ["spam.com"] }
            })
        );
    }

    fn result_from_json(value: serde_json::Value) -> ResponsesResult {
        serde_json::from_value(value).expect("parse ResponsesResult")
    }

    #[test]
    fn extract_collects_text_sources_and_queries() {
        let result = result_from_json(serde_json::json!({
            "id": "resp_1",
            "object": "response",
            "created_at": 1,
            "model": "gpt-5",
            "status": "completed",
            "output_text": "",
            "output": [
                {
                    "type": "web_search_call",
                    "action": { "queries": ["rust async", "tokio runtime"] }
                },
                {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "First part.",
                            "annotations": [
                                { "type": "url_citation", "url": "https://x.com", "title": "X" },
                                { "type": "url_citation", "url": "https://x.com", "title": "Dup" },
                                { "type": "url_citation", "url": "https://y.com" }
                            ]
                        },
                        {
                            "type": "output_text",
                            "text": "Second part.",
                            "annotations": []
                        }
                    ]
                }
            ]
        }));

        let extract = extract_web_search_result(&result);
        assert_eq!(extract.answer_text, "First part.\n\nSecond part.");
        assert_eq!(extract.queries, vec!["rust async", "tokio runtime"]);
        assert_eq!(
            extract.sources,
            vec![
                WebSearchSource {
                    url: "https://x.com".to_string(),
                    title: "X".to_string(),
                    page_age: None,
                },
                // y.com has no title -> title falls back to url.
                WebSearchSource {
                    url: "https://y.com".to_string(),
                    title: "https://y.com".to_string(),
                    page_age: None,
                },
            ]
        );
    }

    #[test]
    fn extract_falls_back_to_output_text() {
        let result = result_from_json(serde_json::json!({
            "id": "resp_2",
            "object": "response",
            "created_at": 1,
            "model": "gpt-5",
            "status": "completed",
            "output_text": "  fallback answer  ",
            "output": []
        }));

        let extract = extract_web_search_result(&result);
        assert_eq!(extract.answer_text, "fallback answer");
        assert!(extract.sources.is_empty());
        assert!(extract.queries.is_empty());
    }

    #[test]
    fn extract_single_query_fallback() {
        let result = result_from_json(serde_json::json!({
            "id": "resp_3",
            "object": "response",
            "created_at": 1,
            "model": "gpt-5",
            "status": "completed",
            "output_text": "answer",
            "output": [
                {
                    "type": "web_search_call",
                    "action": { "query": "single query" }
                },
                {
                    "type": "web_search_call",
                    "action": { "queries": [] }
                }
            ]
        }));

        let extract = extract_web_search_result(&result);
        // First item uses action.query; second has an empty queries array with
        // no query, so it contributes nothing.
        assert_eq!(extract.queries, vec!["single query"]);
        assert_eq!(extract.answer_text, "answer");
    }
}
