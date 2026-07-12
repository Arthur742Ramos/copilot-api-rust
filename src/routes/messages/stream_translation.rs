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

/// Imported from [`super::utils`] so all translation modules share one source of
/// truth for the user-visible "Thinking..." placeholder. Compatible with
/// opencode, which filters out thinking blocks whose text is empty, so a
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

const DEFAULT_UPSTREAM_ERROR_TYPE: &str = "api_error";
const DEFAULT_UPSTREAM_ERROR_MESSAGE: &str = "The upstream model stream reported an error.";
const MAX_UPSTREAM_ERROR_TYPE_BYTES: usize = 64;
const MAX_UPSTREAM_ERROR_MESSAGE_BYTES: usize = 1024;

/// Extract the only two upstream error fields that are safe and useful to an
/// Anthropic client. A present, non-null top-level `error` always terminates the
/// stream, even when its value is malformed; treating it as an empty-choice
/// usage chunk could otherwise fabricate a successful completion.
fn top_level_upstream_error_event(chunk: &Value) -> Option<AnthropicStreamEventData> {
    let error = chunk.get("error")?;
    if error.is_null() {
        return None;
    }

    let kind = safe_upstream_error_type(error.get("type"))
        .unwrap_or_else(|| DEFAULT_UPSTREAM_ERROR_TYPE.to_string());
    let message = safe_upstream_error_message(error.get("message"))
        .unwrap_or_else(|| DEFAULT_UPSTREAM_ERROR_MESSAGE.to_string());

    Some(AnthropicStreamEventData::Error {
        error: super::anthropic_types::AnthropicErrorBody { kind, message },
    })
}

fn safe_upstream_error_type(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    if value.is_empty()
        || value.len() > MAX_UPSTREAM_ERROR_TYPE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return None;
    }
    Some(value.to_string())
}

fn safe_upstream_error_message(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    if value.is_empty()
        || value.len() > MAX_UPSTREAM_ERROR_MESSAGE_BYTES
        || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(value.to_string())
}

struct ValidatedChoice<'a> {
    choice: &'a Value,
    delta: &'a Value,
}

enum ValidatedChatChunk<'a> {
    Choice(ValidatedChoice<'a>),
    UsageOnly,
}

/// Validate the structural fields that drive the translated stream state
/// machine. Missing data and a value of the wrong JSON type must not collapse
/// into the same branch as a legitimate empty array: doing so can turn upstream
/// corruption into a successful Anthropic completion.
fn validate_chat_chunk(chunk: &Value) -> Result<ValidatedChatChunk<'_>, ()> {
    let object = chunk.as_object().ok_or(())?;
    let choices = object.get("choices").and_then(Value::as_array).ok_or(())?;

    if choices.is_empty() {
        // OpenAI's final include_usage record has an explicitly empty choices
        // array and a usage object. A bare `choices: []`, null usage, or a usage
        // value of another type is not that record.
        return object
            .get("usage")
            .filter(|usage| usage.is_object())
            .map(|_| ValidatedChatChunk::UsageOnly)
            .ok_or(());
    }

    // Usage is optional/null on ordinary chunks, but a present non-null value
    // must retain the object shape consumed by the accounting helpers.
    if object
        .get("usage")
        .is_some_and(|usage| !usage.is_null() && !usage.is_object())
    {
        return Err(());
    }

    for choice in choices {
        if !choice.is_object() {
            return Err(());
        }
        if choice.get("delta").and_then(Value::as_object).is_none() {
            return Err(());
        }
        if choice
            .get("finish_reason")
            .is_some_and(|reason| !reason.is_null() && !reason.is_string())
        {
            return Err(());
        }

        let delta = choice.get("delta").ok_or(())?;
        if delta.get("tool_calls").is_some_and(|tool_calls| {
            !tool_calls.is_null()
                && tool_calls
                    .as_array()
                    .is_none_or(|calls| calls.iter().any(|call| !call.is_object()))
        }) {
            return Err(());
        }
    }

    let choice = &choices[0];
    Ok(ValidatedChatChunk::Choice(ValidatedChoice {
        choice,
        delta: choice.get("delta").ok_or(())?,
    }))
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
    // Once success or failure has terminated the stream, every later chunk is
    // ignored. This prevents an upstream aggregator from turning a terminal
    // error into a later success or producing a second terminal event.
    if state.terminal_event_emitted {
        return Vec::new();
    }

    // OpenAI-compatible providers commonly send failures as a valid JSON SSE
    // record with a top-level `error`. Detect it before the empty-choice usage
    // path, which is also a valid Chat Completions record shape.
    if let Some(error) = top_level_upstream_error_event(chunk) {
        return terminal_stream_error_events(state, error);
    }

    let validated = match validate_chat_chunk(chunk) {
        Ok(validated) => validated,
        Err(()) => return malformed_stream_error_events(state),
    };

    let mut events: Vec<AnthropicStreamEventData> = Vec::new();
    let ValidatedChatChunk::Choice(ValidatedChoice {
        choice,
        delta: delta_value,
    }) = validated
    else {
        // An include_usage chunk is only valid after a finish_reason queued the
        // terminal message delta. An orphan usage record would otherwise be
        // silently ignored and allow the malformed stream to continue.
        if state.pending_message_delta.is_some() {
            complete_pending_message(state, &mut events, Some(chunk));
            return events;
        }
        return malformed_stream_error_events(state);
    };

    let mut delta = DeltaView::from_delta(delta_value);

    handle_message_start(state, &mut events, chunk);

    handle_thinking_text(&mut delta, state, &mut events);

    handle_content(&delta, state, &mut events);

    handle_tool_calls(&delta, state, &mut events);

    handle_finish(choice, &delta, state, &mut events, chunk);

    events
}

/// Flush a successful finish that was waiting for a final usage chunk. If the
/// upstream reaches EOF/[DONE] without any `finish_reason`, terminate with an
/// Anthropic `error` event instead of fabricating a successful `end_turn`.
pub fn flush_pending_anthropic_stream_events(
    state: &mut AnthropicStreamState,
) -> Vec<AnthropicStreamEventData> {
    // Terminal failure takes precedence over any stale deferred success state.
    // The normal error path clears the pending delta, but this ordering makes
    // EOF flushing safe even if a future caller constructs state manually.
    if state.terminal_event_emitted {
        state.pending_message_delta = None;
        return Vec::new();
    }

    let mut events: Vec<AnthropicStreamEventData> = Vec::new();

    if state.pending_message_delta.is_some() {
        complete_pending_message(state, &mut events, None);
        return events;
    }

    terminal_stream_error_events(
        state,
        protocol_error_event(
            "The upstream model stream ended before a finish reason was received.",
        ),
    )
}

/// Terminate a translated stream after malformed upstream SSE data.
pub fn malformed_stream_error_events(
    state: &mut AnthropicStreamState,
) -> Vec<AnthropicStreamEventData> {
    terminal_stream_error_events(
        state,
        protocol_error_event("The upstream model stream returned a malformed event."),
    )
}

/// Terminate a translated stream after its transport fails.
pub fn transport_stream_error_events(
    state: &mut AnthropicStreamState,
    cause: Option<&std::io::Error>,
) -> Vec<AnthropicStreamEventData> {
    terminal_stream_error_events(state, translate_error_to_anthropic_error_event(cause))
}

fn terminal_stream_error_events(
    state: &mut AnthropicStreamState,
    error: AnthropicStreamEventData,
) -> Vec<AnthropicStreamEventData> {
    if state.terminal_event_emitted {
        return Vec::new();
    }
    let mut events = Vec::new();
    close_stream_for_error(state, &mut events);
    events.push(error);
    state.terminal_event_emitted = true;
    events
}

fn protocol_error_event(message: &str) -> AnthropicStreamEventData {
    AnthropicStreamEventData::Error {
        error: super::anthropic_types::AnthropicErrorBody {
            kind: "api_error".to_string(),
            message: message.to_string(),
        },
    }
}

/// `translateErrorToAnthropicErrorEvent` — the terminal `error` event.
///
/// When the upstream stream breaks mid-flight (a truncated/reset SSE body or our
/// own read-timeout firing on a stalled-open connection), the failure is
/// transient and the request is safe to retry. We surface those as
/// `overloaded_error` — the type Anthropic clients (Claude Code included) treat
/// as retryable with backoff — instead of the generic `api_error`, which reads
/// like a permanent server fault and discourages an automatic retry. Pass the
/// `io::Error` yielded by [`crate::libs::sse::events`] so the cause can be
/// classified; pass `None` when no cause is available (the type then stays
/// `api_error`).
pub fn translate_error_to_anthropic_error_event(
    cause: Option<&std::io::Error>,
) -> AnthropicStreamEventData {
    if cause.map(is_transient_transport_error).unwrap_or(false) {
        return AnthropicStreamEventData::Error {
            error: super::anthropic_types::AnthropicErrorBody {
                kind: "overloaded_error".to_string(),
                message: "The upstream model stream ended unexpectedly. This is usually transient — retry the request.".to_string(),
            },
        };
    }
    AnthropicStreamEventData::Error {
        error: super::anthropic_types::AnthropicErrorBody {
            kind: "api_error".to_string(),
            message: "An unexpected error occurred during streaming.".to_string(),
        },
    }
}

/// Whether a stream error from [`crate::libs::sse::events`] is a transient
/// transport failure (so the caller should signal a retryable `overloaded_error`
/// rather than `api_error`).
///
/// `sse::events` boxes the originating `reqwest::Error` inside the yielded
/// `io::Error`, so we recover it via `source()` and consult reqwest's own
/// predicates: a read-timeout (`is_timeout`), a connection failure
/// (`is_connect`), or a truncated/aborted body (`is_body`) are all transient.
/// The internal SSE-record overflow guard carries a `&str` (not a reqwest error)
/// and so is correctly treated as non-transient. As a fallback for any non-
/// reqwest source, a `TimedOut`/`ConnectionReset`/`UnexpectedEof` `ErrorKind`
/// also counts as transient.
pub fn is_transient_transport_error(err: &std::io::Error) -> bool {
    use std::error::Error as _;
    if let Some(re) = err
        .source()
        .and_then(|s| s.downcast_ref::<reqwest::Error>())
    {
        return re.is_timeout() || re.is_connect() || re.is_body();
    }
    matches!(
        err.kind(),
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
    )
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
    if state.terminal_event_emitted {
        state.pending_message_delta = None;
        return;
    }

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
    state.terminal_event_emitted = true;
}

fn close_stream_for_error(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    close_thinking_block_if_open(state, events);
    if state.content_block_open {
        events.push(AnthropicStreamEventData::ContentBlockStop {
            index: state.content_block_index,
        });
        state.content_block_open = false;
        state.content_block_index += 1;
    }
    state.active_tool_call_index = None;
    state.tool_calls.clear();
    state.tool_call_order.clear();
    state.deferred_content = None;
    state.pending_message_delta = None;
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
        if tool_block_open {
            state.active_tool_call_index = None;
        }
        if !tool_block_open {
            handle_reasoning_opaque(delta, events, state);
        }
    }

    flush_buffered_tool_calls(state, events);
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
            if !state.tool_calls.contains_key(&index) {
                state.tool_call_order.push(index);
                state.tool_calls.insert(
                    index,
                    AnthropicStreamToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        anthropic_block_index: -1,
                        buffered_arguments: Vec::new(),
                        started: false,
                    },
                );
            }

            // Anthropic allows only one content block to be active at a time.
            // Stream the first OpenAI tool call immediately; later parallel
            // indices are buffered and serialized at finish.
            if state.active_tool_call_index.is_none() {
                if state.content_block_open {
                    events.push(AnthropicStreamEventData::ContentBlockStop {
                        index: state.content_block_index,
                    });
                    state.content_block_index += 1;
                    state.content_block_open = false;
                }
                if let Some(info) = state.tool_calls.get_mut(&index) {
                    info.anthropic_block_index = state.content_block_index;
                    info.started = true;
                    events.push(AnthropicStreamEventData::ContentBlockStart {
                        index: info.anthropic_block_index,
                        content_block: serde_json::json!({
                            "type": "tool_use",
                            "id": info.id,
                            "name": info.name,
                            "input": {},
                        }),
                    });
                }
                state.active_tool_call_index = Some(index);
                state.content_block_open = true;
            }
        }

        let arguments = tool_call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        if let Some(arguments) = arguments {
            if state.active_tool_call_index == Some(index) {
                if let Some(info) = state.tool_calls.get(&index) {
                    events.push(AnthropicStreamEventData::ContentBlockDelta {
                        index: info.anthropic_block_index,
                        delta: AnthropicContentBlockDelta::InputJsonDelta {
                            partial_json: arguments.to_string(),
                        },
                    });
                }
            } else if let Some(info) = state.tool_calls.get_mut(&index) {
                info.buffered_arguments.push(arguments.to_string());
            }
        }
    }
}

/// Serialize tool calls that OpenAI streamed in parallel after the active call.
/// Their argument fragment boundaries are preserved, but their Anthropic blocks
/// are emitted one-at-a-time in first-seen tool-index order.
fn flush_buffered_tool_calls(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    let order = state.tool_call_order.clone();
    for index in order {
        let Some(snapshot) = state.tool_calls.get(&index).cloned() else {
            continue;
        };
        if snapshot.started {
            continue;
        }

        let block_index = state.content_block_index;
        state.content_block_index += 1;
        events.push(AnthropicStreamEventData::ContentBlockStart {
            index: block_index,
            content_block: serde_json::json!({
                "type": "tool_use",
                "id": snapshot.id,
                "name": snapshot.name,
                "input": {},
            }),
        });
        for partial_json in snapshot.buffered_arguments {
            events.push(AnthropicStreamEventData::ContentBlockDelta {
                index: block_index,
                delta: AnthropicContentBlockDelta::InputJsonDelta { partial_json },
            });
        }
        events.push(AnthropicStreamEventData::ContentBlockStop { index: block_index });
        if let Some(info) = state.tool_calls.get_mut(&index) {
            info.anthropic_block_index = block_index;
            info.started = true;
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

/// `chunk?.usage` is present and is an object.
fn has_usage(chunk: &Value) -> bool {
    chunk.get("usage").is_some_and(Value::is_object)
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
    fn parallel_fragmented_tool_calls_are_serialized_into_valid_blocks() {
        let mut state = AnthropicStreamState::default();
        let mut all = Vec::new();

        let announced = json!({
            "id": "chatcmpl-parallel",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_a",
                        "function": { "name": "first", "arguments": "{\"a\":" }
                    },
                    {
                        "index": 1,
                        "id": "call_b",
                        "function": { "name": "second", "arguments": "{\"b\":" }
                    }
                ]},
                "finish_reason": null
            }]
        });
        all.extend(to_values(&translate_chunk_to_anthropic_events(
            &announced, &mut state,
        )));

        let interleaved = json!({
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [
                    { "index": 1, "function": { "arguments": "2}" } },
                    { "index": 0, "function": { "arguments": "1}" } }
                ]},
                "finish_reason": null
            }]
        });
        all.extend(to_values(&translate_chunk_to_anthropic_events(
            &interleaved,
            &mut state,
        )));

        let finish = json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
            "usage": { "prompt_tokens": 4, "completion_tokens": 2 }
        });
        all.extend(to_values(&translate_chunk_to_anthropic_events(
            &finish, &mut state,
        )));

        assert_single_open_block_invariant(&all);
        let starts: Vec<_> = all
            .iter()
            .filter(|event| event["type"] == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["content_block"]["id"], "call_a");
        assert_eq!(starts[1]["content_block"]["id"], "call_b");

        let second_fragments: String = all
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta" && event["index"].as_i64() == Some(1)
            })
            .filter_map(|event| event.pointer("/delta/partial_json").and_then(Value::as_str))
            .collect();
        assert_eq!(second_fragments, "{\"b\":2}");
        assert_eq!(all.last().unwrap()["type"], "message_stop");
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
        let ev = translate_error_to_anthropic_error_event(None);
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({
                "type": "error",
                "error": { "type": "api_error", "message": "An unexpected error occurred during streaming." }
            })
        );
    }

    #[test]
    fn transient_transport_cause_maps_to_overloaded_error() {
        // A stalled/truncated upstream connection (here modeled by a TimedOut
        // io::Error) is transient, so the terminal event must be the retryable
        // `overloaded_error` type rather than `api_error`.
        let cause = std::io::Error::from(std::io::ErrorKind::TimedOut);
        let ev = translate_error_to_anthropic_error_event(Some(&cause));
        let value = serde_json::to_value(&ev).unwrap();
        assert_eq!(value["error"]["type"], "overloaded_error");
    }

    #[test]
    fn non_transport_cause_stays_api_error() {
        // The internal SSE-overflow guard is a plain message error, not a
        // transport failure, so it must NOT be mislabeled as retryable.
        let cause = std::io::Error::other("SSE record exceeded the maximum buffered size");
        let ev = translate_error_to_anthropic_error_event(Some(&cause));
        let value = serde_json::to_value(&ev).unwrap();
        assert_eq!(value["error"]["type"], "api_error");
    }

    #[test]
    fn malformed_event_closes_open_block_and_errors_once() {
        let mut state = AnthropicStreamState::default();
        let chunk = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": { "content": "partial" }, "finish_reason": null }]
        });
        let _ = translate_chunk_to_anthropic_events(&chunk, &mut state);

        let events = to_values(&malformed_stream_error_events(&mut state));
        assert_eq!(events[0], json!({"type":"content_block_stop","index":0}));
        assert_eq!(events[1]["type"], "error");
        assert_eq!(events[1]["error"]["type"], "api_error");
        assert!(malformed_stream_error_events(&mut state).is_empty());
        assert!(flush_pending_anthropic_stream_events(&mut state).is_empty());
    }

    #[test]
    fn top_level_upstream_error_closes_open_block_before_safe_error() {
        let mut state = AnthropicStreamState::default();
        let mut all = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "id": "x",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": { "content": "partial" },
                    "finish_reason": null
                }]
            }),
            &mut state,
        ));
        assert!(state.content_block_open);
        assert!(state.pending_message_delta.is_none());

        let got = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "error": {
                    "type": "server_error",
                    "message": " upstream boom ",
                    "internal": { "request_body": "must-not-leak" }
                },
                "choices": [],
                "usage": { "prompt_tokens": 99, "completion_tokens": 99 }
            }),
            &mut state,
        ));
        assert_eq!(
            got,
            vec![
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "error",
                    "error": {
                        "type": "server_error",
                        "message": "upstream boom"
                    }
                }),
            ]
        );

        all.extend(got);
        assert_single_open_block_invariant(&all);
        assert!(!state.content_block_open);
        assert!(!state.message_stop_emitted);
        assert!(state.terminal_event_emitted);
        assert!(state.pending_message_delta.is_none());
    }

    #[test]
    fn top_level_upstream_error_closes_thinking_block_in_protocol_order() {
        let mut state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id": "x",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": { "reasoning_text": "partial thought" },
                    "finish_reason": null
                }]
            }),
            &mut state,
        );
        assert!(state.thinking_block_open);

        let got = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "error": {
                    "type": "overloaded_error",
                    "message": "capacity unavailable"
                }
            }),
            &mut state,
        ));
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
                    "type": "error",
                    "error": {
                        "type": "overloaded_error",
                        "message": "capacity unavailable"
                    }
                }),
            ]
        );
        assert!(!state.thinking_block_open);
        assert!(state.terminal_event_emitted);
    }

    #[test]
    fn top_level_upstream_error_discards_pending_success_and_terminates_once() {
        let mut state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id": "x",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": { "content": "partial" },
                    "finish_reason": null
                }]
            }),
            &mut state,
        );
        let finish = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            &mut state,
        ));
        assert_eq!(
            finish,
            vec![json!({ "type": "content_block_stop", "index": 0 })]
        );
        assert!(state.pending_message_delta.is_some());
        assert!(!state.terminal_event_emitted);

        let error_chunk = json!({
            "error": {
                "type": "server_error",
                "message": "upstream failed after finish"
            },
            "choices": []
        });
        let terminal = to_values(&translate_chunk_to_anthropic_events(
            &error_chunk,
            &mut state,
        ));
        assert_eq!(
            terminal,
            vec![json!({
                "type": "error",
                "error": {
                    "type": "server_error",
                    "message": "upstream failed after finish"
                }
            })]
        );
        assert!(state.pending_message_delta.is_none());
        assert!(state.terminal_event_emitted);
        assert!(!state.message_stop_emitted);

        // Neither a duplicate upstream error, a complete success chunk, a
        // usage-only chunk, nor EOF may produce success or a second terminal.
        assert!(translate_chunk_to_anthropic_events(&error_chunk, &mut state).is_empty());
        assert!(translate_chunk_to_anthropic_events(
            &json!({
                "choices": [{
                    "index": 0,
                    "delta": { "content": "late success" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
            }),
            &mut state,
        )
        .is_empty());
        assert!(translate_chunk_to_anthropic_events(
            &json!({
                "choices": [],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
            }),
            &mut state,
        )
        .is_empty());
        assert!(flush_pending_anthropic_stream_events(&mut state).is_empty());

        let terminal_count = terminal
            .iter()
            .filter(|event| matches!(event["type"].as_str(), Some("error" | "message_stop")))
            .count();
        assert_eq!(terminal_count, 1);
    }

    #[test]
    fn malformed_choices_discards_pending_success_and_suppresses_followups() {
        let mut state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id": "x",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": { "content": "partial" },
                    "finish_reason": null
                }]
            }),
            &mut state,
        );
        let finish = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            &mut state,
        ));
        assert_eq!(
            finish,
            vec![json!({ "type": "content_block_stop", "index": 0 })]
        );
        assert!(state.pending_message_delta.is_some());

        // A valid JSON object with usage but no choices is not OpenAI's
        // `choices: []` usage-only record. It must discard the queued success
        // instead of flushing message_delta/message_stop.
        let terminal = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "usage": { "prompt_tokens": 4, "completion_tokens": 2 }
            }),
            &mut state,
        ));
        assert_eq!(
            terminal,
            vec![json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": "The upstream model stream returned a malformed event."
                }
            })]
        );
        assert!(state.pending_message_delta.is_none());
        assert!(!state.message_stop_emitted);
        assert!(state.terminal_event_emitted);

        // Later success, upstream-error, and valid usage-only records are all
        // suppressed, and EOF cannot flush the discarded success.
        for late_chunk in [
            json!({
                "choices": [{
                    "index": 0,
                    "delta": { "content": "late success" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
            }),
            json!({
                "error": {
                    "type": "server_error",
                    "message": "late upstream error"
                }
            }),
            json!({
                "choices": [],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
            }),
        ] {
            assert!(translate_chunk_to_anthropic_events(&late_chunk, &mut state).is_empty());
        }
        assert!(flush_pending_anthropic_stream_events(&mut state).is_empty());
    }

    #[test]
    fn malformed_choices_closes_open_blocks_and_clears_tool_state() {
        let mut thinking_state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id": "x",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": { "reasoning_text": "partial thought" },
                    "finish_reason": null
                }]
            }),
            &mut thinking_state,
        );
        assert!(thinking_state.thinking_block_open);

        let thinking_terminal = to_values(&translate_chunk_to_anthropic_events(
            &json!({ "choices": {} }),
            &mut thinking_state,
        ));
        assert_eq!(
            thinking_terminal,
            vec![
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "signature_delta", "signature": "" }
                }),
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "The upstream model stream returned a malformed event."
                    }
                }),
            ]
        );
        assert!(!thinking_state.thinking_block_open);
        assert!(thinking_state.terminal_event_emitted);

        let mut tool_state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id": "x",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "lookup", "arguments": "{\"q\":" }
                    }] },
                    "finish_reason": null
                }]
            }),
            &mut tool_state,
        );
        let deferred = translate_chunk_to_anthropic_events(
            &json!({
                "choices": [{
                    "index": 0,
                    "delta": { "content": "deferred" },
                    "finish_reason": null
                }]
            }),
            &mut tool_state,
        );
        assert!(deferred.is_empty());
        assert!(tool_state.content_block_open);
        assert_eq!(tool_state.deferred_content.as_deref(), Some("deferred"));
        assert!(!tool_state.tool_calls.is_empty());

        let tool_terminal = to_values(&translate_chunk_to_anthropic_events(
            &json!({ "choices": null }),
            &mut tool_state,
        ));
        assert_eq!(
            tool_terminal,
            vec![
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "The upstream model stream returned a malformed event."
                    }
                }),
            ]
        );
        assert!(!tool_state.content_block_open);
        assert!(tool_state.active_tool_call_index.is_none());
        assert!(tool_state.tool_calls.is_empty());
        assert!(tool_state.tool_call_order.is_empty());
        assert!(tool_state.deferred_content.is_none());
        assert!(tool_state.pending_message_delta.is_none());
        assert!(tool_state.terminal_event_emitted);
    }

    #[test]
    fn malformed_choice_and_neighboring_shapes_are_terminal() {
        let malformed_chunks = [
            Value::Null,
            json!([]),
            json!({}),
            json!({ "choices": null }),
            json!({ "choices": "not-an-array" }),
            json!({ "choices": [null] }),
            json!({ "choices": [{}] }),
            json!({ "choices": [{ "delta": null, "finish_reason": null }] }),
            json!({ "choices": [{ "delta": {}, "finish_reason": 42 }] }),
            json!({ "choices": [{ "delta": { "tool_calls": {} } }] }),
            json!({ "choices": [{ "delta": {} }], "usage": [] }),
            json!({ "choices": [] }),
            json!({ "choices": [], "usage": null }),
            json!({ "choices": [], "usage": [] }),
            // Even a structurally valid usage-only record is out of order when
            // no finish_reason has queued a pending message delta.
            json!({ "choices": [], "usage": {} }),
        ];

        for chunk in malformed_chunks {
            let mut state = AnthropicStreamState::default();
            let got = to_values(&translate_chunk_to_anthropic_events(&chunk, &mut state));
            assert_eq!(
                got,
                vec![json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "The upstream model stream returned a malformed event."
                    }
                })],
                "chunk should terminate as malformed: {chunk}"
            );
            assert!(state.terminal_event_emitted);
            assert!(!state.message_stop_emitted);
            assert!(flush_pending_anthropic_stream_events(&mut state).is_empty());
        }
    }

    #[test]
    fn malformed_or_unsafe_top_level_error_uses_opaque_fallback() {
        for chunk in [
            json!({
                "error": {
                    "type": "server error",
                    "message": "unsafe\u{0000}diagnostic",
                    "internal": "must-not-leak"
                },
                "choices": []
            }),
            json!({
                "error": ["opaque", "provider", "details"],
                "choices": []
            }),
        ] {
            let mut state = AnthropicStreamState::default();
            let got = to_values(&translate_chunk_to_anthropic_events(&chunk, &mut state));
            assert_eq!(
                got,
                vec![json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "The upstream model stream reported an error."
                    }
                })]
            );
            assert!(!serde_json::to_string(&got)
                .unwrap()
                .contains("must-not-leak"));
            assert!(state.terminal_event_emitted);
        }
    }

    #[test]
    fn classifies_transient_error_kinds() {
        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::BrokenPipe,
        ] {
            assert!(is_transient_transport_error(&std::io::Error::from(kind)));
        }
        assert!(!is_transient_transport_error(&std::io::Error::other(
            "internal guard"
        )));
    }

    #[test]
    fn unterminated_stream_closes_block_then_errors() {
        // Upstream sends content then ends WITHOUT a finish_reason (e.g. dropped
        // the finishing chunk or sent [DONE] early). A content block is left open
        // and no message_stop was emitted; the flush must close the block and
        // report a terminal error rather than misrepresenting partial output as
        // a successful end_turn.
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
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "The upstream model stream ended before a finish reason was received."
                    }
                })
            ]
        );
        assert!(!state.message_stop_emitted);
        assert!(state.terminal_event_emitted);
        assert_single_open_block_invariant(&all);
    }

    #[test]
    fn unterminated_empty_stream_emits_terminal_error() {
        let mut state = AnthropicStreamState::default();
        let got = to_values(&flush_pending_anthropic_stream_events(&mut state));
        assert_eq!(
            got,
            vec![json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": "The upstream model stream ended before a finish reason was received."
                }
            })]
        );
        assert!(state.terminal_event_emitted);
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
            "error": null,
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
        assert!(state.message_stop_emitted);
        assert!(state.terminal_event_emitted);
    }
}
