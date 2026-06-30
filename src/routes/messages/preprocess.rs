//! Port of `src/routes/messages/preprocess.ts`.
//!
//! The Anthropic Messages payload is modelled as a `serde_json::Value` walked
//! in place (matching the dominant style in `libs/utils.rs`). The heavy use of
//! JS `{...spread}`, `delete`, `Object.hasOwn` and unknown-key passthrough is
//! reproduced by cloning blocks and inserting/removing keys rather than using a
//! fully typed AST.
//!
//! The string constants (`TOOL_REFERENCE_TURN_BOUNDARY`, `SYSTEM_REMINDER_*`,
//! `IDE_*`, `PDF_FILE_READ_PREFIX`, `CLAUDE_CODE_BILLING_HEADER_PREFIX`) live in
//! `crate::routes::messages::anthropic_types`.

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use crate::libs::compact::{
    COMPACT_AUTO_CONTINUE, COMPACT_AUTO_CONTINUE_PROMPT_STARTS, COMPACT_MESSAGE_SECTIONS,
    COMPACT_REQUEST, COMPACT_SUMMARY_PROMPT_START, COMPACT_SYSTEM_PROMPT_STARTS,
    COMPACT_TEXT_ONLY_GUARD,
};
use crate::libs::config::{
    get_configured_reasoning_effort_for_model, get_reasoning_effort_for_model,
};
use crate::libs::models::normalize_sdk_model_id;
use crate::routes::messages::anthropic_types::{
    CLAUDE_CODE_BILLING_HEADER_PREFIX, IDE_EXECUTE_CODE_TOOL, IDE_GET_DIAGNOSTICS_DESCRIPTION,
    IDE_GET_DIAGNOSTICS_TOOL, PDF_FILE_READ_PREFIX, SUBAGENT_START_HOOK_ADDITIONAL_PREFIX,
    SYSTEM_REMINDER_END, SYSTEM_REMINDER_START, TOOL_REFERENCE_TURN_BOUNDARY,
};
use crate::services::copilot::get_models::Model;

// `/(^|;\s*)cch=[^;]+;/u`
static CLAUDE_CODE_CCH_SEGMENT_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^|;\s*)cch=[^;]+;").unwrap());

// --- small value helpers ----------------------------------------------------

/// JS truthiness for a JSON value (`undefined`/`null`/`false`/`0`/`""`/`NaN`
/// are falsy; everything else truthy).
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn block_type(block: &Value) -> &str {
    block.get("type").and_then(|t| t.as_str()).unwrap_or("")
}

fn create_text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn append_text_segment(base: &str, addition: &str) -> String {
    if base.is_empty() {
        return addition.to_string();
    }
    if addition.is_empty() {
        return base.to_string();
    }
    format!("{base}\n\n{addition}")
}

fn ensure_system_reminder_text(text: &str) -> String {
    if text.starts_with(SYSTEM_REMINDER_START) {
        return text.to_string();
    }
    format!(
        "{}\n{}\n{}",
        SYSTEM_REMINDER_START,
        text.trim(),
        SYSTEM_REMINDER_END
    )
}

/// `normalizeSystemStringForMerge`: returns either a `Value::String` or a
/// `Value::Array` of text blocks (the SubagentStart-hook split path).
fn normalize_system_string_for_merge(text: &str) -> Value {
    if !text.starts_with(SUBAGENT_START_HOOK_ADDITIONAL_PREFIX) {
        return Value::String(ensure_system_reminder_text(text));
    }

    // `/\r?\n/.exec(text)`
    let nl = match text.find('\n') {
        Some(idx) => idx,
        None => return Value::Array(vec![create_text_block(&ensure_system_reminder_text(text))]),
    };
    let (start, match_len) = if nl > 0 && text.as_bytes()[nl - 1] == b'\r' {
        (nl - 1, 2)
    } else {
        (nl, 1)
    };

    let first_line = &text[..start];
    let rest = &text[start + match_len..];

    let mut blocks = vec![create_text_block(&ensure_system_reminder_text(first_line))];
    if !rest.is_empty() {
        blocks.push(create_text_block(&ensure_system_reminder_text(rest)));
    }
    Value::Array(blocks)
}

/// `normalizeSystemContentForMerge`: accepts `string | Array<TextBlock>`.
fn normalize_system_content_for_merge(content: &Value) -> Value {
    if let Some(s) = content.as_str() {
        return normalize_system_string_for_merge(s);
    }
    if let Some(arr) = content.as_array() {
        let mapped: Vec<Value> = arr
            .iter()
            .map(|block| {
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if text.starts_with(SYSTEM_REMINDER_START) {
                    block.clone()
                } else {
                    let mut c = block.clone();
                    if let Some(obj) = c.as_object_mut() {
                        obj.insert(
                            "text".into(),
                            Value::String(ensure_system_reminder_text(text)),
                        );
                    }
                    c
                }
            })
            .collect();
        return Value::Array(mapped);
    }
    content.clone()
}

/// `toSystemTextBlocks`: `string -> [textBlock]`, array -> clone of elements.
fn to_system_text_blocks(content: &Value) -> Vec<Value> {
    if let Some(s) = content.as_str() {
        vec![create_text_block(s)]
    } else if let Some(arr) = content.as_array() {
        arr.clone()
    } else {
        Vec::new()
    }
}

fn merge_system_prompt_content(current: Option<&Value>, addition: &Value) -> Value {
    match current {
        None => addition.clone(),
        Some(cur) => {
            if cur.is_string() && addition.is_string() {
                Value::String(append_text_segment(
                    cur.as_str().unwrap_or(""),
                    addition.as_str().unwrap_or(""),
                ))
            } else {
                let mut v = to_system_text_blocks(cur);
                v.extend(to_system_text_blocks(addition));
                Value::Array(v)
            }
        }
    }
}

fn prepend_system_content_to_user_message(message: &mut Value, addition: &Value) {
    let content = message.get("content").cloned().unwrap_or(Value::Null);

    if content.is_string() && addition.is_string() {
        let merged = append_text_segment(
            addition.as_str().unwrap_or(""),
            content.as_str().unwrap_or(""),
        );
        message["content"] = Value::String(merged);
        return;
    }

    if let Some(arr) = content.as_array() {
        let last_tool_result_index = arr
            .iter()
            .rposition(|block| block_type(block) == "tool_result");
        if let Some(idx) = last_tool_result_index {
            let mut new_content: Vec<Value> = arr[..=idx].to_vec();
            new_content.extend(to_system_text_blocks(addition));
            new_content.extend(arr[idx + 1..].iter().cloned());
            message["content"] = Value::Array(new_content);
            return;
        }
    }

    let mut new_content = to_system_text_blocks(addition);
    if let Some(s) = content.as_str() {
        new_content.push(create_text_block(s));
    } else if let Some(arr) = content.as_array() {
        new_content.extend(arr.iter().cloned());
    }
    message["content"] = Value::Array(new_content);
}

fn normalize_claude_code_billing_header(text: &str) -> String {
    if !text.starts_with(CLAUDE_CODE_BILLING_HEADER_PREFIX) {
        return text.to_string();
    }
    // Non-global regex => first match only.
    CLAUDE_CODE_CCH_SEGMENT_PATTERN
        .replace(text, "${1}cch=<stable>;")
        .into_owned()
}

/// `normalizeClaudeCodeBillingHeaderInSystem`: stabilize the `cch=` segment on a
/// `system` string starting with the billing prefix; for an array `system`,
/// only block index 0 is rewritten.
pub fn normalize_claude_code_billing_header_in_system(payload: &mut Value) {
    let system = match payload.get("system") {
        Some(s) if !s.is_null() => s,
        _ => return,
    };

    if let Some(s) = system.as_str() {
        let normalized = normalize_claude_code_billing_header(s);
        payload["system"] = Value::String(normalized);
        return;
    }

    if let Some(arr) = system.as_array() {
        if arr.is_empty() {
            return;
        }
    } else {
        return;
    }

    // Mutate index 0 in place.
    if let Some(first) = payload
        .get_mut("system")
        .and_then(|s| s.as_array_mut())
        .and_then(|arr| arr.get_mut(0))
    {
        if let Some(text) = first.get("text").and_then(|t| t.as_str()) {
            let normalized = normalize_claude_code_billing_header(text);
            if let Some(obj) = first.as_object_mut() {
                obj.insert("text".into(), Value::String(normalized));
            }
        }
    }
}

/// `normalizeSystemMessages`: merge any `system`-role messages into the previous
/// pushed user message (after the last `tool_result`, or prepended) or into
/// `payload.system` when first; a `system` message following an assistant is
/// silently dropped.
pub fn normalize_system_messages(payload: &mut Value) {
    normalize_claude_code_billing_header_in_system(payload);

    let messages = match payload.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m.clone(),
        None => return,
    };

    let has_system = messages
        .iter()
        .any(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("system"));
    if !has_system {
        return;
    }

    let mut normalized: Vec<Value> = Vec::new();
    // `undefined` system is `None`; preserve an empty-string system as `Some`.
    let mut system: Option<Value> = payload.get("system").filter(|v| !v.is_null()).cloned();

    for message in messages {
        if message.get("role").and_then(|r| r.as_str()) == Some("system") {
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            let normalized_content = normalize_system_content_for_merge(&content);
            let previous_role = normalized
                .last()
                .and_then(|m| m.get("role"))
                .and_then(|r| r.as_str());
            match previous_role {
                Some("user") => {
                    if let Some(prev) = normalized.last_mut() {
                        prepend_system_content_to_user_message(prev, &normalized_content);
                    }
                }
                None => {
                    system = Some(merge_system_prompt_content(
                        system.as_ref(),
                        &normalized_content,
                    ));
                }
                // previous is assistant (or non-user): silently drop.
                _ => {}
            }
            continue;
        }

        normalized.push(message);
    }

    payload["messages"] = Value::Array(normalized);
    match system {
        Some(s) => {
            payload["system"] = s;
        }
        None => {
            if let Some(obj) = payload.as_object_mut() {
                obj.remove("system");
            }
        }
    }
}

// --- version helpers --------------------------------------------------------

fn is_version_at_least(version: &str, minimum_major: i64, minimum_minor: i64) -> bool {
    let mut parts = version.split('.');
    let major_part = parts.next().unwrap_or("");
    let minor_part = parts.next().unwrap_or("0");

    let major: Result<i64, _> = major_part.parse();
    let minor: Result<i64, _> = minor_part.parse();
    let (major, minor) = match (major, minor) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return false,
    };

    major > minimum_major || (major == minimum_major && minor >= minimum_minor)
}

fn should_summarize_thinking_display_for_model(model: &str) -> bool {
    match normalize_sdk_model_id(model) {
        Some(n) => is_version_at_least(&n.version, 4, 7),
        None => false,
    }
}

// --- cache_control getters --------------------------------------------------

/// `getBlockCacheControl`: the block's `cache_control` if it is a truthy object
/// and the block is not a `thinking` block.
fn get_block_cache_control(block: Option<&Value>) -> Option<&Value> {
    let block = block?;
    if block_type(block) == "thinking" {
        return None;
    }
    let cache_control = block.get("cache_control")?;
    // `if (!cacheControl || typeof cacheControl !== "object") return;` — any
    // object (even empty) is a truthy object in JS.
    if cache_control.is_object() {
        Some(cache_control)
    } else {
        None
    }
}

/// `getLastMessageContentCacheControl`: shallow clone of `cache_control` on the
/// last content block (None for missing/thinking/non-object).
pub fn get_last_message_content_cache_control(last_message: &Value) -> Option<Value> {
    let content = last_message.get("content")?.as_array()?;
    get_block_cache_control(content.last()).cloned()
}

/// `applyLastMessageCacheControl`: re-apply the tail marker; default
/// `{type:"ephemeral"}`; don't overwrite an existing marker; skip `thinking`.
pub fn apply_last_message_cache_control(payload: &mut Value, last_cc: Option<&Value>) {
    let default_cc = json!({ "type": "ephemeral" });
    let cache_control = last_cc.unwrap_or(&default_cc).clone();

    let last_block = payload
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .and_then(|arr| arr.last_mut())
        .and_then(|msg| msg.get_mut("content"))
        .and_then(|c| c.as_array_mut())
        .and_then(|arr| arr.last_mut());

    let last_block = match last_block {
        Some(b) => b,
        None => return,
    };

    if block_type(last_block) == "thinking" || truthy(last_block.get("cache_control")) {
        return;
    }

    if let Some(obj) = last_block.as_object_mut() {
        obj.insert("cache_control".into(), cache_control);
    }
}

// --- compact detection ------------------------------------------------------

fn get_compact_candidate_text(message: &Value) -> String {
    if message.get("role").and_then(|r| r.as_str()) != Some("user") {
        return String::new();
    }

    let content = match message.get("content") {
        Some(c) => c,
        None => return String::new(),
    };

    if let Some(s) = content.as_str() {
        return s.to_string();
    }

    let arr = match content.as_array() {
        Some(a) => a,
        None => return String::new(),
    };

    arr.iter()
        .filter(|block| block_type(block) == "text")
        .map(|block| {
            let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if text.starts_with("<system-reminder>") {
                ""
            } else {
                text
            }
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn is_compact_message(last_message: &Value) -> bool {
    let text = get_compact_candidate_text(last_message);
    if text.is_empty() {
        return false;
    }

    text.contains(COMPACT_TEXT_ONLY_GUARD)
        && text.contains(COMPACT_SUMMARY_PROMPT_START)
        && COMPACT_MESSAGE_SECTIONS
            .iter()
            .any(|section| text.contains(section))
}

fn is_compact_auto_continue_message(last_message: &Value) -> bool {
    let text = get_compact_candidate_text(last_message);
    !text.is_empty()
        && COMPACT_AUTO_CONTINUE_PROMPT_STARTS
            .iter()
            .any(|prompt_start| text.starts_with(prompt_start))
}

/// `getCompactType`: `1` COMPACT_REQUEST / `2` COMPACT_AUTO_CONTINUE / `0`.
pub fn get_compact_type(payload: &Value) -> i32 {
    let last_message = payload
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|a| a.last());

    if let Some(lm) = last_message {
        if is_compact_message(lm) {
            return COMPACT_REQUEST;
        }
        if is_compact_auto_continue_message(lm) {
            return COMPACT_AUTO_CONTINUE;
        }
    }

    let system = payload.get("system");
    if let Some(s) = system.and_then(|v| v.as_str()) {
        let has_compact = COMPACT_SYSTEM_PROMPT_STARTS
            .iter()
            .any(|prompt_start| s.starts_with(prompt_start));
        return if has_compact { COMPACT_REQUEST } else { 0 };
    }

    let arr = match system.and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return 0,
    };

    let has_compact = arr.iter().any(|msg| {
        msg.get("text")
            .and_then(|t| t.as_str())
            .map(|text| {
                COMPACT_SYSTEM_PROMPT_STARTS
                    .iter()
                    .any(|prompt_start| text.starts_with(prompt_start))
            })
            .unwrap_or(false)
    });

    if has_compact {
        COMPACT_REQUEST
    } else {
        0
    }
}

// --- tool_result merging ----------------------------------------------------

fn has_tool_ref(block: &Value) -> bool {
    block
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| arr.iter().any(|c| block_type(c) == "tool_reference"))
        .unwrap_or(false)
}

fn is_attachment_block(block: &Value) -> bool {
    let t = block_type(block);
    t == "image" || t == "document"
}

/// `stripContentBlockCacheControl`: drop `cache_control` if the block owns it.
fn strip_content_block_cache_control(block: &Value) -> Value {
    let mut copy = block.clone();
    if let Some(obj) = copy.as_object_mut() {
        if obj.contains_key("cache_control") {
            obj.remove("cache_control");
        }
    }
    copy
}

/// Replace the `content` field of a clone of `tr` with `new_content`.
fn tool_result_with_content(tr: &Value, new_content: Value) -> Value {
    let mut copy = tr.clone();
    if let Some(obj) = copy.as_object_mut() {
        obj.insert("content".into(), new_content);
    }
    copy
}

fn merge_content_with_text(tr: &Value, text_block: &Value) -> Value {
    if let Some(s) = tr.get("content").and_then(|c| c.as_str()) {
        let text = text_block
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        return tool_result_with_content(tr, Value::String(format!("{s}\n\n{text}")));
    }
    if has_tool_ref(tr) {
        return tr.clone();
    }
    let mut new_content = tr
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    new_content.push(strip_content_block_cache_control(text_block));
    tool_result_with_content(tr, Value::Array(new_content))
}

fn merge_content_with_texts(tr: &Value, text_blocks: &[Value]) -> Value {
    if let Some(s) = tr.get("content").and_then(|c| c.as_str()) {
        let appended = text_blocks
            .iter()
            .map(|tb| tb.get("text").and_then(|t| t.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n\n");
        return tool_result_with_content(tr, Value::String(format!("{s}\n\n{appended}")));
    }
    if has_tool_ref(tr) {
        return tr.clone();
    }
    let mut new_content = tr
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    new_content.extend(text_blocks.iter().map(strip_content_block_cache_control));
    tool_result_with_content(tr, Value::Array(new_content))
}

fn merge_content_with_attachments(tr: &Value, attachments: &[Value]) -> Value {
    let clean: Vec<Value> = attachments
        .iter()
        .map(strip_content_block_cache_control)
        .collect();

    if let Some(s) = tr.get("content").and_then(|c| c.as_str()) {
        let mut new_content = vec![json!({ "type": "text", "text": s })];
        new_content.extend(clean);
        return tool_result_with_content(tr, Value::Array(new_content));
    }

    let mut new_content = tr
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    new_content.extend(clean);
    tool_result_with_content(tr, Value::Array(new_content))
}

/// `(order, attachment)` pair (`IndexedAttachment`).
type IndexedAttachment = (usize, Value);

fn get_mergeable_tool_result_indices(tool_results: &[Value]) -> Vec<usize> {
    tool_results
        .iter()
        .enumerate()
        .filter(|(_, block)| !(truthy(block.get("is_error")) || has_tool_ref(block)))
        .map(|(index, _)| index)
        .collect()
}

fn merge_attachments_into_tool_results(
    tool_results: &[Value],
    attachments_by_index: &HashMap<usize, Vec<IndexedAttachment>>,
) -> Vec<Value> {
    if attachments_by_index.is_empty() {
        return tool_results.to_vec();
    }

    tool_results
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let matched = match attachments_by_index.get(&index) {
                Some(m) => m,
                None => return block.clone(),
            };
            let mut ordered = matched.clone();
            ordered.sort_by_key(|(order, _)| *order);
            let ordered_attachments: Vec<Value> = ordered
                .into_iter()
                .map(|(_, attachment)| attachment)
                .collect();
            merge_content_with_attachments(block, &ordered_attachments)
        })
        .collect()
}

fn assign_attachments_to_tool_results(
    target: &mut HashMap<usize, Vec<IndexedAttachment>>,
    attachments: &[IndexedAttachment],
    tool_result_indices: &[usize],
    fallback_tool_result_indices: Option<&[usize]>,
) {
    let fallback_tool_result_indices = fallback_tool_result_indices.unwrap_or(tool_result_indices);

    if attachments.is_empty() {
        return;
    }

    if !tool_result_indices.is_empty() && tool_result_indices.len() == attachments.len() {
        for (index, tool_result_index) in tool_result_indices.iter().enumerate() {
            target
                .entry(*tool_result_index)
                .or_default()
                .push(attachments[index].clone());
        }
        return;
    }

    let last_tool_result_index = match fallback_tool_result_indices.last() {
        Some(idx) => *idx,
        None => return,
    };

    target
        .entry(last_tool_result_index)
        .or_default()
        .extend(attachments.iter().cloned());
}

fn starts_with_pdf_file_read(tool_result: &Value) -> bool {
    let content = match tool_result.get("content") {
        Some(c) => c,
        None => return false,
    };

    if let Some(s) = content.as_str() {
        return s.starts_with(PDF_FILE_READ_PREFIX);
    }

    let arr = match content.as_array() {
        Some(a) => a,
        None => return false,
    };

    if arr.iter().any(|block| block_type(block) == "document") {
        return false;
    }
    if arr.is_empty() {
        return false;
    }

    let first_block = &arr[0];
    if block_type(first_block) != "text" {
        return false;
    }
    first_block
        .get("text")
        .and_then(|t| t.as_str())
        .map(|t| t.starts_with(PDF_FILE_READ_PREFIX))
        .unwrap_or(false)
}

struct MergeableUserContent {
    tool_results: Vec<Value>,
    text_blocks: Vec<Value>,
    attachments: Vec<IndexedAttachment>,
}

/// `collectMergeableUserContent`: returns `None` if any block is not
/// `tool_result`/`text`/`image`/`document`.
fn collect_mergeable_user_content(content: &[Value]) -> Option<MergeableUserContent> {
    let mut tool_results = Vec::new();
    let mut text_blocks = Vec::new();
    let mut attachments: Vec<IndexedAttachment> = Vec::new();

    for (order, block) in content.iter().enumerate() {
        match block_type(block) {
            "tool_result" => tool_results.push(block.clone()),
            "text" => text_blocks.push(block.clone()),
            _ if is_attachment_block(block) => attachments.push((order, block.clone())),
            _ => return None,
        }
    }

    Some(MergeableUserContent {
        tool_results,
        text_blocks,
        attachments,
    })
}

fn merge_tool_result(tool_results: &[Value], text_blocks: &[Value]) -> Vec<Value> {
    if tool_results.len() == text_blocks.len() {
        return tool_results
            .iter()
            .enumerate()
            .map(|(i, tr)| merge_content_with_text(tr, &text_blocks[i]))
            .collect();
    }

    let last_index = tool_results.len().wrapping_sub(1);
    tool_results
        .iter()
        .enumerate()
        .map(|(i, tr)| {
            if i == last_index {
                merge_content_with_texts(tr, text_blocks)
            } else {
                tr.clone()
            }
        })
        .collect()
}

fn merge_attachments_for_tool_results(
    tool_results: &[Value],
    attachments: &[IndexedAttachment],
) -> Vec<Value> {
    if attachments.is_empty() {
        return tool_results.to_vec();
    }

    let document_blocks: Vec<IndexedAttachment> = attachments
        .iter()
        .filter(|(_, attachment)| block_type(attachment) == "document")
        .cloned()
        .collect();
    let mergeable_tool_result_indices = get_mergeable_tool_result_indices(tool_results);
    let pdf_read_tool_result_indices: Vec<usize> = mergeable_tool_result_indices
        .iter()
        .copied()
        .filter(|&index| starts_with_pdf_file_read(&tool_results[index]))
        .collect();

    let mut attachments_by_tool_result_index: HashMap<usize, Vec<IndexedAttachment>> =
        HashMap::new();
    let mut remaining_attachments: Vec<IndexedAttachment> = attachments.to_vec();
    let mut count_match_tool_result_indices = mergeable_tool_result_indices.clone();

    // Match PDF read tool results and documents in order first.
    if !document_blocks.is_empty() && !pdf_read_tool_result_indices.is_empty() {
        let matched_document_count = pdf_read_tool_result_indices
            .len()
            .min(document_blocks.len());
        let matched_documents = &document_blocks[..matched_document_count];
        let matched_document_orders: std::collections::HashSet<usize> =
            matched_documents.iter().map(|(order, _)| *order).collect();
        let matched_pdf_tool_result_indices =
            &pdf_read_tool_result_indices[..matched_document_count];
        let matched_pdf_tool_result_index_set: std::collections::HashSet<usize> =
            matched_pdf_tool_result_indices.iter().copied().collect();

        assign_attachments_to_tool_results(
            &mut attachments_by_tool_result_index,
            matched_documents,
            matched_pdf_tool_result_indices,
            None,
        );
        count_match_tool_result_indices = mergeable_tool_result_indices
            .iter()
            .copied()
            .filter(|index| !matched_pdf_tool_result_index_set.contains(index))
            .collect();
        remaining_attachments = attachments
            .iter()
            .filter(|(order, attachment)| {
                block_type(attachment) != "document" || !matched_document_orders.contains(order)
            })
            .cloned()
            .collect();
    }

    // Everything else keeps the count-match / last-tool-result fallback.
    assign_attachments_to_tool_results(
        &mut attachments_by_tool_result_index,
        &remaining_attachments,
        &count_match_tool_result_indices,
        Some(&mergeable_tool_result_indices),
    );

    merge_attachments_into_tool_results(tool_results, &attachments_by_tool_result_index)
}

fn merge_user_message_content(content: &[Value]) -> Option<Vec<Value>> {
    let mergeable = collect_mergeable_user_content(content)?;

    if mergeable.tool_results.is_empty()
        || (mergeable.text_blocks.is_empty() && mergeable.attachments.is_empty())
    {
        return None;
    }

    let merged_tool_results = if mergeable.text_blocks.is_empty() {
        mergeable.tool_results.clone()
    } else {
        merge_tool_result(&mergeable.tool_results, &mergeable.text_blocks)
    };

    Some(merge_attachments_for_tool_results(
        &merged_tool_results,
        &mergeable.attachments,
    ))
}

/// `stripToolReferenceTurnBoundary`: drop text blocks equal to `"Tool loaded."`
/// from user messages that contain a `tool_result` with a `tool_reference`.
pub fn strip_tool_reference_turn_boundary(payload: &mut Value) {
    let messages = match payload.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return,
    };

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let content = match msg.get("content").and_then(|c| c.as_array()) {
            Some(c) => c,
            None => continue,
        };

        let has_tool_reference = content
            .iter()
            .any(|block| block_type(block) == "tool_result" && has_tool_ref(block));
        if !has_tool_reference {
            continue;
        }

        let filtered: Vec<Value> = content
            .iter()
            .filter(|block| {
                block_type(block) != "text"
                    || block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(|t| t.trim() != TOOL_REFERENCE_TURN_BOUNDARY)
                        .unwrap_or(true)
            })
            .cloned()
            .collect();
        msg["content"] = Value::Array(filtered);
    }
}

/// `mergeToolResultForClaude`: coalesce text/attachment blocks into adjacent
/// `tool_result` blocks.
pub fn merge_tool_result_for_claude(payload: &mut Value, skip_last_message: bool) {
    let messages = match payload.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return,
    };

    let last_message_index = messages.len().wrapping_sub(1);

    for (index, msg) in messages.iter_mut().enumerate() {
        if skip_last_message && index == last_message_index {
            continue;
        }
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let content = match msg.get("content").and_then(|c| c.as_array()) {
            Some(c) => c.clone(),
            None => continue,
        };

        if let Some(merged) = merge_user_message_content(&content) {
            msg["content"] = Value::Array(merged);
        }
    }
}

/// `sanitizeIdeTools`: drop `mcp__ide__executeCode` unless `defer_loading`;
/// replace `mcp__ide__getDiagnostics` description with the canonical one.
pub fn sanitize_ide_tools(payload: &mut Value) {
    let tools = match payload.get("tools").and_then(|t| t.as_array()) {
        Some(t) if !t.is_empty() => t.clone(),
        _ => return,
    };

    let mut new_tools: Vec<Value> = Vec::new();
    for tool in tools {
        let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if name == IDE_EXECUTE_CODE_TOOL && !truthy(tool.get("defer_loading")) {
            continue;
        }
        if name == IDE_GET_DIAGNOSTICS_TOOL {
            let mut copy = tool.clone();
            if let Some(obj) = copy.as_object_mut() {
                obj.insert(
                    "description".into(),
                    Value::String(IDE_GET_DIAGNOSTICS_DESCRIPTION.to_string()),
                );
            }
            new_tools.push(copy);
            continue;
        }
        new_tools.push(tool);
    }

    payload["tools"] = Value::Array(new_tools);
}

// --- prepare_messages_api_payload helpers -----------------------------------

/// `stripCacheControl`: remove only the `scope` key from system block
/// `cache_control`.
fn strip_cache_control(payload: &mut Value) {
    let blocks = match payload.get_mut("system").and_then(|s| s.as_array_mut()) {
        Some(b) => b,
        None => return,
    };

    for block in blocks.iter_mut() {
        if let Some(cache_control) = block.get_mut("cache_control") {
            if cache_control.is_object() {
                if let Some(obj) = cache_control.as_object_mut() {
                    obj.remove("scope");
                }
            }
        }
    }
}

/// `applyTopLevelCacheControl`: polyfill the top-level `cache_control` onto the
/// last cacheable block, then delete the top-level field.
fn apply_top_level_cache_control(payload: &mut Value) {
    let top_level = payload.get("cache_control");
    let top_level = match top_level {
        None => return, // undefined: nothing to do.
        Some(v) => v,
    };

    if !v_is_object(top_level) {
        // present but null/non-object: delete and return.
        if let Some(obj) = payload.as_object_mut() {
            obj.remove("cache_control");
        }
        return;
    }

    let top_level = top_level.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("cache_control");
    }

    let messages = match payload.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return,
    };

    for m in (0..messages.len()).rev() {
        let message = &mut messages[m];

        if let Some(s) = message.get("content").and_then(|c| c.as_str()) {
            let text = s.to_string();
            message["content"] = json!([
                { "type": "text", "text": text, "cache_control": top_level.clone() }
            ]);
            return;
        }

        let content = match message.get_mut("content").and_then(|c| c.as_array_mut()) {
            Some(c) => c,
            None => continue,
        };

        for b in (0..content.len()).rev() {
            let block = &mut content[b];
            let t = block_type(block);
            if t != "text" && t != "image" && t != "tool_use" && t != "tool_result" {
                continue;
            }
            // `??=`: set only when absent/null.
            if !truthy_strict_present(block.get("cache_control")) {
                if let Some(obj) = block.as_object_mut() {
                    obj.insert("cache_control".into(), top_level.clone());
                }
            }
            return;
        }
    }
}

fn v_is_object(v: &Value) -> bool {
    v.is_object()
}

/// `??=` only sets when the current value is `undefined`/`null`. So we treat a
/// present non-null value as "keep".
fn truthy_strict_present(v: Option<&Value>) -> bool {
    matches!(v, Some(x) if !x.is_null())
}

/// `stripToolEagerInputStreaming`: delete `eager_input_streaming` from each tool.
fn strip_tool_eager_input_streaming(payload: &mut Value) {
    let tools = match payload.get_mut("tools").and_then(|t| t.as_array_mut()) {
        Some(t) if !t.is_empty() => t,
        _ => return,
    };

    for tool in tools.iter_mut() {
        if let Some(obj) = tool.as_object_mut() {
            if obj.contains_key("eager_input_streaming") {
                obj.remove("eager_input_streaming");
            }
        }
    }
}

/// `filterAssistantThinkingBlocks`: keep `thinking` only if `thinking` truthy
/// AND `!= "Thinking..."` AND `signature` truthy AND `!signature.includes("@")`.
fn filter_assistant_thinking_blocks(payload: &mut Value) {
    let messages = match payload.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return,
    };

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let content = match msg.get("content").and_then(|c| c.as_array()) {
            Some(c) => c.clone(),
            None => continue,
        };

        let filtered: Vec<Value> = content
            .into_iter()
            .filter(|block| {
                if block_type(block) != "thinking" {
                    return true;
                }
                let thinking_ok = block
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .map(|t| !t.is_empty() && t != "Thinking...")
                    .unwrap_or(false);
                let signature_ok = block
                    .get("signature")
                    .and_then(|s| s.as_str())
                    .map(|s| !s.is_empty() && !s.contains('@'))
                    .unwrap_or(false);
                thinking_ok && signature_ok
            })
            .collect();
        msg["content"] = Value::Array(filtered);
    }
}

/// `prepareMessagesApiPayload`: cache-control normalization, eager-input
/// stripping, thinking-block filtering, then adaptive-thinking / reasoning
/// effort resolution.
pub fn prepare_messages_api_payload(payload: &mut Value, selected_model: Option<&Model>) {
    strip_cache_control(payload);
    apply_top_level_cache_control(payload);
    strip_tool_eager_input_streaming(payload);
    filter_assistant_thinking_blocks(payload);

    let has_thinking = truthy(payload.get("thinking"));

    let tool_choice_type = payload
        .get("tool_choice")
        .and_then(|tc| tc.get("type"))
        .and_then(|t| t.as_str());
    let disable_think = tool_choice_type == Some("any") || tool_choice_type == Some("tool");

    let adaptive_thinking = selected_model
        .map(|m| m.capabilities.supports.adaptive_thinking == Some(true))
        .unwrap_or(false);

    if !adaptive_thinking || disable_think {
        return;
    }
    let selected_model = selected_model.unwrap();

    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let mut thinking = json!({ "type": "adaptive" });
    // align with vscode copilot
    if !has_thinking {
        thinking["display"] = json!("summarized");
    }
    if should_summarize_thinking_display_for_model(&model) {
        thinking["display"] = json!("summarized");
    }
    payload["thinking"] = thinking;

    // Effort precedence: an explicit `modelReasoningEfforts` entry in config wins
    // over the client-supplied `output_config.effort`, which in turn wins over the
    // built-in default. This lets an operator force e.g. `max` for a model even
    // when the client hardcodes a lower effort (the Copilot CLI always sends
    // `effort: "high"`). Models without a configured override still honor the
    // client's choice.
    let client_effort = payload
        .get("output_config")
        .and_then(|oc| oc.get("effort"))
        .and_then(|e| e.as_str())
        .map(|s| s.to_string());
    let mut effort = get_configured_reasoning_effort_for_model(&model)
        .or(client_effort)
        .unwrap_or_else(|| get_reasoning_effort_for_model(&model));

    if effort == "none" || effort == "minimal" {
        effort = "low".to_string();
    }

    let mut effort_undefined = false;
    if let Some(reasoning_effort) = selected_model
        .capabilities
        .supports
        .reasoning_effort
        .as_ref()
    {
        if !reasoning_effort.contains(&effort) {
            match reasoning_effort.last() {
                Some(last) => effort = last.clone(),
                None => effort_undefined = true,
            }
        }
    }

    if effort_undefined {
        tracing::info!(
            target: "audit",
            model = %model,
            effort = "none",
            api = "messages",
            "resolved reasoning effort (output_config cleared; model declares no supported efforts)"
        );
        payload["output_config"] = json!({});
    } else {
        tracing::info!(
            target: "audit",
            model = %model,
            effort = %effort,
            api = "messages",
            "resolved reasoning effort"
        );
        payload["output_config"] = json!({ "effort": effort });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::compact::{
        COMPACT_AUTO_CONTINUE_CLAUDE_CODE_PROMPT_START, COMPACT_SUMMARY_PROMPT_START,
        COMPACT_TEXT_ONLY_GUARD,
    };
    use serde_json::json;

    fn model_with_adaptive(reasoning: Option<Vec<&str>>) -> Model {
        let mut m = Model::default();
        m.capabilities.supports.adaptive_thinking = Some(true);
        m.capabilities.supports.reasoning_effort =
            reasoning.map(|r| r.into_iter().map(|s| s.to_string()).collect());
        m
    }

    #[test]
    fn get_compact_type_matrix() {
        // COMPACT_REQUEST: last user message contains the guard + summary +
        // a message section.
        let text = format!(
            "{}\n{}\nPending Tasks: do things",
            COMPACT_TEXT_ONLY_GUARD, COMPACT_SUMMARY_PROMPT_START
        );
        let payload = json!({
            "messages": [
                { "role": "user", "content": text }
            ]
        });
        assert_eq!(get_compact_type(&payload), COMPACT_REQUEST);

        // COMPACT_AUTO_CONTINUE: last user message starts with an auto-continue
        // prompt.
        let payload = json!({
            "messages": [
                { "role": "user", "content": format!("{} more", COMPACT_AUTO_CONTINUE_CLAUDE_CODE_PROMPT_START) }
            ]
        });
        assert_eq!(get_compact_type(&payload), COMPACT_AUTO_CONTINUE);

        // None: ordinary message.
        let payload = json!({
            "messages": [
                { "role": "user", "content": "hello world" }
            ]
        });
        assert_eq!(get_compact_type(&payload), 0);
    }

    #[test]
    fn normalize_system_messages_silent_drop_after_assistant() {
        let payload_template = json!({
            "messages": [
                { "role": "assistant", "content": "prior answer" },
                { "role": "system", "content": "secret system note" }
            ]
        });
        let mut payload = payload_template.clone();
        normalize_system_messages(&mut payload);

        let messages = payload.get("messages").unwrap().as_array().unwrap();
        // The system message following the assistant is silently dropped.
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].get("role").and_then(|r| r.as_str()),
            Some("assistant")
        );
        // System content was NOT merged anywhere.
        assert!(payload.get("system").is_none());
    }

    #[test]
    fn normalize_system_messages_merge_into_user() {
        let mut payload = json!({
            "messages": [
                { "role": "user", "content": "user question" },
                { "role": "system", "content": "extra instructions" }
            ]
        });
        normalize_system_messages(&mut payload);

        let messages = payload.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 1);
        // Both the user content and the normalized system content are strings,
        // so the system reminder is prepended into a single merged string.
        let content = messages[0].get("content").unwrap().as_str().unwrap();
        assert!(content.starts_with("<system-reminder>"));
        assert!(content.contains("extra instructions"));
        assert!(content.contains("user question"));
    }

    #[test]
    fn merge_tool_result_for_claude_basic_text_merge() {
        let mut payload = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "t1", "content": "result body" },
                        { "type": "text", "text": "extra note" }
                    ]
                }
            ]
        });
        merge_tool_result_for_claude(&mut payload, false);

        let content = payload["messages"][0]["content"].as_array().unwrap();
        // Text block coalesced into the tool_result; only the tool_result remains.
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"].as_str(), Some("tool_result"));
        assert_eq!(
            content[0]["content"].as_str(),
            Some("result body\n\nextra note")
        );
    }

    #[test]
    fn prepare_messages_api_payload_effort_replacement() {
        let model = model_with_adaptive(Some(vec!["low", "medium", "high"]));
        let mut payload = json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                { "role": "user", "content": "hi" }
            ],
            "output_config": { "effort": "none" }
        });
        prepare_messages_api_payload(&mut payload, Some(&model));

        // "none" is normalized to "low" (which is in the model's list).
        assert_eq!(payload["output_config"]["effort"].as_str(), Some("low"));
        // adaptive thinking applied.
        assert_eq!(payload["thinking"]["type"].as_str(), Some("adaptive"));
    }

    #[test]
    fn prepare_messages_api_payload_effort_clamped_to_last() {
        // effort "high" is not in the model's restricted list -> clamp to last.
        let model = model_with_adaptive(Some(vec!["low", "medium"]));
        let mut payload = json!({
            "model": "claude-sonnet-4-5",
            "messages": [{ "role": "user", "content": "hi" }],
            "output_config": { "effort": "high" }
        });
        prepare_messages_api_payload(&mut payload, Some(&model));
        assert_eq!(payload["output_config"]["effort"].as_str(), Some("medium"));
    }

    #[test]
    #[serial_test::serial]
    fn prepare_messages_api_payload_config_effort_overrides_client() {
        use crate::libs::config::{
            get_config, reset_cached_config_for_test, set_cached_config_for_test,
        };
        use std::collections::BTreeMap;

        // Force `max` for this model via operator config, preserving all other
        // config fields so the process-global cache stays well-formed.
        let mut cfg = (*get_config()).clone();
        let mut efforts = BTreeMap::new();
        efforts.insert("claude-opus-4.8".to_string(), "max".to_string());
        cfg.model_reasoning_efforts = Some(efforts);
        set_cached_config_for_test(cfg);

        let model = model_with_adaptive(Some(vec!["low", "medium", "high", "xhigh", "max"]));
        let mut payload = json!({
            "model": "claude-opus-4.8",
            "messages": [{ "role": "user", "content": "hi" }],
            // Client hardcodes a lower effort, as the Copilot CLI does.
            "output_config": { "effort": "high" }
        });
        prepare_messages_api_payload(&mut payload, Some(&model));

        // The configured `max` overrides the client-supplied `high`.
        assert_eq!(payload["output_config"]["effort"].as_str(), Some("max"));

        reset_cached_config_for_test();
    }
}
