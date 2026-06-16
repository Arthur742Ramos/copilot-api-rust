//! Stream ID Synchronization for `@ai-sdk/openai` compatibility.
//!
//! Problem: GitHub Copilot's Responses API returns different IDs for the same
//! item in `added` vs `done` events. This breaks `@ai-sdk/openai` which expects
//! consistent IDs across the stream lifecycle.
//!
//! Errors without this fix:
//! - "activeReasoningPart.summaryParts" undefined
//! - "text part not found"
//!
//! Use case: OpenCode (AI coding assistant) using Codex models (gpt-5.2-codex)
//! via `@ai-sdk/openai` provider requires the Responses API endpoint.
//!
//! Ported from `src/routes/responses/stream-id-sync.ts`.

use std::collections::HashMap;

use rand::Rng;
use serde_json::Value;

/// Tracks the synchronized id assigned to each output item, keyed by
/// `output_index`, so that `added`/`done`/streaming events all agree.
pub struct StreamIdTracker {
    output_items: HashMap<i64, String>,
}

impl StreamIdTracker {
    pub fn new() -> Self {
        Self {
            output_items: HashMap::new(),
        }
    }
}

impl Default for StreamIdTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Mirror the TS `Math.random().toString(36).slice(2)` loop that builds at least
/// 16 base36 characters and then truncates to exactly 16.
fn random_base36_suffix() -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::thread_rng();
    let mut suffix = String::with_capacity(16);
    for _ in 0..16 {
        let idx = rng.gen_range(0..36) as usize;
        suffix.push(ALPHABET[idx] as char);
    }
    suffix
}

/// Re-stringify after either passing the original data through unchanged on a
/// serialization failure, or returning the modified value.
fn stringify(parsed: &Value, fallback: &str) -> String {
    serde_json::to_string(parsed).unwrap_or_else(|_| fallback.to_string())
}

/// Synchronize the ids inside a single Responses stream event payload.
///
/// `data` is the raw JSON string from the SSE `data:` line, `event` is the SSE
/// `event:` name. If `data` is empty or not valid JSON (e.g. `[DONE]`), it is
/// returned unchanged.
pub fn fix_stream_ids(data: &str, event: Option<&str>, tracker: &mut StreamIdTracker) -> String {
    if data.is_empty() {
        return data.to_string();
    }
    let mut parsed: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(_) => return data.to_string(),
    };

    match event {
        Some("response.output_item.added") => handle_output_item_added(&mut parsed, tracker, data),
        Some("response.output_item.done") => handle_output_item_done(&mut parsed, tracker, data),
        _ => handle_item_id(&mut parsed, tracker, data),
    }
}

fn output_index_of(parsed: &Value) -> Option<i64> {
    parsed.get("output_index").and_then(Value::as_i64)
}

fn handle_output_item_added(
    parsed: &mut Value,
    tracker: &mut StreamIdTracker,
    fallback: &str,
) -> String {
    let output_index = output_index_of(parsed);

    // Determine current item.id, generating one when missing/empty.
    let needs_id = parsed
        .get("item")
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .map(str::is_empty)
        .unwrap_or(true);

    let id = if needs_id {
        let suffix = random_base36_suffix();
        let index = output_index.unwrap_or(0);
        let generated = format!("oi_{index}_{suffix}");
        if let Some(item) = parsed.get_mut("item") {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("id".to_string(), Value::String(generated.clone()));
            }
        }
        Some(generated)
    } else {
        parsed
            .get("item")
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    if let (Some(index), Some(id)) = (output_index, id) {
        tracker.output_items.insert(index, id);
    }

    stringify(parsed, fallback)
}

fn handle_output_item_done(
    parsed: &mut Value,
    tracker: &mut StreamIdTracker,
    fallback: &str,
) -> String {
    if let Some(index) = output_index_of(parsed) {
        if let Some(original_id) = tracker.output_items.get(&index).cloned() {
            if let Some(item) = parsed.get_mut("item") {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("id".to_string(), Value::String(original_id));
                }
            }
        }
    }
    stringify(parsed, fallback)
}

fn handle_item_id(parsed: &mut Value, tracker: &mut StreamIdTracker, fallback: &str) -> String {
    if let Some(index) = output_index_of(parsed) {
        if let Some(item_id) = tracker.output_items.get(&index).cloned() {
            if let Some(obj) = parsed.as_object_mut() {
                obj.insert("item_id".to_string(), Value::String(item_id));
            }
        }
    }
    stringify(parsed, fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_json_data_passes_through() {
        let mut tracker = StreamIdTracker::new();
        assert_eq!(fix_stream_ids("[DONE]", None, &mut tracker), "[DONE]");
        assert_eq!(fix_stream_ids("", None, &mut tracker), "");
    }

    #[test]
    fn added_generates_id_when_missing() {
        let mut tracker = StreamIdTracker::new();
        let data = r#"{"type":"response.output_item.added","output_index":0,"item":{"id":""}}"#;
        let out = fix_stream_ids(data, Some("response.output_item.added"), &mut tracker);
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let id = parsed["item"]["id"].as_str().unwrap();
        assert!(id.starts_with("oi_0_"));
        assert_eq!(id.len(), "oi_0_".len() + 16);
    }

    #[test]
    fn added_to_done_stabilizes_id() {
        let mut tracker = StreamIdTracker::new();

        let added_data =
            r#"{"type":"response.output_item.added","output_index":2,"item":{"id":""}}"#;
        let added_out =
            fix_stream_ids(added_data, Some("response.output_item.added"), &mut tracker);
        let added_parsed: Value = serde_json::from_str(&added_out).unwrap();
        let generated_id = added_parsed["item"]["id"].as_str().unwrap().to_string();
        assert!(generated_id.starts_with("oi_2_"));

        // The done event arrives with a *different* upstream id.
        let done_data = r#"{"type":"response.output_item.done","output_index":2,"item":{"id":"upstream-different-id"}}"#;
        let done_out = fix_stream_ids(done_data, Some("response.output_item.done"), &mut tracker);
        let done_parsed: Value = serde_json::from_str(&done_out).unwrap();
        assert_eq!(done_parsed["item"]["id"].as_str().unwrap(), generated_id);
    }

    #[test]
    fn added_keeps_existing_id() {
        let mut tracker = StreamIdTracker::new();
        let data =
            r#"{"type":"response.output_item.added","output_index":1,"item":{"id":"existing"}}"#;
        let out = fix_stream_ids(data, Some("response.output_item.added"), &mut tracker);
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["item"]["id"].as_str().unwrap(), "existing");

        // Subsequent streaming event for the same index gets item_id injected.
        let delta = r#"{"type":"response.output_text.delta","output_index":1,"delta":"hi"}"#;
        let delta_out = fix_stream_ids(delta, Some("response.output_text.delta"), &mut tracker);
        let delta_parsed: Value = serde_json::from_str(&delta_out).unwrap();
        assert_eq!(delta_parsed["item_id"].as_str().unwrap(), "existing");
    }
}
