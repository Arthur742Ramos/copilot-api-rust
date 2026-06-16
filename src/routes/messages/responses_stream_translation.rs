//! Responses API streaming events -> Anthropic SSE events.
//!
//! Ported from `src/routes/messages/responses-stream-translation.ts`.
//!
//! These are pure transformation functions: a Responses-API stream event (taken
//! as a loosely-typed `serde_json::Value`, robust to the polymorphic event/item
//! shapes) is folded into a mutable `ResponsesStreamState` and produces zero or
//! more `AnthropicStreamEventData` outputs, in push order.
//!
//! Conventions:
//! - `open_blocks` is insertion-ordered (a `Vec<i64>`) because the order in
//!   which `content_block_stop` events are emitted matters and mirrors the JS
//!   `Set` iteration order.
//! - Helper functions and constants owned by the (Phase-2 sibling)
//!   `responses_translation` module are re-declared here so this file compiles
//!   standalone against Phase-1 foundations.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::libs::tool_search::{format_tool_search_bridge_arguments, BRIDGE_TOOL_SEARCH_NAME};

use super::anthropic_types::{
    AnthropicContentBlockDelta, AnthropicErrorBody, AnthropicMessageDeltaBody,
    AnthropicMessageDeltaUsage, AnthropicMessageStart, AnthropicStreamEventData, AnthropicUsage,
};

// ---------------------------------------------------------------------------
// Constants (local copies — owned upstream by `responses_translation.ts`)
// ---------------------------------------------------------------------------

/// `responses-stream-translation.ts`:
/// `const MAX_CONSECUTIVE_FUNCTION_CALL_WHITESPACE = 20`
const MAX_CONSECUTIVE_FUNCTION_CALL_WHITESPACE: i64 = 20;

/// `responses-translation.ts`: `export const THINKING_TEXT = "Thinking..."`
const THINKING_TEXT: &str = "Thinking...";

/// `responses-translation.ts`: `const COMPACTION_SIGNATURE_PREFIX = "cm1#"`
const COMPACTION_SIGNATURE_PREFIX: &str = "cm1#";

/// `responses-translation.ts`: `const COMPACTION_SIGNATURE_SEPARATOR = "@"`
const COMPACTION_SIGNATURE_SEPARATOR: &str = "@";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Per-output-index tracking for an in-flight function/tool call block.
/// Mirrors the TS `FunctionCallStreamState`.
#[derive(Debug, Clone, Default)]
pub struct FunctionCallStreamState {
    pub block_index: i64,
    pub tool_call_id: String,
    pub name: String,
    pub consecutive_whitespace_count: i64,
}

/// Mirrors the TS `ResponsesStreamState`.
#[derive(Debug, Clone)]
pub struct ResponsesStreamState {
    pub message_start_sent: bool,
    pub message_completed: bool,
    pub next_content_block_index: i64,
    pub block_index_by_key: HashMap<String, i64>,
    /// Insertion-ordered (JS `Set` iteration order is significant here).
    pub open_blocks: Vec<i64>,
    pub block_has_delta: HashSet<i64>,
    pub function_call_state_by_output_index: HashMap<i64, FunctionCallStreamState>,
    pub tool_search_name: String,
    pub has_tool_call: bool,
}

impl ResponsesStreamState {
    /// Mirrors the TS `createResponsesStreamState({ toolSearchName })`.
    pub fn new(tool_search_name: Option<String>) -> Self {
        Self {
            message_start_sent: false,
            message_completed: false,
            next_content_block_index: 0,
            block_index_by_key: HashMap::new(),
            open_blocks: Vec::new(),
            block_has_delta: HashSet::new(),
            function_call_state_by_output_index: HashMap::new(),
            tool_search_name: tool_search_name
                .unwrap_or_else(|| BRIDGE_TOOL_SEARCH_NAME.to_string()),
            has_tool_call: false,
        }
    }
}

/// Free-function form mirroring the TS factory.
pub fn create_responses_stream_state(tool_search_name: Option<String>) -> ResponsesStreamState {
    ResponsesStreamState::new(tool_search_name)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Mirrors the TS `translateResponsesStreamEvent`. Events are accepted as
/// `serde_json::Value` to be robust to the dynamic event/item shapes.
pub fn translate_responses_stream_event(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    match event.get("type").and_then(Value::as_str) {
        Some("response.created") => handle_response_created(event, state),
        Some("response.output_item.added") => handle_output_item_added(event, state),
        Some("response.reasoning_summary_text.delta") => {
            handle_reasoning_summary_text_delta(event, state)
        }
        Some("response.output_text.delta") => handle_output_text_delta(event, state),
        Some("response.reasoning_summary_text.done") => {
            handle_reasoning_summary_text_done(event, state)
        }
        Some("response.output_text.done") => handle_output_text_done(event, state),
        Some("response.output_item.done") => handle_output_item_done(event, state),
        Some("response.function_call_arguments.delta") => {
            handle_function_call_arguments_delta(event, state)
        }
        Some("response.function_call_arguments.done") => {
            handle_function_call_arguments_done(event, state)
        }
        Some("response.completed") | Some("response.incomplete") => {
            handle_response_completed(event, state)
        }
        Some("response.failed") => handle_response_failed(event, state),
        Some("error") => handle_error_event(event, state),
        _ => Vec::new(),
    }
}

/// Mirrors the TS `buildErrorEvent`.
pub fn build_error_event(message: &str) -> AnthropicStreamEventData {
    AnthropicStreamEventData::Error {
        error: AnthropicErrorBody {
            kind: "api_error".to_string(),
            message: message.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// Small accessors
// ---------------------------------------------------------------------------

fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn get_i64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn block_key(output_index: i64, content_index: i64) -> String {
    format!("{output_index}:{content_index}")
}

/// Mirrors the TS `resolveToolUseName`: a non-empty `namespace` wins, else `name`.
fn resolve_tool_use_name(item: &Value) -> String {
    if let Some(ns) = item.get("namespace").and_then(Value::as_str) {
        if !ns.is_empty() {
            return ns.to_string();
        }
    }
    item.get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Mirrors the TS `encodeCompactionCarrierSignature`.
fn encode_compaction_carrier_signature(id: &str, encrypted_content: &str) -> String {
    format!(
        "{COMPACTION_SIGNATURE_PREFIX}{encrypted_content}{COMPACTION_SIGNATURE_SEPARATOR}{id}"
    )
}

// ---------------------------------------------------------------------------
// Whitespace guard
// ---------------------------------------------------------------------------

/// Mirrors the TS `updateWhitespaceRunState`. `\r`, `\n`, `\t` increment the run
/// (and trip once it exceeds the cap); a plain space leaves the run unchanged;
/// any other character resets it.
fn update_whitespace_run_state(previous_count: i64, chunk: &str) -> (i64, bool) {
    let mut count = previous_count;

    for ch in chunk.chars() {
        if ch == '\r' || ch == '\n' || ch == '\t' {
            count += 1;
            if count > MAX_CONSECUTIVE_FUNCTION_CALL_WHITESPACE {
                return (count, true);
            }
            continue;
        }

        if ch != ' ' {
            count = 0;
        }
    }

    (count, false)
}

// ---------------------------------------------------------------------------
// Block bookkeeping
// ---------------------------------------------------------------------------

fn open_blocks_has(state: &ResponsesStreamState, block_index: i64) -> bool {
    state.open_blocks.contains(&block_index)
}

/// Mirrors `closeOpenBlocks`: emit `content_block_stop` for every open block in
/// insertion order, dropping their `blockHasDelta` flags.
fn close_open_blocks(state: &mut ResponsesStreamState, events: &mut Vec<AnthropicStreamEventData>) {
    let to_close: Vec<i64> = state.open_blocks.clone();
    for block_index in to_close {
        events.push(AnthropicStreamEventData::ContentBlockStop { index: block_index });
        state.block_has_delta.remove(&block_index);
    }
    state.open_blocks.clear();
}

/// Mirrors `closeAllOpenBlocks`.
fn close_all_open_blocks(
    state: &mut ResponsesStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    close_open_blocks(state, events);
    state.function_call_state_by_output_index.clear();
}

/// Mirrors `openTextBlockIfNeeded`.
fn open_text_block_if_needed(
    state: &mut ResponsesStreamState,
    output_index: i64,
    content_index: i64,
    events: &mut Vec<AnthropicStreamEventData>,
) -> i64 {
    let key = block_key(output_index, content_index);
    let block_index = match state.block_index_by_key.get(&key) {
        Some(idx) => *idx,
        None => {
            let idx = state.next_content_block_index;
            state.next_content_block_index += 1;
            state.block_index_by_key.insert(key, idx);
            idx
        }
    };

    if !open_blocks_has(state, block_index) {
        close_open_blocks(state, events);
        events.push(AnthropicStreamEventData::ContentBlockStart {
            index: block_index,
            content_block: json!({ "type": "text", "text": "" }),
        });
        state.open_blocks.push(block_index);
    }

    block_index
}

/// Mirrors `openThinkingBlockIfNeeded` (all summary_index values fold into one
/// block by always using summary_index 0 for the key).
fn open_thinking_block_if_needed(
    state: &mut ResponsesStreamState,
    output_index: i64,
    events: &mut Vec<AnthropicStreamEventData>,
) -> i64 {
    let summary_index = 0;
    let key = block_key(output_index, summary_index);
    let block_index = match state.block_index_by_key.get(&key) {
        Some(idx) => *idx,
        None => {
            let idx = state.next_content_block_index;
            state.next_content_block_index += 1;
            state.block_index_by_key.insert(key, idx);
            idx
        }
    };

    if !open_blocks_has(state, block_index) {
        close_open_blocks(state, events);
        events.push(AnthropicStreamEventData::ContentBlockStart {
            index: block_index,
            content_block: json!({ "type": "thinking", "thinking": "" }),
        });
        state.open_blocks.push(block_index);
    }

    block_index
}

/// Mirrors `openFunctionCallBlock`.
fn open_function_call_block(
    state: &mut ResponsesStreamState,
    output_index: i64,
    tool_call_id: Option<&str>,
    name: Option<&str>,
    events: &mut Vec<AnthropicStreamEventData>,
) -> i64 {
    state.has_tool_call = true;

    if !state
        .function_call_state_by_output_index
        .contains_key(&output_index)
    {
        let block_index = state.next_content_block_index;
        state.next_content_block_index += 1;

        let resolved_tool_call_id = tool_call_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("tool_call_{block_index}"));
        let resolved_name = name.map(str::to_string).unwrap_or_else(|| "function".to_string());

        state.function_call_state_by_output_index.insert(
            output_index,
            FunctionCallStreamState {
                block_index,
                tool_call_id: resolved_tool_call_id,
                name: resolved_name,
                consecutive_whitespace_count: 0,
            },
        );
    }

    let fc = state
        .function_call_state_by_output_index
        .get(&output_index)
        .expect("function call state just inserted");
    let block_index = fc.block_index;
    let tool_call_id = fc.tool_call_id.clone();
    let name = fc.name.clone();

    if !open_blocks_has(state, block_index) {
        close_open_blocks(state, events);
        events.push(AnthropicStreamEventData::ContentBlockStart {
            index: block_index,
            content_block: json!({
                "type": "tool_use",
                "id": tool_call_id,
                "name": name,
                "input": {},
            }),
        });
        state.open_blocks.push(block_index);
    }

    block_index
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn handle_response_created(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let empty = Value::Null;
    let response = event.get("response").unwrap_or(&empty);
    message_start(state, response)
}

/// Mirrors `messageStart`.
fn message_start(
    state: &mut ResponsesStreamState,
    response: &Value,
) -> Vec<AnthropicStreamEventData> {
    state.message_start_sent = true;

    let usage = response.get("usage");
    let input_cached_tokens = usage
        .and_then(|u| u.get("input_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64);
    let input_tokens_raw = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let input_tokens = input_tokens_raw - input_cached_tokens.unwrap_or(0);

    let id = get_str(response, "id").unwrap_or("").to_string();
    let model = get_str(response, "model").unwrap_or("").to_string();

    vec![AnthropicStreamEventData::MessageStart {
        message: AnthropicMessageStart {
            id,
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: Vec::new(),
            model,
            stop_reason: None,
            stop_sequence: None,
            usage: AnthropicUsage {
                input_tokens,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(input_cached_tokens.unwrap_or(0)),
                service_tier: None,
            },
        },
    }]
}

fn handle_output_item_added(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();

    let details = match extract_function_call_details(event, state) {
        Some(d) => d,
        None => return events,
    };

    let block_index = open_function_call_block(
        state,
        details.output_index,
        Some(&details.tool_call_id),
        Some(&details.name),
        &mut events,
    );

    if let Some(initial) = details.initial_arguments {
        if !initial.is_empty() {
            events.push(AnthropicStreamEventData::ContentBlockDelta {
                index: block_index,
                delta: AnthropicContentBlockDelta::InputJsonDelta {
                    partial_json: initial,
                },
            });
            state.block_has_delta.insert(block_index);
        }
    }

    events
}

struct FunctionCallDetails {
    output_index: i64,
    tool_call_id: String,
    name: String,
    initial_arguments: Option<String>,
}

/// Mirrors `extractFunctionCallDetails`.
fn extract_function_call_details(
    event: &Value,
    state: &ResponsesStreamState,
) -> Option<FunctionCallDetails> {
    let item = event.get("item")?;
    let item_type = item.get("type").and_then(Value::as_str)?;
    let output_index = get_i64(event, "output_index");

    if item_type == "tool_search_call" {
        return Some(FunctionCallDetails {
            output_index,
            tool_call_id: get_str(item, "call_id").unwrap_or("").to_string(),
            name: state.tool_search_name.clone(),
            initial_arguments: Some(String::new()),
        });
    }

    if item_type != "function_call" {
        return None;
    }

    Some(FunctionCallDetails {
        output_index,
        tool_call_id: get_str(item, "call_id").unwrap_or("").to_string(),
        name: resolve_tool_use_name(item),
        initial_arguments: Some(get_str(item, "arguments").unwrap_or("").to_string()),
    })
}

fn handle_output_item_done(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let empty = Value::Null;
    let item = event.get("item").unwrap_or(&empty);
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    let output_index = get_i64(event, "output_index");

    if item_type == "tool_search_call" {
        let call_id = get_str(item, "call_id").unwrap_or("").to_string();
        let block_index = open_function_call_block(
            state,
            output_index,
            Some(&call_id),
            Some(&state.tool_search_name.clone()),
            &mut events,
        );

        let final_arguments = stringify_tool_search_arguments(
            item.get("arguments").unwrap_or(&Value::Null),
        );

        if !state.block_has_delta.contains(&block_index) {
            if let Some(args) = final_arguments {
                if !args.is_empty() {
                    events.push(AnthropicStreamEventData::ContentBlockDelta {
                        index: block_index,
                        delta: AnthropicContentBlockDelta::InputJsonDelta { partial_json: args },
                    });
                    state.block_has_delta.insert(block_index);
                }
            }
        }

        state.function_call_state_by_output_index.remove(&output_index);
        return events;
    }

    if item_type == "compaction" {
        let id = get_str(item, "id").unwrap_or("");
        let encrypted_content = get_str(item, "encrypted_content").unwrap_or("");
        if id.is_empty() || encrypted_content.is_empty() {
            return events;
        }

        let block_index = open_thinking_block_if_needed(state, output_index, &mut events);

        if !state.block_has_delta.contains(&block_index) {
            events.push(AnthropicStreamEventData::ContentBlockDelta {
                index: block_index,
                delta: AnthropicContentBlockDelta::ThinkingDelta {
                    thinking: THINKING_TEXT.to_string(),
                },
            });
        }

        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: block_index,
            delta: AnthropicContentBlockDelta::SignatureDelta {
                signature: encode_compaction_carrier_signature(id, encrypted_content),
            },
        });
        state.block_has_delta.insert(block_index);
        return events;
    }

    if item_type != "reasoning" {
        return events;
    }

    let block_index = open_thinking_block_if_needed(state, output_index, &mut events);
    let encrypted_content = get_str(item, "encrypted_content").unwrap_or("");
    let id = get_str(item, "id").unwrap_or("");
    let signature = format!("{encrypted_content}@{id}");

    // signature is always a non-empty string (it contains the "@" separator).
    let summary_empty = item
        .get("summary")
        .and_then(Value::as_array)
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if summary_empty {
        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: block_index,
            delta: AnthropicContentBlockDelta::ThinkingDelta {
                thinking: THINKING_TEXT.to_string(),
            },
        });
    }

    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::SignatureDelta { signature },
    });
    state.block_has_delta.insert(block_index);

    events
}

fn handle_function_call_arguments_delta(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = get_i64(event, "output_index");
    let delta_text = get_str(event, "delta").unwrap_or("");

    if delta_text.is_empty() {
        return events;
    }

    let block_index = open_function_call_block(state, output_index, None, None, &mut events);

    let fc = match state.function_call_state_by_output_index.get(&output_index) {
        Some(fc) => fc,
        None => {
            return handle_function_call_arguments_validation_error(
                "Received function call arguments delta without an open tool call block.",
                state,
                events,
            );
        }
    };

    // fix: copilot function calls occasionally emit an unbounded whitespace run
    // (infinite line breaks until max_tokens). Trip the guard and abort.
    let (next_count, exceeded) =
        update_whitespace_run_state(fc.consecutive_whitespace_count, delta_text);
    if exceeded {
        return handle_function_call_arguments_validation_error(
            "Received function call arguments delta containing more than 20 consecutive whitespace characters.",
            state,
            events,
        );
    }

    if let Some(fc) = state.function_call_state_by_output_index.get_mut(&output_index) {
        fc.consecutive_whitespace_count = next_count;
    }

    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::InputJsonDelta {
            partial_json: delta_text.to_string(),
        },
    });
    state.block_has_delta.insert(block_index);

    events
}

fn handle_function_call_arguments_done(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = get_i64(event, "output_index");
    let block_index = open_function_call_block(state, output_index, None, None, &mut events);

    let final_arguments = get_str(event, "arguments").filter(|s| !s.is_empty());

    if !state.block_has_delta.contains(&block_index) {
        if let Some(args) = final_arguments {
            events.push(AnthropicStreamEventData::ContentBlockDelta {
                index: block_index,
                delta: AnthropicContentBlockDelta::InputJsonDelta {
                    partial_json: args.to_string(),
                },
            });
            state.block_has_delta.insert(block_index);
        }
    }

    state.function_call_state_by_output_index.remove(&output_index);
    events
}

fn handle_output_text_delta(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = get_i64(event, "output_index");
    let content_index = get_i64(event, "content_index");
    let delta_text = get_str(event, "delta").unwrap_or("");

    if delta_text.is_empty() {
        return events;
    }

    let block_index =
        open_text_block_if_needed(state, output_index, content_index, &mut events);

    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::TextDelta {
            text: delta_text.to_string(),
        },
    });
    state.block_has_delta.insert(block_index);

    events
}

fn handle_reasoning_summary_text_delta(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = get_i64(event, "output_index");
    let delta_text = get_str(event, "delta").unwrap_or("").to_string();
    let block_index = open_thinking_block_if_needed(state, output_index, &mut events);

    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::ThinkingDelta {
            thinking: delta_text,
        },
    });
    state.block_has_delta.insert(block_index);

    events
}

fn handle_reasoning_summary_text_done(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = get_i64(event, "output_index");
    let text = get_str(event, "text").unwrap_or("");
    let block_index = open_thinking_block_if_needed(state, output_index, &mut events);

    if !text.is_empty() && !state.block_has_delta.contains(&block_index) {
        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: block_index,
            delta: AnthropicContentBlockDelta::ThinkingDelta {
                thinking: text.to_string(),
            },
        });
    }

    events
}

fn handle_output_text_done(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = get_i64(event, "output_index");
    let content_index = get_i64(event, "content_index");
    let text = get_str(event, "text").unwrap_or("");

    let block_index =
        open_text_block_if_needed(state, output_index, content_index, &mut events);

    if !text.is_empty() && !state.block_has_delta.contains(&block_index) {
        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: block_index,
            delta: AnthropicContentBlockDelta::TextDelta {
                text: text.to_string(),
            },
        });
    }

    events
}

fn handle_response_completed(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let empty = Value::Null;
    let response = event.get("response").unwrap_or(&empty);
    let mut events = Vec::new();

    close_all_open_blocks(state, &mut events);

    let stop_reason = map_responses_stop_reason(response, state.has_tool_call);
    let usage = map_responses_usage_delta(response);

    events.push(AnthropicStreamEventData::MessageDelta {
        delta: AnthropicMessageDeltaBody {
            stop_reason,
            stop_sequence: None,
        },
        usage: Some(usage),
    });
    events.push(AnthropicStreamEventData::MessageStop);
    state.message_completed = true;

    events
}

fn handle_response_failed(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let empty = Value::Null;
    let response = event.get("response").unwrap_or(&empty);
    let mut events = Vec::new();
    close_all_open_blocks(state, &mut events);

    let message = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("The response failed due to an unknown error.");

    events.push(build_error_event(message));
    state.message_completed = true;

    events
}

fn handle_error_event(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let message =
        get_str(event, "message").unwrap_or("An unexpected error occurred during streaming.");

    state.message_completed = true;
    vec![build_error_event(message)]
}

/// Mirrors `handleFunctionCallArgumentsValidationError`. The already-accumulated
/// `events` are preserved, open blocks closed, then the error appended.
fn handle_function_call_arguments_validation_error(
    reason: &str,
    state: &mut ResponsesStreamState,
    mut events: Vec<AnthropicStreamEventData>,
) -> Vec<AnthropicStreamEventData> {
    close_all_open_blocks(state, &mut events);
    state.message_completed = true;
    events.push(build_error_event(reason));
    events
}

// ---------------------------------------------------------------------------
// Result mapping (inlined from responses-translation.ts so this compiles alone)
// ---------------------------------------------------------------------------

/// Mirrors `mapResponsesStopReason`.
fn map_responses_stop_reason(response: &Value, has_tool_call: bool) -> Option<String> {
    let status = response.get("status").and_then(Value::as_str);

    if status == Some("completed") {
        let output = response.get("output").and_then(Value::as_array);
        match output {
            None => {
                return Some(if has_tool_call { "tool_use" } else { "end_turn" }.to_string());
            }
            Some(items) if items.is_empty() => {
                return Some(if has_tool_call { "tool_use" } else { "end_turn" }.to_string());
            }
            Some(items) => {
                let has_call = items.iter().any(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call") | Some("tool_search_call")
                    )
                });
                return Some(if has_call { "tool_use" } else { "end_turn" }.to_string());
            }
        }
    }

    if status == Some("incomplete") {
        let reason = response
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str);
        if reason == Some("max_output_tokens") {
            return Some("max_tokens".to_string());
        }
        if reason == Some("content_filter") {
            return Some("end_turn".to_string());
        }
    }

    None
}

/// Mirrors `mapResponsesUsage`, shaped for the streaming `message_delta`.
fn map_responses_usage_delta(response: &Value) -> AnthropicMessageDeltaUsage {
    let usage = response.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cached = usage
        .and_then(|u| u.get("input_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64);

    AnthropicMessageDeltaUsage {
        input_tokens: Some(input_tokens - cached.unwrap_or(0)),
        output_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: cached,
    }
}

/// Mirrors `stringifyToolSearchArguments`.
fn stringify_tool_search_arguments(arguments_value: &Value) -> Option<String> {
    serde_json::to_string(&format_tool_search_bridge_arguments(arguments_value)).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn delta_text(ev: &AnthropicStreamEventData) -> Option<String> {
        match ev {
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::TextDelta { text },
                ..
            } => Some(text.clone()),
            _ => None,
        }
    }

    #[test]
    fn output_text_delta_sequence_produces_anthropic_events() {
        let mut state = create_responses_stream_state(None);

        // response.created -> message_start
        let created = json!({
            "type": "response.created",
            "response": {
                "id": "resp_1",
                "model": "gpt-5",
                "usage": { "input_tokens": 10, "input_tokens_details": { "cached_tokens": 2 } }
            }
        });
        let evs = translate_responses_stream_event(&created, &mut state);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AnthropicStreamEventData::MessageStart { message } => {
                assert_eq!(message.id, "resp_1");
                assert_eq!(message.usage.input_tokens, 8); // 10 - 2 cached
                assert_eq!(message.usage.cache_read_input_tokens, Some(2));
            }
            other => panic!("expected message_start, got {other:?}"),
        }

        // first text delta opens a content block then emits a text_delta
        let d1 = json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "Hello"
        });
        let evs = translate_responses_stream_event(&d1, &mut state);
        assert_eq!(evs.len(), 2);
        assert!(matches!(
            evs[0],
            AnthropicStreamEventData::ContentBlockStart { index: 0, .. }
        ));
        assert_eq!(delta_text(&evs[1]).as_deref(), Some("Hello"));

        // second delta on same block: only a delta event
        let d2 = json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": " world"
        });
        let evs = translate_responses_stream_event(&d2, &mut state);
        assert_eq!(evs.len(), 1);
        assert_eq!(delta_text(&evs[0]).as_deref(), Some(" world"));

        // done with same text: block already had deltas, so nothing emitted
        let done = json!({
            "type": "response.output_text.done",
            "output_index": 0,
            "content_index": 0,
            "text": "Hello world"
        });
        let evs = translate_responses_stream_event(&done, &mut state);
        assert!(evs.is_empty());

        // terminal event closes the open block and finishes the message
        let completed = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{ "type": "message" }],
                "usage": { "input_tokens": 10, "output_tokens": 5, "input_tokens_details": { "cached_tokens": 2 } }
            }
        });
        let evs = translate_responses_stream_event(&completed, &mut state);
        // content_block_stop, message_delta, message_stop
        assert_eq!(evs.len(), 3);
        assert!(matches!(
            evs[0],
            AnthropicStreamEventData::ContentBlockStop { index: 0 }
        ));
        match &evs[1] {
            AnthropicStreamEventData::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
                let usage = usage.as_ref().expect("usage present");
                assert_eq!(usage.input_tokens, Some(8));
                assert_eq!(usage.output_tokens, 5);
                assert_eq!(usage.cache_read_input_tokens, Some(2));
            }
            other => panic!("expected message_delta, got {other:?}"),
        }
        assert!(matches!(evs[2], AnthropicStreamEventData::MessageStop));
        assert!(state.message_completed);
    }

    #[test]
    fn whitespace_guard_trips_at_21_consecutive_whitespace() {
        let mut state = create_responses_stream_state(None);

        // open a function-call block
        let added = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "function_call", "call_id": "call_1", "name": "do_thing", "arguments": "" }
        });
        let evs = translate_responses_stream_event(&added, &mut state);
        assert!(matches!(
            evs[0],
            AnthropicStreamEventData::ContentBlockStart { .. }
        ));

        // 20 whitespace chars: still under the cap, accepted as a delta
        let twenty = "\n".repeat(20);
        let d = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": twenty,
        });
        let evs = translate_responses_stream_event(&d, &mut state);
        assert!(evs
            .iter()
            .any(|e| matches!(e, AnthropicStreamEventData::ContentBlockDelta { .. })));
        assert!(!state.message_completed);

        // one more whitespace char makes it 21 consecutive -> trips the guard
        let d = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "\n",
        });
        let evs = translate_responses_stream_event(&d, &mut state);
        // closes the open block + emits an error
        assert!(matches!(
            evs.last(),
            Some(AnthropicStreamEventData::Error { .. })
        ));
        assert!(state.message_completed);
    }

    #[test]
    fn whitespace_run_helper_caps_at_20() {
        // 20 newlines from a fresh run: not exceeded.
        let (count, exceeded) = update_whitespace_run_state(0, &"\n".repeat(20));
        assert_eq!(count, 20);
        assert!(!exceeded);

        // the 21st consecutive whitespace trips it.
        let (_, exceeded) = update_whitespace_run_state(20, "\n");
        assert!(exceeded);

        // a non-space, non-newline char resets the run.
        let (count, exceeded) = update_whitespace_run_state(15, "x");
        assert_eq!(count, 0);
        assert!(!exceeded);

        // a plain space leaves the run unchanged.
        let (count, exceeded) = update_whitespace_run_state(5, " ");
        assert_eq!(count, 5);
        assert!(!exceeded);
    }

    #[test]
    fn terminal_failed_event_sets_message_completed_and_errors() {
        let mut state = create_responses_stream_state(None);
        let failed = json!({
            "type": "response.failed",
            "response": { "error": { "message": "boom" } }
        });
        let evs = translate_responses_stream_event(&failed, &mut state);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AnthropicStreamEventData::Error { error } => {
                assert_eq!(error.message, "boom");
                assert_eq!(error.kind, "api_error");
            }
            other => panic!("expected error, got {other:?}"),
        }
        assert!(state.message_completed);
    }

    #[test]
    fn error_event_sets_message_completed() {
        let mut state = create_responses_stream_state(None);
        let err = json!({ "type": "error", "message": "stream broke" });
        let evs = translate_responses_stream_event(&err, &mut state);
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], AnthropicStreamEventData::Error { .. }));
        assert!(state.message_completed);
    }

    #[test]
    fn function_call_arguments_delta_emits_input_json() {
        let mut state = create_responses_stream_state(None);
        let added = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "function_call", "call_id": "c1", "name": "fn", "arguments": "" }
        });
        translate_responses_stream_event(&added, &mut state);

        let d = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"a\":1}"
        });
        let evs = translate_responses_stream_event(&d, &mut state);
        match evs.last() {
            Some(AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::InputJsonDelta { partial_json },
                ..
            }) => assert_eq!(partial_json, "{\"a\":1}"),
            other => panic!("expected input_json_delta, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_done_emits_default_thinking_and_signature() {
        let mut state = create_responses_stream_state(None);
        let done = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "reasoning", "id": "r1", "encrypted_content": "enc", "summary": [] }
        });
        let evs = translate_responses_stream_event(&done, &mut state);
        // content_block_start(thinking), thinking_delta(default), signature_delta
        assert!(matches!(
            evs[0],
            AnthropicStreamEventData::ContentBlockStart { .. }
        ));
        assert!(matches!(
            evs[1],
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::ThinkingDelta { .. },
                ..
            }
        ));
        match evs.last() {
            Some(AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::SignatureDelta { signature },
                ..
            }) => assert_eq!(signature, "enc@r1"),
            other => panic!("expected signature_delta, got {other:?}"),
        }
    }
}
