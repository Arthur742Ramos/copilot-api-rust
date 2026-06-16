//! Helpers for the `/responses` route.
//!
//! Ported in full from routes/responses/utils.ts. The TS module operates on the
//! loosely-typed `ResponsesPayload`; here we work on the typed
//! [`ResponsesPayload`]/[`InputField`]/[`ResponseInputItem`] structs where the
//! shape is known and fall back to `serde_json::Value` for the recursive,
//! shape-agnostic scans (`has_vision_input`, the initiator/role probe).

use serde_json::{json, Value};

use crate::libs::compact::COMPACT_REQUEST;
use crate::libs::config::{
    get_model_responses_api_compact_threshold as configured_model_responses_api_compact_threshold,
    is_responses_api_context_management_enabled as configured_responses_api_context_management_enabled,
    is_responses_api_web_socket_enabled as configured_responses_api_web_socket_enabled,
};
use crate::services::copilot::create_responses::{
    FunctionCallOutputContent, InputField, MessageContent, ResponseInputContent,
    ResponseInputImage, ResponseInputItem, ResponsesPayload, ResponsesTransport,
};
use crate::services::copilot::get_models::Model;

pub const RESPONSES_ENDPOINT: &str = "/responses";
pub const RESPONSES_WS_ENDPOINT: &str = "ws:/responses";
/// Some models (e.g. the Codex catalog) advertise the `/v1`-prefixed form.
pub const RESPONSES_ENDPOINT_V1: &str = "/v1/responses";
pub const DEFAULT_RESPONSES_COMPACT_THRESHOLD_RATIO: f64 = 0.9;

const DATA_URL_PREFIX: &str = "data:";

/// Static 96x32 PNG reading "Image too large / Redacted".
///
/// Copied verbatim from utils.ts (the base64 string is split into the exact
/// same line segments as the source array literal).
const REDACTED_IMAGE_PLACEHOLDER_DATA_URL: &str = concat!(
    "data:image/png;base64,",
    "iVBORw0KGgoAAAANSUhEUgAAAGAAAAAgCAMAAADaHo1mAAADAFBMVEX///8fKTfR1dsAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAACae8QWAAAAvElEQVR42u1WixKAIAhj/f9Hdz2BXJiVed3pVSYtpgwsGSo3GaRq6wSd4F8EyIJx",
    "ydSUAMB8il51sHT2fiVQu8czguQwXWAyFvswIJhmoS9gmzYlcFiHj1aAgzcJVgCyguYhAhNZmMhYQZs1EJnnIAqKiuHjSrZT",
    "ucSQ4s8JkKDDIYr3IuR8vEWgqroKP9b1bYKk2wfgeVmqATQLXdXamsXdEKkz3QXEEeTTuWWImMhW6qci94/+hwSVf99HqVoD",
    "OAuj2SEAAAAASUVORK5CYII=",
);

// ---------------------------------------------------------------------------
// Request options / transport selection
// ---------------------------------------------------------------------------

/// Mirrors `getResponsesRequestOptions`. Returns `(vision, initiator)` where
/// `initiator` is `"agent"` or `"user"`.
pub fn get_responses_request_options(payload: &ResponsesPayload) -> (bool, &'static str) {
    let vision = has_vision_input(payload);
    let initiator = if has_agent_initiator(payload) {
        "agent"
    } else {
        "user"
    };
    (vision, initiator)
}

/// Mirrors `getResponsesTransportForModel`.
pub fn get_responses_transport_for_model(
    selected_model: Option<&Model>,
    compact_type: Option<i32>,
) -> Option<ResponsesTransport> {
    let empty: Vec<String> = Vec::new();
    let supported_endpoints = selected_model
        .and_then(|m| m.supported_endpoints.as_ref())
        .unwrap_or(&empty);
    let use_web_socket = configured_responses_api_web_socket_enabled();

    if compact_type != Some(COMPACT_REQUEST)
        && use_web_socket
        && supported_endpoints
            .iter()
            .any(|e| e == RESPONSES_WS_ENDPOINT)
    {
        return Some(ResponsesTransport::Websocket);
    }

    if supported_endpoints
        .iter()
        .any(|e| e == RESPONSES_ENDPOINT || e == RESPONSES_ENDPOINT_V1)
    {
        return Some(ResponsesTransport::Http);
    }

    None
}

/// Mirrors `hasAgentInitiator`. Inspects only the last input item: a missing or
/// empty `role` flags an agent call, otherwise an `assistant` role does.
pub fn has_agent_initiator(payload: &ResponsesPayload) -> bool {
    let items = payload_items(payload);
    let Some(last) = items.last() else {
        return false;
    };
    let value = serde_json::to_value(last).unwrap_or(Value::Null);
    let role = value.get("role");
    // `!("role" in lastItem) || !lastItem.role` -> agent.
    if !role.map(json_truthy).unwrap_or(false) {
        return true;
    }
    match role {
        Some(Value::String(s)) => s.to_lowercase() == "assistant",
        _ => false,
    }
}

/// Mirrors `hasVisionInput`: any input item that (recursively) contains an
/// `input_image` content block.
pub fn has_vision_input(payload: &ResponsesPayload) -> bool {
    payload_items(payload).iter().any(|item| {
        serde_json::to_value(item)
            .map(|v| contains_vision_content(&v))
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// Image sanitization
// ---------------------------------------------------------------------------

/// Mirrors `sanitizeOversizedInputImages`. Replaces data-URL input images whose
/// estimated decoded size exceeds `max_prompt_image_size`. Returns the count.
pub fn sanitize_oversized_input_images(
    payload: &mut ResponsesPayload,
    max_prompt_image_size: Option<i64>,
) -> usize {
    let limit = match max_prompt_image_size {
        Some(n) if n > 0 => n,
        _ => return 0,
    };
    let Some(InputField::Items(items)) = payload.input.as_mut() else {
        return 0;
    };
    sanitize_input_images(items, |bytes| bytes > limit)
}

/// Mirrors `sanitizeAllInputImages`: replace every data-URL input image. Returns
/// the count.
pub fn sanitize_all_input_images(payload: &mut ResponsesPayload) -> usize {
    let Some(InputField::Items(items)) = payload.input.as_mut() else {
        return 0;
    };
    sanitize_input_images(items, |_| true)
}

fn sanitize_input_images(
    items: &mut [ResponseInputItem],
    should_replace: impl Fn(i64) -> bool,
) -> usize {
    let mut count = 0;
    for item in items.iter_mut() {
        let blocks = match item {
            ResponseInputItem::Message(m) => message_blocks_mut(m.content.as_mut()),
            ResponseInputItem::FunctionCallOutput(o) => output_blocks_mut(&mut o.output),
            _ => None,
        };
        let Some(blocks) = blocks else {
            continue;
        };
        for block in blocks.iter_mut() {
            if let ResponseInputContent::Image(image) = block {
                if let Some(bytes) = input_image_data_url_bytes(image) {
                    if should_replace(bytes) {
                        replace_input_image_with_placeholder(image);
                        count += 1;
                    }
                }
            }
        }
    }
    count
}

fn message_blocks_mut(
    content: Option<&mut MessageContent>,
) -> Option<&mut Vec<ResponseInputContent>> {
    match content {
        Some(MessageContent::Blocks(blocks)) => Some(blocks),
        _ => None,
    }
}

fn output_blocks_mut(
    output: &mut FunctionCallOutputContent,
) -> Option<&mut Vec<ResponseInputContent>> {
    match output {
        FunctionCallOutputContent::Blocks(blocks) => Some(blocks),
        _ => None,
    }
}

/// Mirrors `getInputImageDataUrl` + `estimateDataUrlByteLength`. Returns the
/// estimated decoded byte length when the block is a `data:` URL `input_image`.
fn input_image_data_url_bytes(image: &ResponseInputImage) -> Option<i64> {
    if image.block_type != "input_image" {
        return None;
    }
    let image_url = image.image_url.as_ref()?;
    if !image_url.starts_with(DATA_URL_PREFIX) {
        return None;
    }
    // Math.max(0, Math.floor((value.length * 3) / 4)); data URLs are ASCII so
    // byte length equals JS's UTF-16 code-unit length.
    let len = image_url.len() as i64;
    Some(((len * 3) / 4).max(0))
}

/// Mirrors `replaceInputImageWithPlaceholder`.
fn replace_input_image_with_placeholder(image: &mut ResponseInputImage) {
    image.block_type = "input_image".to_string();
    image.image_url = Some(REDACTED_IMAGE_PLACEHOLDER_DATA_URL.to_string());
    image.detail = "low".to_string();
    image.file_id = None;
}

// ---------------------------------------------------------------------------
// Context management / compaction
// ---------------------------------------------------------------------------

/// Mirrors `resolveResponsesCompactThreshold`.
pub fn resolve_responses_compact_threshold(
    max_prompt_tokens: Option<i64>,
    compact_threshold_ratio: f64,
) -> i64 {
    match max_prompt_tokens {
        Some(n) if n > 0 => ((n as f64) * compact_threshold_ratio).floor() as i64,
        _ => (200_000.0 * compact_threshold_ratio) as i64,
    }
}

/// Mirrors the internal `getModelResponsesApiCompactThreshold`: a validated
/// per-model threshold (finite & > 0) or `None`.
fn model_responses_api_compact_threshold(model: &str) -> Option<f64> {
    let threshold = configured_model_responses_api_compact_threshold(model)?;
    if !threshold.is_finite() || threshold <= 0.0 {
        return None;
    }
    Some(threshold)
}

/// Mirrors `applyResponsesApiContextManagement`: install a default compaction
/// `context_management` entry unless the request already carries one, ends in a
/// terminal compaction trigger, or the feature is disabled.
pub fn apply_responses_api_context_management(
    payload: &mut ResponsesPayload,
    max_prompt_tokens: Option<i64>,
    compact_threshold_ratio: f64,
) {
    if has_terminal_compaction_trigger(payload) {
        return;
    }
    if payload.context_management.is_some() {
        return;
    }
    if !configured_responses_api_context_management_enabled() {
        return;
    }

    let threshold = match model_responses_api_compact_threshold(&payload.model) {
        Some(t) => number_value(t),
        None => Value::from(resolve_responses_compact_threshold(
            max_prompt_tokens,
            compact_threshold_ratio,
        )),
    };

    payload.context_management = Some(vec![json!({
        "type": "compaction",
        "compact_threshold": threshold,
    })]);
}

/// Mirrors `compactInputByLatestCompaction`: drop everything before the latest
/// `compaction` item so only the compacted tail remains.
pub fn compact_input_by_latest_compaction(payload: &mut ResponsesPayload) {
    let Some(InputField::Items(items)) = payload.input.as_mut() else {
        return;
    };
    if items.is_empty() {
        return;
    }
    let Some(index) = latest_compaction_message_index(items) else {
        return;
    };
    // Equivalent to `payload.input = payload.input.slice(index)`.
    items.drain(0..index);
}

fn latest_compaction_message_index(items: &[ResponseInputItem]) -> Option<usize> {
    (0..items.len())
        .rev()
        .find(|&i| is_response_input_item_type(&items[i], "compaction"))
}

fn has_terminal_compaction_trigger(payload: &ResponsesPayload) -> bool {
    let Some(InputField::Items(items)) = payload.input.as_ref() else {
        return false;
    };
    match items.last() {
        Some(last) => is_response_input_item_type(last, "compaction_trigger"),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers / type guards
// ---------------------------------------------------------------------------

/// Mirrors `getPayloadItems`: the input array, or empty when `input` is a bare
/// string / absent.
fn payload_items(payload: &ResponsesPayload) -> &[ResponseInputItem] {
    match payload.input.as_ref() {
        Some(InputField::Items(items)) => items,
        _ => &[],
    }
}

/// Mirrors `isResponseInputItemType(value, type)`.
fn is_response_input_item_type(item: &ResponseInputItem, item_type: &str) -> bool {
    input_item_type_tag(item).as_deref() == Some(item_type)
}

/// Returns the `type` discriminant of an input item (input messages may omit it,
/// defaulting to `"message"`), matching the TS objects' `type` field.
fn input_item_type_tag(item: &ResponseInputItem) -> Option<String> {
    match item {
        ResponseInputItem::Message(m) => {
            Some(m.item_type.clone().unwrap_or_else(|| "message".to_string()))
        }
        ResponseInputItem::FunctionToolCall(i) => Some(i.item_type.clone()),
        ResponseInputItem::FunctionCallOutput(i) => Some(i.item_type.clone()),
        ResponseInputItem::ToolSearchCall(i) => Some(i.item_type.clone()),
        ResponseInputItem::ToolSearchOutput(i) => Some(i.item_type.clone()),
        ResponseInputItem::Reasoning(i) => Some(i.item_type.clone()),
        ResponseInputItem::Compaction(i) => Some(i.item_type.clone()),
        ResponseInputItem::CompactionTrigger(i) => Some(i.item_type.clone()),
        ResponseInputItem::Other(v) => v.get("type").and_then(Value::as_str).map(str::to_owned),
    }
}

/// Mirrors `containsVisionContent`: recursively look for an `input_image` block.
fn contains_vision_content(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_vision_content),
        Value::Object(map) => {
            let type_lower = map
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_lowercase);
            if type_lower.as_deref() == Some("input_image") {
                return true;
            }
            match map.get("content") {
                Some(Value::Array(content)) => content.iter().any(contains_vision_content),
                _ => false,
            }
        }
        // Strings/numbers/bools/null are never vision content (JS treats them as
        // non-objects or falsy).
        _ => false,
    }
}

/// JS truthiness for a JSON value (used for the `role` probe).
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Emit an integer JSON number when the value is integral (matching JS, which
/// renders `217600.0` as `217600`), otherwise a float.
fn number_value(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.007e15 {
        Value::from(n as i64)
    } else {
        json!(n)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_with_input(input: Value) -> ResponsesPayload {
        let mut value = json!({ "model": "gpt-5" });
        value["input"] = input;
        serde_json::from_value(value).expect("payload")
    }

    #[test]
    fn has_vision_input_detects_nested_input_image() {
        let payload = payload_with_input(json!([
            {
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hi" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high" }
                ]
            }
        ]));
        assert!(has_vision_input(&payload));
    }

    #[test]
    fn has_vision_input_false_without_images() {
        let payload = payload_with_input(json!([
            { "role": "user", "content": [ { "type": "input_text", "text": "hi" } ] }
        ]));
        assert!(!has_vision_input(&payload));
    }

    #[test]
    fn has_vision_input_false_for_string_input() {
        let payload = payload_with_input(json!("just a prompt"));
        assert!(!has_vision_input(&payload));
    }

    #[test]
    fn agent_initiator_for_trailing_assistant_and_typeless_item() {
        // assistant role -> agent
        let assistant = payload_with_input(json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "there" }
        ]));
        assert!(has_agent_initiator(&assistant));

        // trailing user role -> not agent
        let user = payload_with_input(json!([
            { "role": "assistant", "content": "there" },
            { "role": "user", "content": "hi" }
        ]));
        assert!(!has_agent_initiator(&user));

        // trailing role-less item (function call) -> agent
        let function = payload_with_input(json!([
            { "role": "user", "content": "hi" },
            { "type": "function_call", "call_id": "c1", "name": "n", "arguments": "{}" }
        ]));
        assert!(has_agent_initiator(&function));
    }

    #[test]
    fn agent_initiator_false_for_empty_input() {
        let payload = payload_with_input(json!([]));
        assert!(!has_agent_initiator(&payload));
    }

    #[test]
    fn compact_threshold_math() {
        assert_eq!(
            resolve_responses_compact_threshold(Some(100_000), 0.9),
            90_000
        );
        assert_eq!(
            resolve_responses_compact_threshold(Some(272_000), 0.9),
            244_800
        );
        // Non-positive / missing falls back to 200_000 * ratio.
        assert_eq!(resolve_responses_compact_threshold(None, 0.9), 180_000);
        assert_eq!(resolve_responses_compact_threshold(Some(0), 0.9), 180_000);
        assert_eq!(
            resolve_responses_compact_threshold(None, DEFAULT_RESPONSES_COMPACT_THRESHOLD_RATIO),
            180_000
        );
        // Floor behaviour.
        assert_eq!(resolve_responses_compact_threshold(Some(101), 0.9), 90);
    }

    #[test]
    fn compact_input_slices_from_latest_compaction() {
        let mut payload = payload_with_input(json!([
            { "role": "user", "content": "a" },
            { "id": "x1", "type": "compaction", "encrypted_content": "e1" },
            { "role": "assistant", "content": "b" },
            { "id": "x2", "type": "compaction", "encrypted_content": "e2" },
            { "role": "user", "content": "c" }
        ]));
        compact_input_by_latest_compaction(&mut payload);
        match payload.input.as_ref() {
            Some(InputField::Items(items)) => {
                // Kept from the latest compaction (index 3) onward: 2 items.
                assert_eq!(items.len(), 2);
                assert_eq!(
                    input_item_type_tag(&items[0]).as_deref(),
                    Some("compaction")
                );
                assert_eq!(input_item_type_tag(&items[1]).as_deref(), Some("message"));
            }
            other => panic!("expected items, got {other:?}"),
        }
    }

    #[test]
    fn compact_input_noop_without_compaction() {
        let mut payload = payload_with_input(json!([
            { "role": "user", "content": "a" },
            { "role": "assistant", "content": "b" }
        ]));
        compact_input_by_latest_compaction(&mut payload);
        match payload.input.as_ref() {
            Some(InputField::Items(items)) => assert_eq!(items.len(), 2),
            other => panic!("expected items, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_all_input_images_replaces_data_urls() {
        let mut payload = payload_with_input(json!([
            {
                "role": "user",
                "content": [
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high" }
                ]
            }
        ]));
        let count = sanitize_all_input_images(&mut payload);
        assert_eq!(count, 1);
        let value = serde_json::to_value(&payload).unwrap();
        let block = &value["input"][0]["content"][0];
        assert_eq!(block["detail"], "low");
        assert_eq!(block["image_url"], REDACTED_IMAGE_PLACEHOLDER_DATA_URL);
    }

    #[test]
    fn sanitize_oversized_only_replaces_large_images() {
        let small = "data:image/png;base64,AAAA"; // tiny
        let mut payload = payload_with_input(json!([
            {
                "role": "user",
                "content": [
                    { "type": "input_image", "image_url": small, "detail": "high" }
                ]
            }
        ]));
        // Limit far above the small image: no replacement.
        assert_eq!(
            sanitize_oversized_input_images(&mut payload, Some(1_000_000)),
            0
        );
        // Limit of zero is treated as "no limit set" -> no replacement.
        assert_eq!(sanitize_oversized_input_images(&mut payload, Some(0)), 0);
        // Tiny limit: replaced.
        assert_eq!(sanitize_oversized_input_images(&mut payload, Some(1)), 1);
    }
}
