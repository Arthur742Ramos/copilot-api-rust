//! OpenAI ChatCompletions streaming chunk -> Anthropic SSE event state machine.
//!
//! Mirrors `src/routes/messages/stream-translation.ts`. Each incoming OpenAI
//! streaming delta (`chunk`) is translated into an ordered list of Anthropic
//! `AnthropicStreamEventData` events. The translation is stateful: per-stream
//! progress lives in [`AnthropicStreamState`] (message_start emitted once, the
//! single currently-open content block, the thinking block, accumulated tool
//! calls, deferred text, and a pending `message_delta`).
//!
//! Chunks are dynamic (`content` is `string | null`, `tool_calls` is a sparse
//! array, etc.) so we accept the chunk as `&serde_json::Value` and project the
//! one delta we care about into a small mutable [`DeltaView`], reproducing the
//! TS `delta.content = …` mutation in `handleThinkingText`.

use serde_json::Value;

use super::anthropic_types::{
    AnthropicContentBlockDelta, AnthropicMessageDeltaBody, AnthropicMessageDeltaUsage,
    AnthropicMessageStart, AnthropicStreamEventData, AnthropicStreamState, AnthropicStreamToolCall,
    AnthropicUsage,
};
use super::utils::map_openai_stop_reason_to_anthropic;

/// Re-exported from [`super::utils`] so all translation modules share one
/// source of truth for the user-visible "Thinking..." placeholder. Compatible
/// with opencode, which filters out thinking blocks whose text is empty, so a
/// non-empty default is emitted.
use super::utils::THINKING_TEXT;

// ---------------------------------------------------------------------------
// Delta projection
// ---------------------------------------------------------------------------

/// The fields of a single OpenAI streaming `delta` that the state machine
/// consumes. Mutable so `handle_thinking_text` can rewrite `content` and clear
/// the reasoning text fields, exactly like the TS in-place mutation.
struct DeltaView {
    content: Option<String>,
    reasoning_text: Option<String>,
    reasoning_content: Option<String>,
    reasoning_opaque: Option<String>,
    tool_calls: Vec<Value>,
}

impl DeltaView {
    fn from_delta(delta: &Value) -> Self {
        Self {
            content: opt_string(delta.get("content")),
            reasoning_text: opt_string(delta.get("reasoning_text")),
            reasoning_content: opt_string(delta.get("reasoning_content")),
            reasoning_opaque: opt_string(delta.get("reasoning_opaque")),
            tool_calls: delta
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// JS `delta.content && delta.content.length > 0`.
    fn has_content(&self) -> bool {
        self.content.as_deref().is_some_and(|c| !c.is_empty())
    }

    /// JS `delta.tool_calls && delta.tool_calls.length > 0`.
    fn has_tool_call_delta(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// `string | null | undefined` -> `Option<String>`. Empty string stays `Some("")`
/// so callers can distinguish `=== ""` from `null`/absent.
fn opt_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// `translateChunkToAnthropicEvents` — translate one OpenAI streaming chunk into
/// the ordered Anthropic events it produces, mutating `state` as it goes.
pub fn translate_chunk_to_anthropic_events(
    chunk: &Value,
    state: &mut AnthropicStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events: Vec<AnthropicStreamEventData> = Vec::new();

    let choices = chunk.get("choices").and_then(|v| v.as_array());
    let choice = match choices {
        Some(arr) if !arr.is_empty() => &arr[0],
        _ => {
            // `chunk.choices.length === 0`
            complete_pending_message(state, &mut events, Some(chunk));
            return events;
        }
    };

    let delta_value = choice.get("delta").cloned().unwrap_or(Value::Null);
    let mut delta = DeltaView::from_delta(&delta_value);

    handle_message_start(state, &mut events, chunk);

    handle_thinking_text(&mut delta, state, &mut events);

    handle_content(&delta, state, &mut events);

    handle_tool_calls(&delta, state, &mut events);

    handle_finish(choice, &delta, state, &mut events, chunk);

    events
}

/// `flushPendingAnthropicStreamEvents` — emit any queued `message_delta` plus
/// `message_stop` when the upstream stream ends without a usage-bearing chunk.
///
/// Also handles the degenerate case where the upstream ended (clean end, a
/// `[DONE]` with no prior `finish_reason`, or a finishing chunk that failed to
/// parse and was dropped) WITHOUT ever queueing a `message_delta`: a content
/// block may still be open and no `message_stop` was emitted, which leaves a
/// client like Claude Code waiting forever. In that case we synthesize a
/// well-formed terminal close (close any open block, then `message_delta` +
/// `message_stop`) so every stream is terminated.
pub fn flush_pending_anthropic_stream_events(
    state: &mut AnthropicStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events: Vec<AnthropicStreamEventData> = Vec::new();

    if state.pending_message_delta.is_some() {
        complete_pending_message(state, &mut events, None);
        return events;
    }

    // Nothing queued. If we already emitted a terminal message_stop the stream is
    // well-formed and there is nothing to do. Likewise, if message_start was never
    // sent there is no open message to close — emitting a bare message_delta would
    // itself be malformed, so leave the (empty) stream as-is.
    if state.message_stop_emitted || !state.message_start_sent {
        return events;
    }

    // Upstream ended without a finishing chunk. Close any block left open
    // (thinking or content/tool) so the stream stays well-formed, then synthesize
    // the terminal message_delta + message_stop.
    close_thinking_block_if_open(state, &mut events);
    if state.content_block_open {
        events.push(AnthropicStreamEventData::ContentBlockStop {
            index: state.content_block_index,
        });
        state.content_block_open = false;
        state.content_block_index += 1;
    }

    events.push(AnthropicStreamEventData::MessageDelta {
        delta: AnthropicMessageDeltaBody {
            // No finish_reason was delivered; "end_turn" is the safe Anthropic
            // default for a normally-terminated turn.
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
        },
        usage: None,
    });
    events.push(AnthropicStreamEventData::MessageStop);
    state.message_stop_emitted = true;

    events
}

/// `translateErrorToAnthropicErrorEvent` — the terminal `error` event.
pub fn translate_error_to_anthropic_error_event() -> AnthropicStreamEventData {
    AnthropicStreamEventData::Error {
        error: super::anthropic_types::AnthropicErrorBody {
            kind: "api_error".to_string(),
            message: "An unexpected error occurred during streaming.".to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// State-machine steps (mirroring the TS private functions, push-order exact)
// ---------------------------------------------------------------------------

/// `isToolBlockOpen` — is the currently-open block a tool_use block?
fn is_tool_block_open(state: &AnthropicStreamState) -> bool {
    if !state.content_block_open {
        return false;
    }
    state
        .tool_calls
        .values()
        .any(|tc| tc.anthropic_block_index == state.content_block_index)
}

/// `completePendingMessage` — flush the queued `message_delta` then `message_stop`.
fn complete_pending_message(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
    chunk: Option<&Value>,
) {
    let Some(pending) = state.pending_message_delta.take() else {
        return;
    };

    // `if (chunk?.usage)` -> refresh the usage on the pending delta.
    let pending = match (chunk, pending) {
        (Some(c), AnthropicStreamEventData::MessageDelta { delta, usage: _ }) if has_usage(c) => {
            AnthropicStreamEventData::MessageDelta {
                delta,
                usage: Some(get_anthropic_usage_from_openai_chunk(c)),
            }
        }
        (_, pending) => pending,
    };

    events.push(pending);
    events.push(AnthropicStreamEventData::MessageStop);
    state.message_stop_emitted = true;
}

/// `handleFinish` — on a finishing chunk, close the open block, flush deferred
/// text, queue the `message_delta`, and (if usage is present) complete it.
fn handle_finish(
    choice: &Value,
    delta: &DeltaView,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
    chunk: &Value,
) {
    let finish_reason = opt_string(choice.get("finish_reason"));
    let Some(finish_reason) = finish_reason.filter(|s| !s.is_empty()) else {
        return;
    };

    // Already terminated: a well-behaved upstream sends one finishing chunk, but
    // an aggregator could send a second usage-bearing one. Ignore it so we never
    // emit a second message_delta/message_stop after the terminal stop.
    if state.message_stop_emitted {
        return;
    }

    // A reasoning-only turn leaves a `content_block_start{thinking}` open with no
    // matching content block; close it here so the stream stays well-formed.
    close_thinking_block_if_open(state, events);

    if state.content_block_open {
        let tool_block_open = is_tool_block_open(state);
        events.push(AnthropicStreamEventData::ContentBlockStop {
            index: state.content_block_index,
        });
        state.content_block_open = false;
        state.content_block_index += 1;
        if !tool_block_open {
            handle_reasoning_opaque(delta, events, state);
        }
    }

    flush_deferred_content(state, events);

    state.pending_message_delta = Some(AnthropicStreamEventData::MessageDelta {
        delta: AnthropicMessageDeltaBody {
            stop_reason: map_openai_stop_reason_to_anthropic(Some(finish_reason.as_str()))
                .map(|s| s.to_string()),
            stop_sequence: None,
        },
        usage: Some(get_anthropic_usage_from_openai_chunk(chunk)),
    });

    if has_usage(chunk) {
        complete_pending_message(state, events, Some(chunk));
    }
}

/// `getAnthropicUsageFromOpenAIChunk`.
fn get_anthropic_usage_from_openai_chunk(chunk: &Value) -> AnthropicMessageDeltaUsage {
    let (cache_creation_tokens, cached_tokens, input_tokens) = get_openai_chunk_usage_tokens(chunk);

    AnthropicMessageDeltaUsage {
        input_tokens: Some(input_tokens),
        output_tokens: usage_num(chunk, &["usage", "completion_tokens"]),
        cache_creation_input_tokens: usage_field_present(chunk, "cache_creation_input_tokens")
            .then_some(cache_creation_tokens),
        cache_read_input_tokens: usage_field_present(chunk, "cached_tokens")
            .then_some(cached_tokens),
    }
}

/// `getOpenAIChunkUsageTokens` -> (cacheCreationTokens, cachedTokens, inputTokens).
fn get_openai_chunk_usage_tokens(chunk: &Value) -> (i64, i64, i64) {
    let prompt_tokens = usage_num(chunk, &["usage", "prompt_tokens"]);
    let cached_tokens = usage_num(chunk, &["usage", "prompt_tokens_details", "cached_tokens"]);
    let cache_creation_tokens = usage_num(
        chunk,
        &[
            "usage",
            "prompt_tokens_details",
            "cache_creation_input_tokens",
        ],
    );

    (
        cache_creation_tokens,
        cached_tokens,
        (prompt_tokens - cached_tokens - cache_creation_tokens).max(0),
    )
}

/// `handleToolCalls`.
fn handle_tool_calls(
    delta: &DeltaView,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    if !delta.has_tool_call_delta() {
        return;
    }

    close_thinking_block_if_open(state, events);

    handle_reasoning_opaque_in_tool_calls(state, events, delta);

    for tool_call in &delta.tool_calls {
        let index = tool_call.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        let id = tool_call
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let name = tool_call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        if let (Some(id), Some(name)) = (id, name) {
            // New tool call starting.
            if state.content_block_open {
                // Close any previously open block.
                events.push(AnthropicStreamEventData::ContentBlockStop {
                    index: state.content_block_index,
                });
                state.content_block_index += 1;
                state.content_block_open = false;
            }

            let anthropic_block_index = state.content_block_index;
            state.tool_calls.insert(
                index,
                AnthropicStreamToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    anthropic_block_index,
                },
            );

            events.push(AnthropicStreamEventData::ContentBlockStart {
                index: anthropic_block_index,
                content_block: serde_json::json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": {},
                }),
            });
            state.content_block_open = true;
        }

        let arguments = tool_call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        if let Some(arguments) = arguments {
            if let Some(info) = state.tool_calls.get(&index) {
                events.push(AnthropicStreamEventData::ContentBlockDelta {
                    index: info.anthropic_block_index,
                    delta: AnthropicContentBlockDelta::InputJsonDelta {
                        partial_json: arguments.to_string(),
                    },
                });
            }
        }
    }
}

/// `handleReasoningOpaqueInToolCalls`.
fn handle_reasoning_opaque_in_tool_calls(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
    delta: &DeltaView,
) {
    if state.content_block_open && !is_tool_block_open(state) {
        events.push(AnthropicStreamEventData::ContentBlockStop {
            index: state.content_block_index,
        });
        state.content_block_index += 1;
        state.content_block_open = false;
    }
    handle_reasoning_opaque(delta, events, state);
}

/// `handleContent`.
fn handle_content(
    delta: &DeltaView,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    if delta.has_content() {
        close_thinking_block_if_open(state, events);

        let content = delta.content.clone().unwrap_or_default();

        if is_tool_block_open(state) || delta.has_tool_call_delta() {
            let mut deferred = state.deferred_content.take().unwrap_or_default();
            deferred.push_str(&content);
            state.deferred_content = Some(deferred);
            return;
        }

        if !state.content_block_open {
            events.push(AnthropicStreamEventData::ContentBlockStart {
                index: state.content_block_index,
                content_block: serde_json::json!({ "type": "text", "text": "" }),
            });
            state.content_block_open = true;
        }

        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: state.content_block_index,
            delta: AnthropicContentBlockDelta::TextDelta { text: content },
        });
    }

    // handle for claude model
    if delta.content.as_deref() == Some("")
        && delta
            .reasoning_opaque
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        && state.thinking_block_open
    {
        let signature = delta.reasoning_opaque.clone().unwrap_or_default();
        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: state.content_block_index,
            delta: AnthropicContentBlockDelta::SignatureDelta { signature },
        });
        events.push(AnthropicStreamEventData::ContentBlockStop {
            index: state.content_block_index,
        });
        state.content_block_index += 1;
        state.thinking_block_open = false;
    }
}

/// `flushDeferredContent`.
fn flush_deferred_content(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    // `if (!state.deferredContent)` — empty string is falsy and skips.
    let deferred = match state.deferred_content.take() {
        Some(s) if !s.is_empty() => s,
        _ => {
            // Restore a non-flushed-but-present empty string? TS leaves it as-is;
            // an empty/absent value is indistinguishable downstream, so drop it.
            return;
        }
    };

    if !state.content_block_open {
        events.push(AnthropicStreamEventData::ContentBlockStart {
            index: state.content_block_index,
            content_block: serde_json::json!({ "type": "text", "text": "" }),
        });
        state.content_block_open = true;
    }

    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: state.content_block_index,
        delta: AnthropicContentBlockDelta::TextDelta { text: deferred },
    });
    events.push(AnthropicStreamEventData::ContentBlockStop {
        index: state.content_block_index,
    });
    state.deferred_content = None;
    state.content_block_open = false;
    state.content_block_index += 1;
}

/// `handleMessageStart`.
fn handle_message_start(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
    chunk: &Value,
) {
    if state.message_start_sent {
        return;
    }

    let (cache_creation_tokens, cached_tokens, input_tokens) = get_openai_chunk_usage_tokens(chunk);

    let message = AnthropicMessageStart {
        id: chunk
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        kind: "message".to_string(),
        role: "assistant".to_string(),
        content: Vec::new(),
        model: chunk
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        stop_reason: None,
        stop_sequence: None,
        usage: AnthropicUsage {
            input_tokens,
            output_tokens: 0, // Will be updated in message_delta when finished
            cache_creation_input_tokens: usage_field_present(chunk, "cache_creation_input_tokens")
                .then_some(cache_creation_tokens),
            cache_read_input_tokens: usage_field_present(chunk, "cached_tokens")
                .then_some(cached_tokens),
            service_tier: None,
            extra: serde_json::Map::new(),
        },
    };

    events.push(AnthropicStreamEventData::MessageStart { message });
    state.message_start_sent = true;
}

/// `handleReasoningOpaque` — emit a complete thinking block (start, default
/// thinking_delta, signature_delta, stop) for an opaque reasoning blob.
fn handle_reasoning_opaque(
    delta: &DeltaView,
    events: &mut Vec<AnthropicStreamEventData>,
    state: &mut AnthropicStreamState,
) {
    let Some(signature) = delta.reasoning_opaque.as_deref().filter(|s| !s.is_empty()) else {
        return;
    };

    events.push(AnthropicStreamEventData::ContentBlockStart {
        index: state.content_block_index,
        content_block: serde_json::json!({ "type": "thinking", "thinking": "" }),
    });
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: state.content_block_index,
        delta: AnthropicContentBlockDelta::ThinkingDelta {
            thinking: THINKING_TEXT.to_string(),
        },
    });
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: state.content_block_index,
        delta: AnthropicContentBlockDelta::SignatureDelta {
            signature: signature.to_string(),
        },
    });
    events.push(AnthropicStreamEventData::ContentBlockStop {
        index: state.content_block_index,
    });
    state.content_block_index += 1;
}

/// `handleThinkingText`.
fn handle_thinking_text(
    delta: &mut DeltaView,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    // `delta.reasoning_text ?? delta.reasoning_content`
    let reasoning_text = delta
        .reasoning_text
        .clone()
        .or_else(|| delta.reasoning_content.clone());

    let Some(reasoning_text) = reasoning_text.filter(|s| !s.is_empty()) else {
        return;
    };

    // compatible with copilot API returning content->reasoning_text->reasoning_opaque
    // in different deltas; abnormal claude-model server behaviour.
    if state.content_block_open {
        delta.content = Some(reasoning_text);
        delta.reasoning_text = None;
        delta.reasoning_content = None;
        return;
    }

    if !state.thinking_block_open {
        events.push(AnthropicStreamEventData::ContentBlockStart {
            index: state.content_block_index,
            content_block: serde_json::json!({ "type": "thinking", "thinking": "" }),
        });
        state.thinking_block_open = true;
    }

    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: state.content_block_index,
        delta: AnthropicContentBlockDelta::ThinkingDelta {
            thinking: reasoning_text,
        },
    });
}

/// `closeThinkingBlockIfOpen`.
fn close_thinking_block_if_open(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    if !state.thinking_block_open {
        return;
    }
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: state.content_block_index,
        delta: AnthropicContentBlockDelta::SignatureDelta {
            signature: String::new(),
        },
    });
    events.push(AnthropicStreamEventData::ContentBlockStop {
        index: state.content_block_index,
    });
    state.content_block_index += 1;
    state.thinking_block_open = false;
}

// ---------------------------------------------------------------------------
// Usage helpers over the dynamic chunk Value
// ---------------------------------------------------------------------------

/// `chunk?.usage` is present and truthy (an object).
fn has_usage(chunk: &Value) -> bool {
    chunk.get("usage").is_some_and(|u| !u.is_null())
}

/// Navigate `path` and read an integer with a `?? 0` default.
fn usage_num(chunk: &Value, path: &[&str]) -> i64 {
    let mut cur = chunk;
    for key in path {
        match cur.get(key) {
            Some(v) => cur = v,
            None => return 0,
        }
    }
    cur.as_i64()
        .or_else(|| cur.as_f64().map(|f| f as i64))
        .unwrap_or(0)
}

/// `chunk.usage?.prompt_tokens_details?.<key> !== undefined` — the key exists
/// (even if its value is `null`).
fn usage_field_present(chunk: &Value, key: &str) -> bool {
    chunk
        .get("usage")
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get(key))
        .is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn to_values(events: &[AnthropicStreamEventData]) -> Vec<Value> {
        events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    /// Verify start/stop balance: never two blocks open at once, every start is
    /// stopped exactly once across the full event stream.
    fn assert_single_open_block_invariant(all: &[Value]) {
        let mut open: Option<i64> = None;
        for ev in all {
            match ev.get("type").and_then(|t| t.as_str()) {
                Some("content_block_start") => {
                    assert!(
                        open.is_none(),
                        "two content blocks open simultaneously: {ev:?}"
                    );
                    open = ev.get("index").and_then(|i| i.as_i64());
                }
                Some("content_block_stop") => {
                    assert!(open.is_some(), "stop without an open block: {ev:?}");
                    assert_eq!(
                        open,
                        ev.get("index").and_then(|i| i.as_i64()),
                        "stop index mismatches the open block: {ev:?}"
                    );
                    open = None;
                }
                _ => {}
            }
        }
        assert!(open.is_none(), "stream ended with an open content block");
    }

    #[test]
    fn simple_text_delta_sequence() {
        let mut state = AnthropicStreamState::default();
        let mut all: Vec<Value> = Vec::new();

        let first = json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{ "index": 0, "delta": { "content": "Hello" }, "finish_reason": null }],
        });
        let ev = translate_chunk_to_anthropic_events(&first, &mut state);
        let got = to_values(&ev);
        all.extend(got.clone());

        assert_eq!(
            got,
            vec![
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "chatcmpl-1",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "gpt-4o",
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 }
                    }
                }),
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text", "text": "" }
                }),
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "Hello" }
                }),
            ]
        );

        let finish = json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 },
        });
        let ev = translate_chunk_to_anthropic_events(&finish, &mut state);
        let got = to_values(&ev);
        all.extend(got.clone());

        assert_eq!(
            got,
            vec![
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "input_tokens": 10, "output_tokens": 5 }
                }),
                json!({ "type": "message_stop" }),
            ]
        );

        assert_single_open_block_invariant(&all);
    }

    #[test]
    fn tool_call_input_json_accumulation() {
        let mut state = AnthropicStreamState::default();
        let mut all: Vec<Value> = Vec::new();

        // Chunk 1: tool call announced (id + name).
        let c1 = json!({
            "id": "chatcmpl-2",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "get_weather" }
                }] },
                "finish_reason": null
            }],
        });
        let ev = translate_chunk_to_anthropic_events(&c1, &mut state);
        let got = to_values(&ev);
        all.extend(got.clone());
        assert_eq!(
            got,
            vec![
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "chatcmpl-2",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "gpt-4o",
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 }
                    }
                }),
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": "call_1", "name": "get_weather", "input": {} }
                }),
            ]
        );

        // Chunk 2 & 3: argument fragments accumulate as input_json_delta.
        let c2 = json!({
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "{\"location\":" } }] },
                "finish_reason": null
            }],
        });
        let ev = translate_chunk_to_anthropic_events(&c2, &mut state);
        let got = to_values(&ev);
        all.extend(got.clone());
        assert_eq!(
            got,
            vec![json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": "{\"location\":" }
            })]
        );

        let c3 = json!({
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "\"Paris\"}" } }] },
                "finish_reason": null
            }],
        });
        let ev = translate_chunk_to_anthropic_events(&c3, &mut state);
        let got = to_values(&ev);
        all.extend(got.clone());
        assert_eq!(
            got,
            vec![json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": "\"Paris\"}" }
            })]
        );

        // Finish.
        let c4 = json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
            "usage": { "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10 },
        });
        let ev = translate_chunk_to_anthropic_events(&c4, &mut state);
        let got = to_values(&ev);
        all.extend(got.clone());
        assert_eq!(
            got,
            vec![
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use" },
                    "usage": { "input_tokens": 7, "output_tokens": 3 }
                }),
                json!({ "type": "message_stop" }),
            ]
        );

        assert_single_open_block_invariant(&all);
    }

    #[test]
    fn content_during_tool_block_is_deferred_then_flushed() {
        let mut state = AnthropicStreamState::default();

        // Open a tool block.
        let c1 = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0, "id": "call_1", "function": { "name": "f" }
            }] }, "finish_reason": null }],
        });
        translate_chunk_to_anthropic_events(&c1, &mut state);

        // Plain text arrives while the tool block is open -> deferred, no events.
        let c2 = json!({
            "choices": [{ "index": 0, "delta": { "content": "trailing" }, "finish_reason": null }],
        });
        let ev = translate_chunk_to_anthropic_events(&c2, &mut state);
        assert!(
            ev.is_empty(),
            "content must be deferred while a tool block is open"
        );
        assert_eq!(state.deferred_content.as_deref(), Some("trailing"));

        // Finish: tool block closes, deferred text flushed as its own block.
        let c3 = json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        });
        let ev = translate_chunk_to_anthropic_events(&c3, &mut state);
        let got = to_values(&ev);
        assert_eq!(
            got,
            vec![
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": { "type": "text", "text": "" }
                }),
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": { "type": "text_delta", "text": "trailing" }
                }),
                json!({ "type": "content_block_stop", "index": 1 }),
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use" },
                    "usage": { "input_tokens": 1, "output_tokens": 1 }
                }),
                json!({ "type": "message_stop" }),
            ]
        );
    }

    #[test]
    fn thinking_text_opens_and_closes_thinking_block() {
        let mut state = AnthropicStreamState::default();
        let mut all: Vec<Value> = Vec::new();

        let c1 = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": { "reasoning_text": "pondering" }, "finish_reason": null }],
        });
        let ev = translate_chunk_to_anthropic_events(&c1, &mut state);
        let got = to_values(&ev);
        all.extend(got.clone());
        assert_eq!(
            got,
            vec![
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "x", "type": "message", "role": "assistant", "content": [],
                        "model": "m", "stop_reason": null, "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 }
                    }
                }),
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "thinking", "thinking": "" }
                }),
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "thinking_delta", "thinking": "pondering" }
                }),
            ]
        );
        assert!(state.thinking_block_open);

        // Real content closes the thinking block (signature_delta "" + stop) and
        // opens a fresh text block.
        let c2 = json!({
            "choices": [{ "index": 0, "delta": { "content": "Answer" }, "finish_reason": null }],
        });
        let ev = translate_chunk_to_anthropic_events(&c2, &mut state);
        let got = to_values(&ev);
        all.extend(got.clone());
        assert_eq!(
            got,
            vec![
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "signature_delta", "signature": "" }
                }),
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": { "type": "text", "text": "" }
                }),
                json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": { "type": "text_delta", "text": "Answer" }
                }),
            ]
        );

        let c3 = json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        });
        let ev = translate_chunk_to_anthropic_events(&c3, &mut state);
        all.extend(to_values(&ev));

        // No usage on the finishing chunk -> message_delta/message_stop are queued
        // and emitted by the flush.
        let ev = flush_pending_anthropic_stream_events(&mut state);
        let got = to_values(&ev);
        all.extend(got.clone());
        assert_eq!(
            got,
            vec![
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "input_tokens": 0, "output_tokens": 0 }
                }),
                json!({ "type": "message_stop" }),
            ]
        );

        assert_single_open_block_invariant(&all);
    }

    #[test]
    fn error_event_shape() {
        let ev = translate_error_to_anthropic_error_event();
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({
                "type": "error",
                "error": { "type": "api_error", "message": "An unexpected error occurred during streaming." }
            })
        );
    }

    #[test]
    fn unterminated_stream_synthesizes_close_with_open_block() {
        // Upstream sends content then ends WITHOUT a finish_reason (e.g. dropped
        // the finishing chunk or sent [DONE] early). A content block is left open
        // and no message_stop was emitted; the flush must close the block and
        // synthesize message_delta + message_stop so the client doesn't hang.
        let mut state = AnthropicStreamState::default();
        let mut all: Vec<Value> = Vec::new();

        let chunk = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": { "content": "Hi" }, "finish_reason": null }],
        });
        all.extend(to_values(&translate_chunk_to_anthropic_events(
            &chunk, &mut state,
        )));
        assert!(state.content_block_open);
        assert!(!state.message_stop_emitted);

        let got = to_values(&flush_pending_anthropic_stream_events(&mut state));
        all.extend(got.clone());

        assert_eq!(
            got,
            vec![
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" }
                }),
                json!({ "type": "message_stop" }),
            ]
        );
        assert!(state.message_stop_emitted);
        assert_single_open_block_invariant(&all);
    }

    #[test]
    fn unterminated_empty_stream_synthesizes_nothing() {
        // A stream that never even sent message_start has no open message to
        // close; the flush must emit nothing rather than a bare message_delta.
        let mut state = AnthropicStreamState::default();
        let got = to_values(&flush_pending_anthropic_stream_events(&mut state));
        assert!(got.is_empty());
    }

    #[test]
    fn flush_is_idempotent_after_normal_finish() {
        // After a normal finishing chunk emits message_stop, a follow-up flush
        // must not emit a second terminal close.
        let mut state = AnthropicStreamState::default();

        let start = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": { "content": "Hi" }, "finish_reason": null }],
        });
        let _ = translate_chunk_to_anthropic_events(&start, &mut state);

        let finish = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        });
        let _ = translate_chunk_to_anthropic_events(&finish, &mut state);
        assert!(state.message_stop_emitted);

        let got = to_values(&flush_pending_anthropic_stream_events(&mut state));
        assert!(got.is_empty(), "flush after a clean finish must be a no-op");
    }

    #[test]
    fn reasoning_only_turn_closes_thinking_block_on_finish() {
        // A turn that only ever emits reasoning text leaves a thinking block open;
        // the finishing chunk must close it (signature_delta "" + stop) so the
        // stream is well-formed rather than leaving a dangling content_block_start.
        let mut state = AnthropicStreamState::default();
        let mut all: Vec<Value> = Vec::new();

        let c1 = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": { "reasoning_text": "pondering" }, "finish_reason": null }],
        });
        all.extend(to_values(&translate_chunk_to_anthropic_events(
            &c1, &mut state,
        )));
        assert!(state.thinking_block_open);

        let finish = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 },
        });
        let got = to_values(&translate_chunk_to_anthropic_events(&finish, &mut state));
        all.extend(got.clone());

        // The thinking block is closed before the message_delta/stop are emitted.
        assert_eq!(
            got,
            vec![
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "signature_delta", "signature": "" }
                }),
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "input_tokens": 3, "output_tokens": 2 }
                }),
                json!({ "type": "message_stop" }),
            ]
        );
        assert!(!state.thinking_block_open);
        assert_single_open_block_invariant(&all);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn empty_choices_completes_pending_with_usage() {
        let mut state = AnthropicStreamState::default();
        state.pending_message_delta = Some(AnthropicStreamEventData::MessageDelta {
            delta: AnthropicMessageDeltaBody {
                stop_reason: Some("end_turn".to_string()),
                stop_sequence: None,
            },
            usage: Some(AnthropicMessageDeltaUsage {
                input_tokens: Some(0),
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
        });

        let chunk = json!({
            "choices": [],
            "usage": { "prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6 },
        });
        let ev = translate_chunk_to_anthropic_events(&chunk, &mut state);
        let got = to_values(&ev);
        assert_eq!(
            got,
            vec![
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "input_tokens": 4, "output_tokens": 2 }
                }),
                json!({ "type": "message_stop" }),
            ]
        );
        assert!(state.pending_message_delta.is_none());
    }
}
