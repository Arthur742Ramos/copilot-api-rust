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

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{json, Value};

use crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES;
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
const MAX_TRACKED_OUTPUT_ITEMS: usize = 4096;
const MAX_TRACKED_CONTENT_PARTS: usize = 4096;
const MAX_BUFFERED_TRANSLATION_BYTES: usize = MAX_UPSTREAM_RESPONSE_BYTES;

/// Imported from [`super::utils`] so all translation modules share one source of
/// truth for the "Thinking..." placeholder.
use super::utils::THINKING_TEXT;

/// Shared with [`super::responses_translation`] so the byte-exact compaction
/// carrier signature (`cm1#{enc}@{id}`) has a single source of truth — the
/// signature must match exactly for Copilot cache hits, so a divergent second
/// copy would silently corrupt them.
use super::responses_translation::{
    effective_reasoning_text, encode_compaction_carrier_signature, encode_reasoning_signature,
};

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
    pub buffered_arguments: Vec<String>,
    pub accumulated_arguments: String,
    pub started: bool,
    pub done: bool,
}

#[derive(Debug, Clone)]
pub struct OutputItemLifecycle {
    pub item_type: String,
    pub item_id: Option<String>,
    pub done: bool,
    pub added_item: Option<Value>,
    pub done_item: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct ReasoningSummaryPartState {
    pub text: String,
    pub done_text: Option<String>,
    pub added: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ReasoningItemStreamState {
    pub item_id: Option<String>,
    pub summary_parts: BTreeMap<i64, ReasoningSummaryPartState>,
    pub content_parts: BTreeMap<i64, String>,
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
    pub function_call_order: Vec<i64>,
    pub active_function_call_output_index: Option<i64>,
    pub output_items_by_index: HashMap<i64, OutputItemLifecycle>,
    pub output_index_by_item_id: HashMap<String, i64>,
    pub reasoning_state_by_output_index: HashMap<i64, ReasoningItemStreamState>,
    pub last_sequence_number: Option<i64>,
    pub last_sequence_event: Option<Value>,
    pub buffered_translation_bytes: usize,
    pub tracked_reasoning_parts: usize,
    pub tracked_text_parts: usize,
    pub output_text_by_key: HashMap<String, String>,
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
            function_call_order: Vec::new(),
            active_function_call_output_index: None,
            output_items_by_index: HashMap::new(),
            output_index_by_item_id: HashMap::new(),
            reasoning_state_by_output_index: HashMap::new(),
            last_sequence_number: None,
            last_sequence_event: None,
            buffered_translation_bytes: 0,
            tracked_reasoning_parts: 0,
            tracked_text_parts: 0,
            output_text_by_key: HashMap::new(),
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
    if state.message_completed {
        return Vec::new();
    }
    match validate_event_sequence(event, state) {
        Ok(true) => return Vec::new(),
        Ok(false) => {}
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    }

    let event_type = event.get("type").and_then(Value::as_str);
    if event_type == Some("response.created") && state.message_start_sent {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("The Responses stream emitted more than one response.created event."),
        );
    }
    if !state.message_start_sent
        && !matches!(
            event_type,
            Some("response.created" | "response.failed" | "error")
        )
    {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("The Responses stream emitted an event before response.created."),
        );
    }

    match event_type {
        Some("response.created") => handle_response_created(event, state),
        Some("response.output_item.added") => handle_output_item_added(event, state),
        Some("response.reasoning_summary_part.added") => {
            handle_reasoning_summary_part_added(event, state)
        }
        Some("response.reasoning_summary_text.delta") => {
            handle_reasoning_summary_text_delta(event, state)
        }
        Some("response.output_text.delta") => handle_output_text_delta(event, state),
        Some("response.reasoning_summary_text.done") => {
            handle_reasoning_summary_text_done(event, state)
        }
        Some("response.reasoning_text.delta") => handle_reasoning_text_delta(event, state),
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

/// Close any open content block and emit one terminal error. Used by stream
/// drivers for transport failures, malformed SSE records, and premature EOF.
pub fn terminate_responses_stream_with_error(
    state: &mut ResponsesStreamState,
    error: AnthropicStreamEventData,
) -> Vec<AnthropicStreamEventData> {
    if state.message_completed {
        return Vec::new();
    }
    let mut events = Vec::new();
    close_all_open_blocks(state, &mut events);
    events.push(error);
    state.message_completed = true;
    events
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

fn required_nonnegative_index(
    event: &Value,
    key: &str,
    missing_message: &'static str,
    negative_message: &'static str,
) -> Result<i64, &'static str> {
    let Some(index) = event.get(key).and_then(Value::as_i64) else {
        return Err(missing_message);
    };
    if index < 0 {
        return Err(negative_message);
    }
    Ok(index)
}

fn validate_event_sequence(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Result<bool, &'static str> {
    let Some(raw_sequence) = event.get("sequence_number") else {
        return Ok(false);
    };
    let Some(sequence) = raw_sequence.as_i64() else {
        return Err("A Responses stream event had a non-integer sequence number.");
    };
    if sequence < 0 {
        return Err("A Responses stream event had a negative sequence number.");
    }
    if let Some(last) = state.last_sequence_number {
        if sequence < last {
            return Err("The Responses stream emitted an out-of-order sequence number.");
        }
        if sequence == last {
            if state.last_sequence_event.as_ref() == Some(event) {
                return Ok(true);
            }
            return Err("The Responses stream reused a sequence number for a different event.");
        }
    }
    state.last_sequence_number = Some(sequence);
    state.last_sequence_event = Some(event.clone());
    Ok(false)
}

fn reserve_buffered_translation_bytes(
    state: &mut ResponsesStreamState,
    additional: usize,
) -> Result<(), &'static str> {
    let Some(total) = state.buffered_translation_bytes.checked_add(additional) else {
        return Err("The Responses stream exceeded the translation buffer limit.");
    };
    if total > MAX_BUFFERED_TRANSLATION_BYTES {
        return Err("The Responses stream exceeded the translation buffer limit.");
    }
    state.buffered_translation_bytes = total;
    Ok(())
}

fn reserve_json_value(state: &mut ResponsesStreamState, value: &Value) -> Result<(), &'static str> {
    let size = serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(MAX_BUFFERED_TRANSLATION_BYTES.saturating_add(1));
    reserve_buffered_translation_bytes(state, size)
}

fn block_key(output_index: i64, content_index: i64) -> String {
    format!("{output_index}:{content_index}")
}

fn indices_are_contiguous<T>(parts: &BTreeMap<i64, T>) -> bool {
    parts
        .keys()
        .enumerate()
        .all(|(expected, actual)| *actual == expected as i64)
}

fn reserve_reasoning_part(
    state: &mut ResponsesStreamState,
    output_index: i64,
    part_index: i64,
    summary: bool,
) -> Result<(), &'static str> {
    let exists = state
        .reasoning_state_by_output_index
        .get(&output_index)
        .is_some_and(|reasoning| {
            if summary {
                reasoning.summary_parts.contains_key(&part_index)
            } else {
                reasoning.content_parts.contains_key(&part_index)
            }
        });
    if !exists {
        if state.tracked_reasoning_parts >= MAX_TRACKED_CONTENT_PARTS {
            return Err("The Responses stream emitted too many reasoning parts.");
        }
        state.tracked_reasoning_parts += 1;
    }
    Ok(())
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

fn validate_done_item_identity(
    added: &Value,
    done: &Value,
    item_type: &str,
) -> Result<(), &'static str> {
    let conflicts = |field: &str| {
        let added = get_str(added, field).filter(|value| !value.is_empty());
        let done = get_str(done, field).filter(|value| !value.is_empty());
        added.is_some() && done.is_some() && added != done
    };
    if matches!(item_type, "function_call" | "tool_search_call") && conflicts("call_id") {
        return Err("A completed function/tool call changed its call id.");
    }
    if item_type == "function_call" {
        let added_name = resolve_tool_use_name(added);
        let done_name = resolve_tool_use_name(done);
        if !added_name.is_empty() && !done_name.is_empty() && added_name != done_name {
            return Err("A completed function call changed its function name.");
        }
    }
    if item_type == "message" && conflicts("role") {
        return Err("A completed message changed its role.");
    }
    Ok(())
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
    state.function_call_order.clear();
    state.active_function_call_output_index = None;
    state.output_items_by_index.clear();
    state.output_index_by_item_id.clear();
    state.reasoning_state_by_output_index.clear();
    state.last_sequence_number = None;
    state.last_sequence_event = None;
    state.buffered_translation_bytes = 0;
    state.tracked_reasoning_parts = 0;
    state.tracked_text_parts = 0;
    state.output_text_by_key.clear();
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
        let resolved_name = name
            .map(str::to_string)
            .unwrap_or_else(|| "function".to_string());

        state.function_call_state_by_output_index.insert(
            output_index,
            FunctionCallStreamState {
                block_index,
                tool_call_id: resolved_tool_call_id,
                name: resolved_name,
                consecutive_whitespace_count: 0,
                buffered_arguments: Vec::new(),
                accumulated_arguments: String::new(),
                started: false,
                done: false,
            },
        );
        state.function_call_order.push(output_index);
    } else if let Some(fc) = state
        .function_call_state_by_output_index
        .get_mut(&output_index)
    {
        // An arguments event can arrive before output_item.added. Replace the
        // temporary defaults once authoritative metadata appears.
        if let Some(id) = tool_call_id.filter(|id| !id.is_empty()) {
            fc.tool_call_id = id.to_string();
        }
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            fc.name = name.to_string();
        }
    }

    if state.active_function_call_output_index.is_none() {
        state.active_function_call_output_index = Some(output_index);
    }

    let is_active = state.active_function_call_output_index == Some(output_index);
    let fc = state
        .function_call_state_by_output_index
        .get(&output_index)
        .expect("function call state just inserted");
    let block_index = fc.block_index;
    let should_start = is_active && !fc.started;
    let tool_call_id = fc.tool_call_id.clone();
    let name = fc.name.clone();

    if should_start {
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
        if let Some(fc) = state
            .function_call_state_by_output_index
            .get_mut(&output_index)
        {
            fc.started = true;
        }
    }

    block_index
}

fn function_call_is_active(state: &ResponsesStreamState, output_index: i64) -> bool {
    state.active_function_call_output_index == Some(output_index)
}

fn append_function_call_arguments(
    state: &mut ResponsesStreamState,
    output_index: i64,
    arguments: String,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), &'static str> {
    if arguments.is_empty() {
        return Ok(());
    }
    reserve_buffered_translation_bytes(state, arguments.len())?;
    let active = function_call_is_active(state, output_index);
    let Some(call) = state
        .function_call_state_by_output_index
        .get_mut(&output_index)
    else {
        return Err("Received function call arguments without an output item.");
    };
    call.accumulated_arguments.push_str(&arguments);
    let block_index = call.block_index;
    if active {
        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: block_index,
            delta: AnthropicContentBlockDelta::InputJsonDelta {
                partial_json: arguments,
            },
        });
        state.block_has_delta.insert(block_index);
    } else {
        call.buffered_arguments.push(arguments);
    }
    Ok(())
}

fn reconcile_function_call_arguments(
    state: &mut ResponsesStreamState,
    output_index: i64,
    authoritative: &str,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), &'static str> {
    let Some(current) = state
        .function_call_state_by_output_index
        .get(&output_index)
        .map(|call| call.accumulated_arguments.clone())
    else {
        return Err("Received completed function arguments without an output item.");
    };
    let Some(suffix) = authoritative.strip_prefix(&current) else {
        return Err("Completed function arguments conflicted with streamed argument deltas.");
    };
    append_function_call_arguments(state, output_index, suffix.to_string(), events)
}

/// Finish the active call and activate buffered parallel calls in first-seen
/// order. Calls already marked done are emitted completely and drained; the
/// first unfinished call remains active for future deltas.
fn advance_function_call_queue(
    state: &mut ResponsesStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    if let Some(active) = state.active_function_call_output_index.take() {
        if let Some(fc) = state.function_call_state_by_output_index.remove(&active) {
            if open_blocks_has(state, fc.block_index) {
                events.push(AnthropicStreamEventData::ContentBlockStop {
                    index: fc.block_index,
                });
                state.open_blocks.retain(|index| *index != fc.block_index);
                state.block_has_delta.remove(&fc.block_index);
            }
        }
    }

    loop {
        let next = state.function_call_order.iter().copied().find(|index| {
            state
                .function_call_state_by_output_index
                .contains_key(index)
        });
        let Some(next) = next else {
            return;
        };

        state.active_function_call_output_index = Some(next);
        let block_index = open_function_call_block(state, next, None, None, events);
        let (fragments, done) = {
            let fc = state
                .function_call_state_by_output_index
                .get_mut(&next)
                .expect("queued function call exists");
            (std::mem::take(&mut fc.buffered_arguments), fc.done)
        };
        for partial_json in fragments {
            events.push(AnthropicStreamEventData::ContentBlockDelta {
                index: block_index,
                delta: AnthropicContentBlockDelta::InputJsonDelta { partial_json },
            });
            state.block_has_delta.insert(block_index);
        }
        if !done {
            return;
        }
        if open_blocks_has(state, block_index) {
            events.push(AnthropicStreamEventData::ContentBlockStop { index: block_index });
            state.open_blocks.retain(|index| *index != block_index);
            state.block_has_delta.remove(&block_index);
        }
        state.function_call_state_by_output_index.remove(&next);
        state.active_function_call_output_index = None;
    }
}

fn finish_all_function_calls(
    state: &mut ResponsesStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    for fc in state.function_call_state_by_output_index.values_mut() {
        fc.done = true;
    }
    while !state.function_call_state_by_output_index.is_empty() {
        advance_function_call_queue(state, events);
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn handle_response_created(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let Some(response) = event
        .get("response")
        .filter(|response| response.is_object())
    else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.created event was missing its response object."),
        );
    };
    if get_str(response, "id").is_none() || get_str(response, "model").is_none() {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.created event was missing its response id or model."),
        );
    }
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
    let input_tokens = (input_tokens_raw - input_cached_tokens.unwrap_or(0)).max(0);

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
                cache_read_input_tokens: input_cached_tokens,
                service_tier: None,
                extra: serde_json::Map::new(),
            },
        },
    }]
}

fn handle_output_item_added(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let Some(item) = event.get("item") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.output_item.added event was missing its item."),
        );
    };
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.output_item.added event was missing its item type."),
        );
    };
    let output_index = match required_nonnegative_index(
        event,
        "output_index",
        "A response.output_item.added event was missing its output index.",
        "A response.output_item.added event contained a negative output index.",
    ) {
        Ok(index) => index,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);

    if let Some(existing) = state.output_items_by_index.get(&output_index) {
        if !existing.done && existing.added_item.as_ref() == Some(item) {
            // Identical replay before completion is safe to deduplicate.
            return events;
        }
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "The Responses stream reused an output index for a different or completed item.",
            ),
        );
    }
    if state.output_items_by_index.len() >= MAX_TRACKED_OUTPUT_ITEMS {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("The Responses stream emitted too many output items."),
        );
    }
    if let Err(message) = reserve_json_value(state, item) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    if let Some(nonempty_id) = item_id.as_deref().filter(|id| !id.is_empty()) {
        if state
            .output_index_by_item_id
            .get(nonempty_id)
            .is_some_and(|index| *index != output_index)
        {
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "The Responses stream reused a reasoning/output item id at another index.",
                ),
            );
        }
        state
            .output_index_by_item_id
            .insert(nonempty_id.to_string(), output_index);
    }
    state.output_items_by_index.insert(
        output_index,
        OutputItemLifecycle {
            item_type: item_type.to_string(),
            item_id: item_id.clone(),
            done: false,
            added_item: Some(item.clone()),
            done_item: None,
        },
    );
    if item_type == "reasoning" {
        state.reasoning_state_by_output_index.insert(
            output_index,
            ReasoningItemStreamState {
                item_id,
                ..Default::default()
            },
        );
    }

    let details = match extract_function_call_details(event, state) {
        Some(d) => d,
        None => return events,
    };

    open_function_call_block(
        state,
        details.output_index,
        Some(&details.tool_call_id),
        Some(&details.name),
        &mut events,
    );

    if let Some(initial) = details.initial_arguments {
        if let Err(message) =
            append_function_call_arguments(state, details.output_index, initial, &mut events)
        {
            return terminate_responses_stream_with_error(state, build_error_event(message));
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
    let Some(item) = event.get("item") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.output_item.done event was missing its item."),
        );
    };
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.output_item.done event was missing its item type."),
        );
    };
    let output_index = match required_nonnegative_index(
        event,
        "output_index",
        "A response.output_item.done event was missing its output index.",
        "A response.output_item.done event contained a negative output index.",
    ) {
        Ok(index) => index,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);

    if let Some(existing) = state.output_items_by_index.get(&output_index).cloned() {
        if existing.item_type != item_type {
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "A response.output_item.done event did not match its added item type.",
                ),
            );
        }
        if let Some(added) = existing.added_item.as_ref() {
            if let Err(message) = validate_done_item_identity(added, item, item_type) {
                return terminate_responses_stream_with_error(state, build_error_event(message));
            }
        }
        let expected_id = existing.item_id.as_deref().filter(|id| !id.is_empty());
        let actual_id = item_id.as_deref().filter(|id| !id.is_empty());
        if expected_id.is_some() && actual_id.is_some() && expected_id != actual_id {
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "A response.output_item.done event did not match its added item id.",
                ),
            );
        }
        if existing.done {
            if existing.done_item.as_ref() == Some(item) {
                // A structurally identical JSON replay is safe to deduplicate.
                return events;
            }
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "The Responses stream emitted conflicting response.output_item.done events.",
                ),
            );
        }
        if let Err(message) = reserve_json_value(state, item) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        if let Some(lifecycle) = state.output_items_by_index.get_mut(&output_index) {
            lifecycle.done = true;
            lifecycle.done_item = Some(item.clone());
            if lifecycle.item_id.as_deref().is_none_or(|id| id.is_empty()) && actual_id.is_some() {
                lifecycle.item_id.clone_from(&item_id);
            }
        }
    } else {
        // Some compatible upstreams omit `output_item.added` when an item has no
        // deltas. Treat the first complete item as an implicit add+done, while
        // still requiring an explicit add before any incremental reasoning event.
        if state.output_items_by_index.len() >= MAX_TRACKED_OUTPUT_ITEMS {
            return terminate_responses_stream_with_error(
                state,
                build_error_event("The Responses stream emitted too many output items."),
            );
        }
        if let Err(message) = reserve_json_value(state, item) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        state.output_items_by_index.insert(
            output_index,
            OutputItemLifecycle {
                item_type: item_type.to_string(),
                item_id: item_id.clone(),
                done: true,
                added_item: None,
                done_item: Some(item.clone()),
            },
        );
    }
    if let Some(nonempty_id) = item_id.as_deref().filter(|id| !id.is_empty()) {
        if state
            .output_index_by_item_id
            .get(nonempty_id)
            .is_some_and(|index| *index != output_index)
        {
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "A response.output_item.done event reused an item id at another index.",
                ),
            );
        }
        state
            .output_index_by_item_id
            .insert(nonempty_id.to_string(), output_index);
    }

    if item_type == "message" {
        if let Err(message) = render_complete_message_item(item, state, output_index, &mut events) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        return events;
    }

    if matches!(item_type, "function_call" | "tool_search_call") {
        let call_id = get_str(item, "call_id").unwrap_or("").to_string();
        let (name, final_arguments) = if item_type == "tool_search_call" {
            (
                state.tool_search_name.clone(),
                stringify_tool_search_arguments(item.get("arguments").unwrap_or(&Value::Null)),
            )
        } else {
            (
                resolve_tool_use_name(item),
                get_str(item, "arguments").map(str::to_string),
            )
        };
        open_function_call_block(
            state,
            output_index,
            Some(&call_id),
            Some(&name),
            &mut events,
        );

        if let Some(args) = final_arguments {
            if let Err(message) =
                reconcile_function_call_arguments(state, output_index, &args, &mut events)
            {
                return terminate_responses_stream_with_error(state, build_error_event(message));
            }
        }

        let active = function_call_is_active(state, output_index);
        if let Some(fc) = state
            .function_call_state_by_output_index
            .get_mut(&output_index)
        {
            fc.done = true;
        }
        if active {
            advance_function_call_queue(state, &mut events);
        }
        return events;
    }

    if item_type == "compaction" {
        let id = get_str(item, "id").unwrap_or("");
        let encrypted_content = get_str(item, "encrypted_content").unwrap_or("");
        if encrypted_content.is_empty() {
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
                signature: encode_compaction_carrier_signature(encrypted_content, id),
            },
        });
        state.block_has_delta.insert(block_index);
        return events;
    }

    if item_type != "reasoning" {
        return events;
    }

    let encrypted_content = get_str(item, "encrypted_content");
    let lifecycle_id = state
        .output_items_by_index
        .get(&output_index)
        .and_then(|lifecycle| lifecycle.item_id.clone());
    let id = get_str(item, "id").or(lifecycle_id.as_deref());
    let buffered = state
        .reasoning_state_by_output_index
        .remove(&output_index)
        .unwrap_or_default();
    if !indices_are_contiguous(&buffered.summary_parts)
        || !indices_are_contiguous(&buffered.content_parts)
    {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A reasoning item completed with sparse or out-of-order part indices.",
            ),
        );
    }
    let buffered_summary_segments: Vec<String> = buffered
        .summary_parts
        .into_values()
        .map(|part| part.text)
        .collect();
    let buffered_content_segments: Vec<String> = buffered.content_parts.into_values().collect();
    let summary_segments: Vec<String> = match item.get("summary").and_then(Value::as_array) {
        Some(summary) if !summary.is_empty() || buffered_summary_segments.is_empty() => summary
            .iter()
            .map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect(),
        _ => buffered_summary_segments,
    };
    let content_segments: Vec<String> = match item.get("content").and_then(Value::as_array) {
        Some(content) if !content.is_empty() || buffered_content_segments.is_empty() => content
            .iter()
            .map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect(),
        _ => buffered_content_segments,
    };
    let display_text = effective_reasoning_text(
        summary_segments
            .iter()
            .chain(content_segments.iter())
            .map(String::as_str),
        encrypted_content,
        id,
    );

    // A carrier-free item with no aggregate summary has no Anthropic thinking
    // content to represent. Do not open a block or invent a signature.
    let Some(display_text) = display_text else {
        return events;
    };

    let block_index = open_thinking_block_if_needed(state, output_index, &mut events);
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::ThinkingDelta {
            thinking: display_text,
        },
    });
    state.block_has_delta.insert(block_index);

    let signature = encode_reasoning_signature(encrypted_content, id);
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::SignatureDelta { signature },
    });
    state.block_has_delta.insert(block_index);

    events
}

fn append_output_text(
    state: &mut ResponsesStreamState,
    output_index: i64,
    content_index: i64,
    text: &str,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), &'static str> {
    let key = block_key(output_index, content_index);
    if !state.output_text_by_key.contains_key(&key) {
        if state.tracked_text_parts >= MAX_TRACKED_CONTENT_PARTS {
            return Err("The Responses stream emitted too many text content parts.");
        }
        state.tracked_text_parts += 1;
        state.output_text_by_key.insert(key.clone(), String::new());
    }
    if text.is_empty() {
        return Ok(());
    }
    reserve_buffered_translation_bytes(state, text.len())?;
    state
        .output_text_by_key
        .get_mut(&key)
        .expect("text part inserted above")
        .push_str(text);
    let block_index = open_text_block_if_needed(state, output_index, content_index, events);
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::TextDelta {
            text: text.to_string(),
        },
    });
    state.block_has_delta.insert(block_index);
    Ok(())
}

fn reconcile_output_text(
    state: &mut ResponsesStreamState,
    output_index: i64,
    content_index: i64,
    authoritative: &str,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), &'static str> {
    let key = block_key(output_index, content_index);
    let current = state
        .output_text_by_key
        .get(&key)
        .cloned()
        .unwrap_or_default();
    let Some(suffix) = authoritative.strip_prefix(&current) else {
        return Err("Completed output text conflicted with streamed text deltas.");
    };
    append_output_text(state, output_index, content_index, suffix, events)
}

fn render_complete_message_item(
    item: &Value,
    state: &mut ResponsesStreamState,
    output_index: i64,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), &'static str> {
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return Ok(());
    };
    for (content_index, block) in content.iter().enumerate() {
        let text = match block.get("type").and_then(Value::as_str) {
            Some("output_text") => block.get("text").and_then(Value::as_str),
            Some("refusal") => block.get("refusal").and_then(Value::as_str),
            _ => block
                .get("text")
                .or_else(|| block.get("reasoning"))
                .and_then(Value::as_str),
        };
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            continue;
        };
        let content_index = content_index as i64;
        reconcile_output_text(state, output_index, content_index, text, events)?;
    }
    Ok(())
}

fn handle_function_call_arguments_delta(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index =
        match validate_active_output_item(event, state, &["function_call", "tool_search_call"]) {
            Ok(index) => index,
            Err(message) => {
                return terminate_responses_stream_with_error(state, build_error_event(message))
            }
        };
    let Some(delta_text) = get_str(event, "delta") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.function_call_arguments.delta event was missing its delta.",
            ),
        );
    };

    if delta_text.is_empty() {
        return events;
    }

    open_function_call_block(state, output_index, None, None, &mut events);

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

    if let Some(fc) = state
        .function_call_state_by_output_index
        .get_mut(&output_index)
    {
        fc.consecutive_whitespace_count = next_count;
    }

    if let Err(message) =
        append_function_call_arguments(state, output_index, delta_text.to_string(), &mut events)
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }

    events
}

fn handle_function_call_arguments_done(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index =
        match validate_active_output_item(event, state, &["function_call", "tool_search_call"]) {
            Ok(index) => index,
            Err(message) => {
                return terminate_responses_stream_with_error(state, build_error_event(message))
            }
        };
    open_function_call_block(state, output_index, None, None, &mut events);
    let Some(final_arguments) = get_str(event, "arguments") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.function_call_arguments.done event was missing its arguments.",
            ),
        );
    };
    if let Err(message) =
        reconcile_function_call_arguments(state, output_index, final_arguments, &mut events)
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    events
}

fn handle_output_text_delta(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = match validate_active_output_item(event, state, &["message"]) {
        Ok(index) => index,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let content_index = match required_nonnegative_index(
        event,
        "content_index",
        "A response.output_text.delta event was missing its content index.",
        "A response.output_text.delta event contained a negative content index.",
    ) {
        Ok(index) => index,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let Some(delta_text) = get_str(event, "delta") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.output_text.delta event was missing its delta."),
        );
    };
    if let Err(message) =
        append_output_text(state, output_index, content_index, delta_text, &mut events)
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }

    events
}

fn validate_active_output_item(
    event: &Value,
    state: &mut ResponsesStreamState,
    expected_types: &[&str],
) -> Result<i64, &'static str> {
    let output_index = required_nonnegative_index(
        event,
        "output_index",
        "An incremental output event was missing its output index.",
        "An incremental output event contained a negative output index.",
    )?;
    let Some(lifecycle) = state.output_items_by_index.get(&output_index) else {
        return Err("An incremental output event arrived before response.output_item.added.");
    };
    if lifecycle.done || !expected_types.contains(&lifecycle.item_type.as_str()) {
        return Err("An incremental output event targeted a completed or incompatible item.");
    }
    let expected_id = lifecycle
        .item_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let actual_id = event
        .get("item_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    if let (Some(expected), Some(actual)) = (expected_id.as_deref(), actual_id) {
        if expected != actual {
            return Err("An incremental output event item id did not match its output item.");
        }
    }
    if expected_id.is_none() {
        if let Some(actual) = actual_id {
            if state
                .output_index_by_item_id
                .get(actual)
                .is_some_and(|index| *index != output_index)
            {
                return Err("An incremental output event reused an item id at another index.");
            }
            state
                .output_index_by_item_id
                .insert(actual.to_string(), output_index);
            if let Some(lifecycle) = state.output_items_by_index.get_mut(&output_index) {
                lifecycle.item_id = Some(actual.to_string());
            }
        }
    }
    Ok(output_index)
}

fn validate_reasoning_event(
    event: &Value,
    state: &mut ResponsesStreamState,
    index_field: &str,
) -> Result<(i64, i64), &'static str> {
    let output_index = required_nonnegative_index(
        event,
        "output_index",
        "A reasoning stream event was missing its output index.",
        "A reasoning stream event contained a negative output index.",
    )?;
    let part_index = required_nonnegative_index(
        event,
        index_field,
        "A reasoning stream event was missing its part index.",
        "A reasoning stream event contained a negative part index.",
    )?;
    let Some(lifecycle) = state.output_items_by_index.get(&output_index) else {
        return Err("A reasoning stream event arrived before response.output_item.added.");
    };
    if lifecycle.item_type != "reasoning" || lifecycle.done {
        return Err("A reasoning stream event arrived for a non-reasoning or completed item.");
    }
    let expected_id = lifecycle
        .item_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let actual_id = event
        .get("item_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    if let (Some(expected), Some(actual)) = (expected_id.as_deref(), actual_id) {
        if expected != actual {
            return Err("A reasoning stream event item id did not match its output item.");
        }
    }
    if expected_id.is_none() {
        if let Some(actual) = actual_id {
            if state
                .output_index_by_item_id
                .get(actual)
                .is_some_and(|index| *index != output_index)
            {
                return Err("A reasoning stream event reused an item id at another output index.");
            }
            state
                .output_index_by_item_id
                .insert(actual.to_string(), output_index);
            if let Some(lifecycle) = state.output_items_by_index.get_mut(&output_index) {
                lifecycle.item_id = Some(actual.to_string());
            }
            if let Some(reasoning) = state.reasoning_state_by_output_index.get_mut(&output_index) {
                reasoning.item_id = Some(actual.to_string());
            }
        }
    }
    Ok((output_index, part_index))
}

fn handle_reasoning_summary_part_added(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let (output_index, summary_index) =
        match validate_reasoning_event(event, state, "summary_index") {
            Ok(value) => value,
            Err(message) => {
                return terminate_responses_stream_with_error(state, build_error_event(message))
            }
        };
    if let Err(message) = reserve_reasoning_part(state, output_index, summary_index, true) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    // Codex 0.144.1 maps this event before the part's text delta/done events.
    // Keep one ordered slot per semantic part; duplicate `added` events reuse the
    // same slot and therefore cannot create duplicate separators.
    let part = state
        .reasoning_state_by_output_index
        .get_mut(&output_index)
        .expect("validated reasoning item")
        .summary_parts
        .entry(summary_index)
        .or_default();
    if part.added {
        return Vec::new();
    }
    if !part.text.is_empty() || part.done_text.is_some() {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.reasoning_summary_part.added event arrived after summary text.",
            ),
        );
    }
    part.added = true;
    Vec::new()
}

fn handle_reasoning_summary_text_delta(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let (output_index, summary_index) =
        match validate_reasoning_event(event, state, "summary_index") {
            Ok(value) => value,
            Err(message) => {
                return terminate_responses_stream_with_error(state, build_error_event(message))
            }
        };
    let Some(delta_text) = get_str(event, "delta") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.reasoning_summary_text.delta event was missing its delta.",
            ),
        );
    };
    if let Err(message) = reserve_reasoning_part(state, output_index, summary_index, true) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    let already_done = state
        .reasoning_state_by_output_index
        .get(&output_index)
        .and_then(|reasoning| reasoning.summary_parts.get(&summary_index))
        .is_some_and(|part| part.done_text.is_some());
    if already_done {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.reasoning_summary_text.delta event arrived after its done event.",
            ),
        );
    }
    if let Err(message) = reserve_buffered_translation_bytes(state, delta_text.len()) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    let part = state
        .reasoning_state_by_output_index
        .get_mut(&output_index)
        .expect("validated reasoning item")
        .summary_parts
        .entry(summary_index)
        .or_default();
    part.text.push_str(delta_text);
    Vec::new()
}

fn handle_reasoning_summary_text_done(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let (output_index, summary_index) =
        match validate_reasoning_event(event, state, "summary_index") {
            Ok(value) => value,
            Err(message) => {
                return terminate_responses_stream_with_error(state, build_error_event(message))
            }
        };
    let Some(text) = get_str(event, "text") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.reasoning_summary_text.done event was missing its text."),
        );
    };
    if let Err(message) = reserve_reasoning_part(state, output_index, summary_index, true) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    let previous = state
        .reasoning_state_by_output_index
        .get(&output_index)
        .and_then(|reasoning| reasoning.summary_parts.get(&summary_index))
        .and_then(|part| part.done_text.clone());
    if let Some(previous) = previous {
        if previous == text {
            return Vec::new();
        }
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A reasoning summary part emitted conflicting text.done events."),
        );
    }
    if let Err(message) = reserve_buffered_translation_bytes(state, text.len()) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    // `text.done` is the authoritative complete text for this summary part.
    // Assign rather than append so prior deltas cannot duplicate content.
    let part = state
        .reasoning_state_by_output_index
        .get_mut(&output_index)
        .expect("validated reasoning item")
        .summary_parts
        .entry(summary_index)
        .or_default();
    part.text = text.to_string();
    part.done_text = Some(text.to_string());
    Vec::new()
}

fn handle_reasoning_text_delta(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let (output_index, content_index) =
        match validate_reasoning_event(event, state, "content_index") {
            Ok(value) => value,
            Err(message) => {
                return terminate_responses_stream_with_error(state, build_error_event(message))
            }
        };
    let Some(delta) = get_str(event, "delta") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.reasoning_text.delta event was missing its delta."),
        );
    };
    if let Err(message) = reserve_reasoning_part(state, output_index, content_index, false) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    if let Err(message) = reserve_buffered_translation_bytes(state, delta.len()) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    state
        .reasoning_state_by_output_index
        .get_mut(&output_index)
        .expect("validated reasoning item")
        .content_parts
        .entry(content_index)
        .or_default()
        .push_str(delta);
    Vec::new()
}

fn handle_output_text_done(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = match validate_active_output_item(event, state, &["message"]) {
        Ok(index) => index,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let content_index = match required_nonnegative_index(
        event,
        "content_index",
        "A response.output_text.done event was missing its content index.",
        "A response.output_text.done event contained a negative content index.",
    ) {
        Ok(index) => index,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let Some(text) = get_str(event, "text") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.output_text.done event was missing its text."),
        );
    };
    if let Err(message) =
        reconcile_output_text(state, output_index, content_index, text, &mut events)
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }

    events
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsesTerminalKind {
    Completed,
    Incomplete,
}

impl ResponsesTerminalKind {
    fn from_event(event: &Value) -> Option<Self> {
        match event.get("type").and_then(Value::as_str) {
            Some("response.completed") => Some(Self::Completed),
            Some("response.incomplete") => Some(Self::Incomplete),
            _ => None,
        }
    }

    fn expected_status(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
        }
    }
}

fn handle_response_completed(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let Some(terminal_kind) = ResponsesTerminalKind::from_event(event) else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("An unrecognized Responses event reached terminal handling."),
        );
    };

    let empty_response = json!({});
    let response = match event.get("response") {
        Some(response) if response.is_object() => response,
        None if terminal_kind == ResponsesTerminalKind::Incomplete => &empty_response,
        _ => {
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "A terminal Responses event contained an invalid response object.",
                ),
            )
        }
    };

    if terminal_kind == ResponsesTerminalKind::Completed
        && response.get("id").and_then(Value::as_str).is_none()
    {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.completed event was missing its response id."),
        );
    }

    if let Some(status) = response.get("status") {
        if status.as_str() != Some(terminal_kind.expected_status()) {
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "A terminal Responses event had an inconsistent response status.",
                ),
            );
        }
    }

    let mut events = Vec::new();

    let pending_output_item = state.output_items_by_index.values().any(|item| !item.done);
    let pending_function_call = state
        .function_call_state_by_output_index
        .values()
        .any(|call| !call.done);
    let terminal_output_mismatch = match response.get("output") {
        None => false,
        Some(Value::Array(output)) => {
            output.len() != state.output_items_by_index.len()
                || output.iter().enumerate().any(|(index, item)| {
                    state
                        .output_items_by_index
                        .get(&(index as i64))
                        .is_none_or(|lifecycle| lifecycle.done_item.as_ref() != Some(item))
                })
        }
        Some(_) => true,
    };
    if pending_output_item
        || pending_function_call
        || !state.reasoning_state_by_output_index.is_empty()
        || terminal_output_mismatch
    {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "The Responses stream terminated before all output items emitted response.output_item.done.",
            ),
        );
    }

    let stop_reason = match map_responses_stop_reason(response, state.has_tool_call, terminal_kind)
    {
        Ok(stop_reason) => stop_reason,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let usage = map_responses_usage_delta(response);

    finish_all_function_calls(state, &mut events);
    close_all_open_blocks(state, &mut events);

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

    terminate_responses_stream_with_error(state, build_error_event(message))
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

/// Maps the terminal event discriminator to Anthropic stop semantics. Codex
/// 0.144.1 does not model `response.status` on `ResponseCompleted`, so the SSE
/// event type is authoritative and status is only a consistency check.
fn map_responses_stop_reason(
    response: &Value,
    has_tool_call: bool,
    terminal_kind: ResponsesTerminalKind,
) -> Result<Option<String>, &'static str> {
    if terminal_kind == ResponsesTerminalKind::Completed {
        let output = response.get("output").and_then(Value::as_array);
        match output {
            None => {
                return Ok(Some(
                    if has_tool_call {
                        "tool_use"
                    } else {
                        "end_turn"
                    }
                    .to_string(),
                ));
            }
            Some(items) if items.is_empty() => {
                return Ok(Some(
                    if has_tool_call {
                        "tool_use"
                    } else {
                        "end_turn"
                    }
                    .to_string(),
                ));
            }
            Some(items) => {
                let has_call = items.iter().any(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call") | Some("tool_search_call")
                    )
                });
                return Ok(Some(
                    if has_call { "tool_use" } else { "end_turn" }.to_string(),
                ));
            }
        }
    }

    let reason = response
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str);
    match reason {
        Some("max_output_tokens") => Ok(Some("max_tokens".to_string())),
        Some("content_filter") => Ok(Some("refusal".to_string())),
        Some(_) | None => {
            Err("The Responses stream ended incomplete without a supported truncation reason.")
        }
    }
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
        input_tokens: Some((input_tokens - cached.unwrap_or(0)).max(0)),
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
    use crate::routes::messages::responses_translation::REASONING_SUMMARY_SEPARATOR;

    fn started_state() -> ResponsesStreamState {
        let mut state = create_responses_stream_state(None);
        let events = translate_responses_stream_event(
            &json!({
                "type": "response.created",
                "response": {"id": "resp_test", "model": "gpt-5"}
            }),
            &mut state,
        );
        assert!(matches!(
            events.as_slice(),
            [AnthropicStreamEventData::MessageStart { .. }]
        ));
        state
    }

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
    fn idless_compaction_stream_emits_decodable_carrier() {
        let mut state = started_state();
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"compaction",
                    "encrypted_content":"enc_stream_idless"
                }
            }),
            &mut state,
        );
        let signature = events.iter().find_map(|event| match event {
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::SignatureDelta { signature },
                ..
            } => Some(signature.as_str()),
            _ => None,
        });
        assert_eq!(signature, Some("cm1#enc_stream_idless@"));
    }

    #[test]
    fn optional_reasoning_stream_uses_versioned_carrier() {
        let mut state = started_state();
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"reasoning",
                    "encrypted_content":"enc_without_id",
                    "summary":[]
                }
            }),
            &mut state,
        );
        let signature = events.iter().find_map(|event| match event {
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::SignatureDelta { signature },
                ..
            } => Some(signature.as_str()),
            _ => None,
        });
        assert!(signature.is_some_and(|value| {
            value.starts_with("rs1#")
                && value.contains("\"encrypted_content\":\"enc_without_id\"")
                && value.contains("\"id\":null")
        }));
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
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{
                    "type":"message",
                    "id":"message-1",
                    "role":"assistant",
                    "content":[]
                }
            }),
            &mut state,
        )
        .is_empty());

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
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"message",
                    "id":"message-1",
                    "role":"assistant",
                    "content":[{"type":"output_text","text":"Hello world"}]
                }
            }),
            &mut state,
        )
        .is_empty());

        // terminal event closes the open block and finishes the message
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "output": [{
                    "type":"message",
                    "id":"message-1",
                    "role":"assistant",
                    "content":[{"type":"output_text","text":"Hello world"}]
                }],
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
        let mut state = started_state();

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
        let mut state = started_state();
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
    fn parallel_function_calls_keep_anthropic_blocks_sequential() {
        let mut state = started_state();
        let mut all = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "type": "function_call", "call_id": "c0",
                    "name": "first", "arguments": ""
                }
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {
                    "type": "function_call", "call_id": "c1",
                    "name": "second", "arguments": ""
                }
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 1,
                "delta": "{\"b\":"
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"a\":1}"
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 1,
                "arguments": "{\"b\":2}"
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "arguments": "{\"a\":1}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call", "call_id": "c0",
                    "name": "first", "arguments": "{\"a\":1}"
                }
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {
                    "type": "function_call", "call_id": "c1",
                    "name": "second", "arguments": "{\"b\":2}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_test",
                    "status": "completed",
                    "output": [
                        {
                            "type": "function_call", "call_id": "c0",
                            "name": "first", "arguments": "{\"a\":1}"
                        },
                        {
                            "type": "function_call", "call_id": "c1",
                            "name": "second", "arguments": "{\"b\":2}"
                        }
                    ],
                    "usage": {"input_tokens": 3, "output_tokens": 2}
                }
            }),
        ] {
            all.extend(
                translate_responses_stream_event(&event, &mut state)
                    .into_iter()
                    .map(|event| serde_json::to_value(event).unwrap()),
            );
        }

        let mut open = None;
        for event in &all {
            match event["type"].as_str() {
                Some("content_block_start") => {
                    assert!(open.is_none(), "started a block while {open:?} was open");
                    open = event["index"].as_i64();
                }
                Some("content_block_delta") => {
                    assert_eq!(open, event["index"].as_i64());
                }
                Some("content_block_stop") => {
                    assert_eq!(open, event["index"].as_i64());
                    open = None;
                }
                _ => {}
            }
        }
        assert!(open.is_none());

        let starts: Vec<_> = all
            .iter()
            .filter(|event| event["type"] == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["content_block"]["id"], "c0");
        assert_eq!(starts[1]["content_block"]["id"], "c1");
        assert_eq!(all.last().unwrap()["type"], "message_stop");
    }

    #[test]
    fn content_filter_incomplete_maps_to_refusal() {
        let mut state = started_state();
        let event = json!({
            "type": "response.incomplete",
            "response": {
                "status": "incomplete",
                "incomplete_details": {"reason": "content_filter"},
                "usage": {"input_tokens": 1, "output_tokens": 0}
            }
        });
        let events = translate_responses_stream_event(&event, &mut state);
        let delta = events
            .iter()
            .find_map(|event| match event {
                AnthropicStreamEventData::MessageDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .unwrap();
        assert_eq!(delta.stop_reason.as_deref(), Some("refusal"));
    }

    #[test]
    fn reasoning_done_emits_default_thinking_and_signature() {
        let mut state = started_state();
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

    #[test]
    fn aggregate_empty_reasoning_stream_uses_placeholder_only_with_carrier() {
        for summary in [
            json!([]),
            json!([{"type":"summary_text","text":""}]),
            json!([
                {"type":"summary_text","text":" \n\t"},
                {"type":"summary_text","text":""}
            ]),
        ] {
            let mut state = started_state();
            let events = translate_responses_stream_event(
                &json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{
                        "type":"reasoning",
                        "id":"reasoning-id",
                        "encrypted_content":"encrypted",
                        "summary":summary.clone()
                    }
                }),
                &mut state,
            );
            assert!(events.iter().any(|event| matches!(
                event,
                AnthropicStreamEventData::ContentBlockDelta {
                    delta: AnthropicContentBlockDelta::ThinkingDelta { thinking },
                    ..
                } if thinking == THINKING_TEXT
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                AnthropicStreamEventData::ContentBlockDelta {
                    delta: AnthropicContentBlockDelta::SignatureDelta { signature },
                    ..
                } if signature == "encrypted@reasoning-id"
            )));

            let mut state = started_state();
            let carrier_free = translate_responses_stream_event(
                &json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{"type":"reasoning","summary":summary}
                }),
                &mut state,
            );
            assert!(
                carrier_free.is_empty(),
                "carrier-free aggregate-empty reasoning emitted {carrier_free:?}"
            );
        }

        for (encrypted_content, id) in [(Some(""), None), (None, Some("")), (Some(""), Some(""))] {
            let mut item = json!({
                "type":"reasoning",
                "summary":[{"type":"summary_text","text":" \n"}]
            });
            if let Some(encrypted_content) = encrypted_content {
                item["encrypted_content"] = json!(encrypted_content);
            }
            if let Some(id) = id {
                item["id"] = json!(id);
            }
            let mut state = started_state();
            let events = translate_responses_stream_event(
                &json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":item
                }),
                &mut state,
            );
            assert!(events.iter().any(|event| matches!(
                event,
                AnthropicStreamEventData::ContentBlockDelta {
                    delta: AnthropicContentBlockDelta::ThinkingDelta { thinking },
                    ..
                } if thinking == THINKING_TEXT
            )));
            let expected = encode_reasoning_signature(encrypted_content, id);
            assert!(events.iter().any(|event| matches!(
                event,
                AnthropicStreamEventData::ContentBlockDelta {
                    delta: AnthropicContentBlockDelta::SignatureDelta { signature },
                    ..
                } if signature == &expected
            )));
        }
    }

    #[test]
    fn reasoning_stream_framing_preserves_leading_and_trailing_whitespace() {
        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"reasoning-id","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        for delta in ["  ", "analysis", "  "] {
            let events = translate_responses_stream_event(
                &json!({
                    "type":"response.reasoning_summary_text.delta",
                    "output_index":0,
                    "summary_index":0,
                    "delta":delta
                }),
                &mut state,
            );
            assert!(events.is_empty());
        }
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"reasoning",
                    "id":"reasoning-id",
                    "encrypted_content":"encrypted",
                    "summary":[{"type":"summary_text","text":"  analysis  "}]
                }
            }),
            &mut state,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::ThinkingDelta { thinking },
                ..
            } if thinking == "  analysis  "
        )));
    }

    #[test]
    fn reasoning_summary_part_boundaries_are_ordered_and_not_duplicated() {
        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"reasoning-id","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        for event in [
            json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"one"}),
            json!({"type":"response.reasoning_summary_text.done","output_index":0,"summary_index":0,"text":"one"}),
            json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1}),
            json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"two"}),
            json!({"type":"response.reasoning_summary_text.done","output_index":0,"summary_index":1,"text":"two"}),
        ] {
            assert!(translate_responses_stream_event(&event, &mut state).is_empty());
        }
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"reasoning",
                    "id":"reasoning-id",
                    "encrypted_content":"encrypted"
                }
            }),
            &mut state,
        );
        let thinking = events.iter().find_map(|event| match event {
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::ThinkingDelta { thinking },
                ..
            } => Some(thinking.as_str()),
            _ => None,
        });
        let expected = format!("one{REASONING_SUMMARY_SEPARATOR}two");
        assert_eq!(thinking, Some(expected.as_str()));
    }

    #[test]
    fn reasoning_content_deltas_and_summary_render_losslessly_at_item_done() {
        let mut state = started_state();
        let added = json!({
            "type":"response.output_item.added",
            "sequence_number":1,
            "output_index":0,
            "item":{"type":"reasoning","id":"reasoning-content","summary":[]}
        });
        assert!(translate_responses_stream_event(&added, &mut state).is_empty());
        let replayed_content_delta = json!({
            "type":"response.reasoning_text.delta",
            "sequence_number":3,
            "output_index":0,
            "content_index":0,
            "delta":" raw"
        });
        for event in [
            json!({"type":"response.reasoning_summary_text.delta","sequence_number":2,"output_index":0,"summary_index":0,"delta":" summary "}),
            replayed_content_delta.clone(),
            replayed_content_delta,
            json!({"type":"response.reasoning_text.delta","sequence_number":4,"output_index":0,"content_index":0,"delta":" content "}),
            json!({"type":"response.reasoning_text.delta","sequence_number":5,"output_index":0,"content_index":1,"delta":"second"}),
        ] {
            assert!(translate_responses_stream_event(&event, &mut state).is_empty());
        }

        let done_item = json!({
            "type":"reasoning",
            "id":"reasoning-content",
            "encrypted_content":"encrypted-content",
            "summary":[{"type":"summary_text","text":" summary "}]
        });
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "sequence_number":6,
                "output_index":0,
                "item":done_item
            }),
            &mut state,
        );
        let thinking = events.iter().find_map(|event| match event {
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::ThinkingDelta { thinking },
                ..
            } => Some(thinking.as_str()),
            _ => None,
        });
        let expected = [" summary ", " raw content ", "second"].join(REASONING_SUMMARY_SEPARATOR);
        assert_eq!(thinking, Some(expected.as_str()));
        assert!(events.iter().any(|event| matches!(
            event,
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::SignatureDelta { signature },
                ..
            } if signature == "encrypted-content@reasoning-content"
        )));
    }

    #[test]
    fn completion_with_pending_reasoning_errors_once_and_never_stops() {
        let mut state = started_state();
        for event in [
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"pending","summary":[]}
            }),
            json!({
                "type":"response.reasoning_text.delta",
                "output_index":0,
                "content_index":0,
                "delta":"not finished"
            }),
        ] {
            assert!(translate_responses_stream_event(&event, &mut state).is_empty());
        }
        let completed = json!({
            "type":"response.completed",
            "response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}
        });
        let events = translate_responses_stream_event(&completed, &mut state);
        assert!(matches!(
            events.as_slice(),
            [AnthropicStreamEventData::Error { .. }]
        ));
        assert!(!events
            .iter()
            .any(|event| matches!(event, AnthropicStreamEventData::MessageStop)));
        assert!(translate_responses_stream_event(&completed, &mut state).is_empty());
    }

    #[test]
    fn identical_done_and_added_replays_are_deduplicated() {
        let mut state = started_state();
        let added = json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"reasoning","id":"dedupe","summary":[]}
        });
        assert!(translate_responses_stream_event(&added, &mut state).is_empty());
        assert!(translate_responses_stream_event(&added, &mut state).is_empty());

        let done = json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":{
                "type":"reasoning",
                "id":"dedupe",
                "encrypted_content":"opaque",
                "summary":[{"type":"summary_text","text":"once"}]
            }
        });
        let first = translate_responses_stream_event(&done, &mut state);
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(
                    event,
                    AnthropicStreamEventData::ContentBlockDelta {
                        delta: AnthropicContentBlockDelta::ThinkingDelta { .. },
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(translate_responses_stream_event(&done, &mut state).is_empty());

        let terminal = translate_responses_stream_event(
            &json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_test",
                    "status":"completed",
                    "output":[{
                        "type":"reasoning",
                        "id":"dedupe",
                        "encrypted_content":"opaque",
                        "summary":[{"type":"summary_text","text":"once"}]
                    }],
                    "usage":{}
                }
            }),
            &mut state,
        );
        assert!(matches!(
            terminal.last(),
            Some(AnthropicStreamEventData::MessageStop)
        ));
    }

    #[test]
    fn conflicting_done_and_out_of_order_reasoning_events_fail_closed() {
        let mut state = started_state();
        assert!(matches!(
            translate_responses_stream_event(
                &json!({
                    "type":"response.reasoning_summary_text.delta",
                    "output_index":0,
                    "summary_index":0,
                    "delta":"early"
                }),
                &mut state,
            )
            .as_slice(),
            [AnthropicStreamEventData::Error { .. }]
        ));

        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"conflict","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        let first_done = json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":{
                "type":"reasoning",
                "id":"conflict",
                "summary":[{"type":"summary_text","text":"first"}]
            }
        });
        assert!(!translate_responses_stream_event(&first_done, &mut state).is_empty());
        let conflicting = json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":{
                "type":"reasoning",
                "id":"conflict",
                "summary":[{"type":"summary_text","text":"different"}]
            }
        });
        let events = translate_responses_stream_event(&conflicting, &mut state);
        assert!(events
            .iter()
            .any(|event| matches!(event, AnthropicStreamEventData::Error { .. })));
        assert!(!events
            .iter()
            .any(|event| matches!(event, AnthropicStreamEventData::MessageStop)));
    }

    #[test]
    fn summary_done_without_part_added_is_valid_but_delta_after_done_is_not() {
        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"summary-done","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        let done = json!({
            "type":"response.reasoning_summary_text.done",
            "item_id":"summary-done",
            "output_index":0,
            "summary_index":0,
            "text":"authoritative"
        });
        assert!(translate_responses_stream_event(&done, &mut state).is_empty());
        assert!(translate_responses_stream_event(&done, &mut state).is_empty());
        assert!(matches!(
            translate_responses_stream_event(
                &json!({
                    "type":"response.reasoning_summary_text.delta",
                    "item_id":"summary-done",
                    "output_index":0,
                    "summary_index":0,
                    "delta":"late"
                }),
                &mut state,
            )
            .last(),
            Some(AnthropicStreamEventData::Error { .. })
        ));
    }

    #[test]
    fn reasoning_buffer_and_part_limits_fail_closed_without_large_allocations() {
        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"bounded","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        state.buffered_translation_bytes = MAX_BUFFERED_TRANSLATION_BYTES;
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.reasoning_text.delta",
                "output_index":0,
                "content_index":0,
                "delta":"x"
            }),
            &mut state,
        );
        assert!(matches!(
            events.last(),
            Some(AnthropicStreamEventData::Error { .. })
        ));

        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"bounded-parts","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        state.tracked_reasoning_parts = MAX_TRACKED_CONTENT_PARTS;
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.reasoning_summary_part.added",
                "output_index":0,
                "summary_index":0
            }),
            &mut state,
        );
        assert!(matches!(
            events.last(),
            Some(AnthropicStreamEventData::Error { .. })
        ));
    }

    #[test]
    fn completion_before_created_terminates_with_error() {
        let mut state = create_responses_stream_state(None);
        let events = translate_responses_stream_event(
            &json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            }),
            &mut state,
        );

        assert!(matches!(
            events.as_slice(),
            [AnthropicStreamEventData::Error { .. }]
        ));
        assert!(state.message_completed);
        assert!(!state.message_start_sent);
    }

    #[test]
    fn duplicate_created_terminates_with_error_without_second_start() {
        let mut state = started_state();
        let events = translate_responses_stream_event(
            &json!({
                "type": "response.created",
                "response": {"id": "resp_duplicate", "model": "gpt-5"}
            }),
            &mut state,
        );

        assert!(matches!(
            events.as_slice(),
            [AnthropicStreamEventData::Error { .. }]
        ));
        assert!(state.message_completed);
    }
}
