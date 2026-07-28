//! OpenAI ChatCompletions streaming chunk -> Anthropic SSE event state machine.
//!
//! Mirrors `src/routes/messages/stream-translation.ts`. Each incoming OpenAI
//! streaming delta (`chunk`) is translated into an ordered list of Anthropic
//! `AnthropicStreamEventData` events. The translation is stateful: per-stream
//! progress lives in [`AnthropicStreamState`] (message_start emitted once, the
//! single currently-open content block, the thinking block, accumulated tool
//! calls, a source-ordered deferred-output scheduler, and a pending
//! `message_delta`).
//!
//! Chunks are dynamic (`content` is `string | null`, `tool_calls` is a sparse
//! array, etc.) so we accept the chunk as `&serde_json::Value` and project the
//! one delta we care about into a small [`DeltaView`].

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use super::anthropic_types::{
    AnthropicContentBlockDelta, AnthropicMessageDeltaBody, AnthropicMessageDeltaUsage,
    AnthropicMessageStart, AnthropicStreamDeferredOutput, AnthropicStreamEventData,
    AnthropicStreamState, AnthropicStreamToolCall, AnthropicUsage,
};
use super::non_stream_translation::{
    empty_chat_completion_usage, encode_chat_reasoning_signature, map_openai_chat_completion_usage,
    parse_chat_service_tier,
};
use super::request_validation::collect_open_object_extensions;
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
/// consumes. Ordinary content remains distinct from reasoning fallback text so
/// refusal authority and actual emission state cannot be conflated.
struct DeltaView {
    content: Option<String>,
    reasoning_text: Option<String>,
    reasoning_content: Option<String>,
    reasoning_opaque: Option<String>,
    refusal: Option<String>,
    tool_calls: Vec<ValidatedToolDelta>,
}

impl DeltaView {
    /// JS `delta.content && delta.content.length > 0`.
    fn has_content(&self) -> bool {
        self.content.as_deref().is_some_and(|c| !c.is_empty())
    }

    /// JS `delta.tool_calls && delta.tool_calls.length > 0`.
    fn has_tool_call_delta(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

#[derive(Debug, Clone)]
struct ValidatedToolDelta {
    index: i64,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
    extra: Map<String, Value>,
    first: bool,
}

/// `string | null | undefined` -> `Option<String>`. Empty string stays `Some("")`
/// so callers can distinguish `=== ""` from `null`/absent.
fn opt_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// The translated stream treats omitted and explicit-null optional strings the
/// same way, but any other present JSON type is malformed. Keep this predicate
/// shared with the `opt_string` projection so validation and extraction cannot
/// drift apart.
fn validate_optional_string(v: Option<&Value>) -> Result<(), ()> {
    match v {
        None | Some(Value::Null | Value::String(_)) => Ok(()),
        Some(_) => Err(()),
    }
}

/// OpenAI tool indices and token counts are JSON integers in the range this
/// state machine stores. In particular, do not accept a floating-point number
/// and truncate it while extracting the value.
pub(crate) fn nonnegative_i64(v: &Value) -> Option<i64> {
    v.as_i64().filter(|value| *value >= 0)
}

const DEFAULT_UPSTREAM_ERROR_TYPE: &str = "api_error";
const DEFAULT_UPSTREAM_ERROR_MESSAGE: &str = "The upstream model stream reported an error.";
const MAX_UPSTREAM_ERROR_TYPE_BYTES: usize = 64;
const MAX_UPSTREAM_ERROR_MESSAGE_BYTES: usize = 1024;
const MAX_CHAT_STREAM_TOOL_CALLS: usize = 128;

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

pub(crate) fn safe_upstream_error_type(value: Option<&Value>) -> Option<String> {
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

pub(crate) fn safe_upstream_error_message(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    if value.is_empty()
        || value.len() > MAX_UPSTREAM_ERROR_MESSAGE_BYTES
        || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(value.to_string())
}

struct ValidatedChoice {
    delta: DeltaView,
    finish_reason: Option<String>,
    usage: Option<AnthropicUsage>,
    usage_present: bool,
}

enum ValidatedChatChunk {
    Choice(ValidatedChoice),
    UsageOnly(AnthropicUsage),
}

const MESSAGE_START_CANONICAL_FIELDS: &[&str] = &[
    "id",
    "type",
    "role",
    "content",
    "model",
    "stop_reason",
    "stop_sequence",
    "usage",
];

struct ChunkIdentity {
    id: String,
    model: String,
    created: i64,
    service_tier: Option<Option<String>>,
    system_fingerprint: Option<Option<String>>,
    extras: Map<String, Value>,
}

fn asserted_optional_string(value: Option<&Value>) -> Result<Option<Option<String>>, ()> {
    match value {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(value)) => Ok(Some(Some(value.clone()))),
        Some(_) => Err(()),
    }
}

fn validate_service_tier(value: Option<&Value>) -> Result<Option<Option<String>>, ()> {
    let asserted = asserted_optional_string(value)?;
    parse_chat_service_tier(value, "chunk.service_tier")
        .map_err(|_| ())
        .map(|_| asserted)
}

fn validate_chunk_identity(
    chunk: &Map<String, Value>,
    state: &AnthropicStreamState,
) -> Result<ChunkIdentity, ()> {
    let id = chunk
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(())?
        .to_string();
    let model = chunk
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(())?
        .to_string();
    if chunk.get("object").and_then(Value::as_str) != Some("chat.completion.chunk") {
        return Err(());
    }
    let created = chunk.get("created").and_then(nonnegative_i64).ok_or(())?;
    let service_tier = validate_service_tier(chunk.get("service_tier"))?;
    let system_fingerprint = asserted_optional_string(chunk.get("system_fingerprint"))?;
    if let (Some(Some(current)), Some(established)) = (
        service_tier.as_ref(),
        state
            .chat_usage
            .as_ref()
            .and_then(|usage| usage.service_tier.as_ref()),
    ) {
        if current != established {
            return Err(());
        }
    }
    let extras = collect_open_object_extensions(
        chunk,
        &[
            "id",
            "object",
            "created",
            "model",
            "choices",
            "usage",
            "error",
            "service_tier",
        ],
        MESSAGE_START_CANONICAL_FIELDS,
        "chunk",
    )
    .map_err(|_| ())?;

    if let Some(expected) = state.chat_id.as_deref() {
        if id != expected
            || state.chat_model.as_deref() != Some(model.as_str())
            || state.chat_created != Some(created)
        {
            return Err(());
        }
        validate_optional_assertion(&state.chat_service_tier, &service_tier)?;
        validate_optional_assertion(&state.chat_system_fingerprint, &system_fingerprint)?;
        for (key, value) in &extras {
            if key == "system_fingerprint" {
                continue;
            }
            if state.chat_top_level_extras.get(key) != Some(value) {
                return Err(());
            }
        }
    }
    Ok(ChunkIdentity {
        id,
        model,
        created,
        service_tier,
        system_fingerprint,
        extras,
    })
}

fn validate_optional_assertion(
    established: &Option<Option<String>>,
    current: &Option<Option<String>>,
) -> Result<(), ()> {
    if let Some(Some(current)) = current {
        if established
            .as_ref()
            .and_then(|value| value.as_ref())
            .is_some_and(|value| value != current)
        {
            return Err(());
        }
    }
    Ok(())
}

fn validate_choice_extras(choice: &Map<String, Value>) -> Result<(), ()> {
    if choice.get("logprobs").is_some_and(|value| !value.is_null()) {
        return Err(());
    }
    let extras = collect_open_object_extensions(
        choice,
        &["index", "delta", "finish_reason", "logprobs"],
        &[],
        "choice",
    )
    .map_err(|_| ())?;
    if extras.is_empty() {
        Ok(())
    } else {
        Err(())
    }
}

fn validate_delta_extras(delta: &Map<String, Value>) -> Result<(), ()> {
    if delta
        .get("role")
        .is_some_and(|role| !role.is_null() && role.as_str() != Some("assistant"))
        || delta
            .get("function_call")
            .is_some_and(|value| !value.is_null())
    {
        return Err(());
    }
    let extras = collect_open_object_extensions(
        delta,
        &[
            "role",
            "content",
            "reasoning_text",
            "reasoning_content",
            "reasoning_opaque",
            "tool_calls",
            "refusal",
            "function_call",
        ],
        &[],
        "choice.delta",
    )
    .map_err(|_| ())?;
    if extras.is_empty() {
        Ok(())
    } else {
        Err(())
    }
}

fn validate_tool_deltas(
    delta: &Map<String, Value>,
    state: &AnthropicStreamState,
) -> Result<Vec<ValidatedToolDelta>, ()> {
    let calls = match delta.get("tool_calls") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(calls)) => calls,
        Some(_) => return Err(()),
    };
    let mut seen = HashSet::new();
    let mut next_index = state.tool_calls.len() as i64;
    let mut validated = Vec::with_capacity(calls.len());
    for call in calls {
        let source = call.as_object().ok_or(())?;
        let index = source.get("index").and_then(nonnegative_i64).ok_or(())?;
        if !seen.insert(index) {
            return Err(());
        }
        let existing = state.tool_calls.get(&index);
        let first = existing.is_none();
        if first && index != next_index {
            return Err(());
        }
        if first && next_index as usize >= MAX_CHAT_STREAM_TOOL_CALLS {
            return Err(());
        }
        match source.get("type") {
            None | Some(Value::Null) => {}
            Some(Value::String(value)) if value == "function" => {}
            _ => return Err(()),
        }
        let id = optional_nonempty_tool_string(source.get("id"))?;
        let (name, arguments, function_extensions) = match source.get("function") {
            None | Some(Value::Null) => (None, None, Map::new()),
            Some(Value::Object(function)) => {
                let name = optional_nonempty_tool_string(function.get("name"))?;
                validate_optional_string(function.get("arguments"))?;
                let arguments = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let extensions = collect_open_object_extensions(
                    function,
                    &["name", "arguments"],
                    &[],
                    "tool_call.function",
                )
                .map_err(|_| ())?;
                (name, arguments, extensions)
            }
            Some(_) => return Err(()),
        };
        if let Some(arguments) = arguments.as_ref() {
            let existing_len = existing.map_or(0, |call| call.arguments.len());
            if existing_len
                .checked_add(arguments.len())
                .is_none_or(|length| length > crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES)
            {
                return Err(());
            }
        }
        if let Some(existing) = existing {
            if id
                .as_ref()
                .zip(existing.id.as_ref())
                .is_some_and(|(current, established)| current != established)
                || name
                    .as_ref()
                    .zip(existing.name.as_ref())
                    .is_some_and(|(current, established)| current != established)
            {
                return Err(());
            }
        }
        let mut extra = collect_open_object_extensions(
            source,
            &["index", "id", "type", "function"],
            &["type", "id", "name", "input", "chat_function_extensions"],
            "tool_call",
        )
        .map_err(|_| ())?;
        if !function_extensions.is_empty() {
            extra.insert(
                "chat_function_extensions".to_string(),
                Value::Object(function_extensions),
            );
        }
        if let Some(existing) = existing {
            let mut merged = existing.extra.clone();
            merge_tool_extensions(&mut merged, &extra)?;
            if existing.started && merged != existing.extra {
                return Err(());
            }
        }
        validated.push(ValidatedToolDelta {
            index,
            id,
            name,
            arguments,
            extra,
            first,
        });
        if first {
            next_index += 1;
        }
    }
    Ok(validated)
}

fn optional_nonempty_tool_string(value: Option<&Value>) -> Result<Option<String>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

fn merge_tool_extensions(
    target: &mut Map<String, Value>,
    incoming: &Map<String, Value>,
) -> Result<(), ()> {
    for (key, value) in incoming {
        if key == "chat_function_extensions" {
            let incoming = value.as_object().ok_or(())?;
            let entry = target
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            let target = entry.as_object_mut().ok_or(())?;
            for (nested_key, nested_value) in incoming {
                if target
                    .get(nested_key)
                    .is_some_and(|existing| existing != nested_value)
                {
                    return Err(());
                }
                target
                    .entry(nested_key.clone())
                    .or_insert_with(|| nested_value.clone());
            }
        } else {
            if target.get(key).is_some_and(|existing| existing != value) {
                return Err(());
            }
            target.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    Ok(())
}

fn validate_terminal_tools(
    finish_reason: &str,
    deltas: &[ValidatedToolDelta],
    state: &AnthropicStreamState,
) -> Result<(), ()> {
    let mut arguments: std::collections::HashMap<i64, String> = state
        .tool_calls
        .iter()
        .map(|(index, call)| (*index, call.arguments.clone()))
        .collect();
    let mut identities: std::collections::HashMap<i64, (Option<String>, Option<String>)> = state
        .tool_calls
        .iter()
        .map(|(index, call)| (*index, (call.id.clone(), call.name.clone())))
        .collect();
    for delta in deltas {
        if delta.first {
            arguments.insert(delta.index, String::new());
            identities.insert(delta.index, (None, None));
        }
        if let Some(identity) = identities.get_mut(&delta.index) {
            if delta.id.is_some() {
                identity.0 = delta.id.clone();
            }
            if delta.name.is_some() {
                identity.1 = delta.name.clone();
            }
        }
        if let Some(fragment) = &delta.arguments {
            arguments.entry(delta.index).or_default().push_str(fragment);
        }
    }
    let tools_must_be_complete = finish_reason == "tool_calls"
        || (finish_reason == "content_filter" && !arguments.is_empty());
    if tools_must_be_complete {
        if finish_reason == "tool_calls" && arguments.is_empty() {
            return Err(());
        }
        for index in 0..arguments.len() as i64 {
            let (id, name) = identities.get(&index).ok_or(())?;
            if id.as_deref().is_none_or(str::is_empty) || name.as_deref().is_none_or(str::is_empty)
            {
                return Err(());
            }
            let value: Value =
                serde_json::from_str(arguments.get(&index).ok_or(())?).map_err(|_| ())?;
            if !value.is_object() {
                return Err(());
            }
        }
    } else if !arguments.is_empty() {
        return Err(());
    }
    Ok(())
}

fn validate_finish_reason(
    value: Option<&Value>,
    delta: &DeltaView,
    state: &AnthropicStreamState,
    reconciliation: RefusalReconciliation,
) -> Result<Option<String>, ()> {
    let finish = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(value)) if !value.is_empty() => value,
        _ => return Err(()),
    };
    if finish == "function_call"
        || !matches!(
            finish.as_str(),
            "stop" | "length" | "content_filter" | "tool_calls"
        )
    {
        return Err(());
    }
    validate_terminal_tools(finish, &delta.tool_calls, state)?;
    let has_output = state.chat_output_seen
        || delta.has_content()
        || delta
            .reasoning_text
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || delta
            .reasoning_content
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || delta
            .reasoning_opaque
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || delta
            .refusal
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || !delta.tool_calls.is_empty();
    let refusal_seen = reconciliation.refusal_len > 0;
    if refusal_seen && finish != "content_filter" {
        return Err(());
    }
    if !has_output {
        return Err(());
    }
    Ok(Some(finish.to_string()))
}

fn parse_chunk_usage(
    chunk: &Map<String, Value>,
    service_tier: Option<&str>,
    state: &AnthropicStreamState,
) -> Result<(Option<AnthropicUsage>, bool), ()> {
    let Some(usage) = chunk.get("usage") else {
        return Ok((None, false));
    };
    if usage.is_null() {
        return Ok((None, false));
    }
    let parsed = map_openai_chat_completion_usage(usage, service_tier).map_err(|_| ())?;
    if let Some(existing) = &state.chat_usage_source {
        if existing != usage {
            return Err(());
        }
    }
    Ok((Some(parsed), true))
}

/// OpenAI declares `refusal` on `ChoiceDelta`, so every present string is a
/// fragment rather than a complete snapshot. Ordinary content and refusal may
/// mirror one another. Aggregates must remain prefix-compatible; whichever
/// representation is longer supplies the complete visible text at finish.
#[derive(Debug, Clone, Copy)]
struct RefusalReconciliation {
    refusal_len: usize,
}

fn reconcile_refusal_content(
    delta: &DeltaView,
    state: &AnthropicStreamState,
) -> Result<RefusalReconciliation, ()> {
    let content_fragment = delta.content.as_deref().unwrap_or_default();
    let refusal_base = state.chat_refusal_text.as_deref().unwrap_or_default();
    let refusal_fragment = delta.refusal.as_deref().unwrap_or_default();
    let content_len = state
        .chat_content_seen
        .len()
        .checked_add(content_fragment.len())
        .filter(|length| *length <= crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES)
        .ok_or(())?;
    let refusal_len = refusal_base
        .len()
        .checked_add(refusal_fragment.len())
        .filter(|length| *length <= crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES)
        .ok_or(())?;

    // Previous aggregates were already prefix-checked. Compare only the newly
    // overlapping bytes so long mirrored streams remain linear rather than
    // rescanning or cloning their entire output for every fragment.
    let compared = state.chat_content_seen.len().min(refusal_base.len());
    let newly_comparable = content_len.min(refusal_len);
    let content_base = state.chat_content_seen.as_bytes();
    let content_fragment = content_fragment.as_bytes();
    let refusal_base = refusal_base.as_bytes();
    let refusal_fragment = refusal_fragment.as_bytes();
    for index in compared..newly_comparable {
        let content = if index < content_base.len() {
            content_base[index]
        } else {
            content_fragment[index - content_base.len()]
        };
        let refusal = if index < refusal_base.len() {
            refusal_base[index]
        } else {
            refusal_fragment[index - refusal_base.len()]
        };
        if content != refusal {
            return Err(());
        }
    }
    Ok(RefusalReconciliation { refusal_len })
}

fn validate_chat_chunk(
    chunk: &Value,
    state: &mut AnthropicStreamState,
) -> Result<ValidatedChatChunk, ()> {
    let object = chunk.as_object().ok_or(())?;
    let identity = validate_chunk_identity(object, state)?;
    let service_tier = identity
        .service_tier
        .as_ref()
        .and_then(|value| value.as_deref())
        .or_else(|| {
            state
                .chat_service_tier
                .as_ref()
                .and_then(|value| value.as_deref())
        });
    let (usage, usage_present) = parse_chunk_usage(object, service_tier, state)?;
    let choices = object.get("choices").and_then(Value::as_array).ok_or(())?;

    let validated = if choices.is_empty() {
        if !usage_present {
            return Err(());
        }
        ValidatedChatChunk::UsageOnly(usage.clone().ok_or(())?)
    } else {
        if choices.len() != 1 || state.pending_message_delta.is_some() {
            return Err(());
        }
        let choice = choices[0].as_object().ok_or(())?;
        if choice.get("index").and_then(nonnegative_i64) != Some(0) {
            return Err(());
        }
        validate_choice_extras(choice)?;
        let delta = choice.get("delta").and_then(Value::as_object).ok_or(())?;
        validate_delta_extras(delta)?;
        for field in [
            "content",
            "reasoning_text",
            "reasoning_content",
            "reasoning_opaque",
            "refusal",
        ] {
            validate_optional_string(delta.get(field))?;
        }
        if let (Some(left), Some(right)) = (
            delta.get("reasoning_text").and_then(Value::as_str),
            delta.get("reasoning_content").and_then(Value::as_str),
        ) {
            if left != right {
                return Err(());
            }
        }
        let tool_calls = validate_tool_deltas(delta, state)?;
        let delta = DeltaView {
            content: opt_string(delta.get("content")),
            reasoning_text: opt_string(delta.get("reasoning_text")),
            reasoning_content: opt_string(delta.get("reasoning_content")),
            reasoning_opaque: opt_string(delta.get("reasoning_opaque")),
            refusal: opt_string(delta.get("refusal")),
            tool_calls,
        };
        let reconciliation = reconcile_refusal_content(&delta, state)?;
        let finish_reason =
            validate_finish_reason(choice.get("finish_reason"), &delta, state, reconciliation)?;
        ValidatedChatChunk::Choice(ValidatedChoice {
            delta,
            finish_reason,
            usage: usage.clone(),
            usage_present,
        })
    };

    if state.chat_id.is_none() {
        state.chat_id = Some(identity.id);
        state.chat_model = Some(identity.model);
        state.chat_created = Some(identity.created);
        state.chat_service_tier = identity.service_tier;
        state.chat_system_fingerprint = identity.system_fingerprint;
        state.chat_top_level_extras = identity.extras;
    } else {
        if state
            .chat_service_tier
            .as_ref()
            .and_then(|value| value.as_ref())
            .is_none()
            && identity
                .service_tier
                .as_ref()
                .and_then(|value| value.as_ref())
                .is_some()
        {
            state.chat_service_tier = identity.service_tier;
        }
        if state
            .chat_system_fingerprint
            .as_ref()
            .and_then(|value| value.as_ref())
            .is_none()
            && identity
                .system_fingerprint
                .as_ref()
                .and_then(|value| value.as_ref())
                .is_some()
        {
            state.chat_system_fingerprint = identity.system_fingerprint.clone();
            if let Some(Some(fingerprint)) = identity.system_fingerprint {
                state
                    .chat_top_level_extras
                    .insert("system_fingerprint".to_string(), json!(fingerprint));
            }
        }
    }
    if let Some(usage) = usage {
        state.chat_usage = Some(usage);
        state.chat_usage_source = object.get("usage").cloned();
    }
    Ok(validated)
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

    let validated = match validate_chat_chunk(chunk, state) {
        Ok(validated) => validated,
        Err(()) => return malformed_stream_error_events(state),
    };

    let mut events: Vec<AnthropicStreamEventData> = Vec::new();
    let (delta, finish_reason, usage, usage_present) = match validated {
        ValidatedChatChunk::UsageOnly(mut usage) => {
            // An include_usage chunk is only valid after a finish_reason queued
            // the terminal message delta, and only once. Keep success pending
            // until [DONE]/EOF so a later record cannot hide behind an already
            // emitted message_stop.
            if state.pending_message_delta.is_some() {
                if state.chat_terminal_usage_seen {
                    return malformed_stream_error_events(state);
                }
                if usage.service_tier.is_none() {
                    usage.service_tier = state
                        .chat_service_tier
                        .as_ref()
                        .and_then(|value| value.clone());
                }
                if let Some(fingerprint) = state
                    .chat_system_fingerprint
                    .as_ref()
                    .and_then(|value| value.as_ref())
                {
                    usage
                        .extra
                        .entry("chat_system_fingerprint".to_string())
                        .or_insert_with(|| json!(fingerprint));
                }
                state.chat_terminal_usage_seen = true;
                update_pending_message_usage(state, &usage);
                return events;
            }
            return malformed_stream_error_events(state);
        }
        ValidatedChatChunk::Choice(ValidatedChoice {
            delta,
            finish_reason,
            usage,
            usage_present,
        }) => (delta, finish_reason, usage, usage_present),
    };

    if handle_message_start(state, &mut events).is_err() {
        events.extend(malformed_stream_error_events(state));
        return events;
    }

    let source_content = delta
        .content
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let output_in_chunk = delta.has_content()
        || delta
            .reasoning_text
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || delta
            .reasoning_content
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || delta
            .reasoning_opaque
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || delta
            .refusal
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || !delta.tool_calls.is_empty();
    let refusal_fragment = delta.refusal.clone();

    state.chat_output_seen |= output_in_chunk;
    if let Some(content) = source_content.as_deref() {
        state.chat_content_seen.push_str(content);
    }
    if let Some(fragment) = refusal_fragment {
        state
            .chat_refusal_text
            .get_or_insert_with(String::new)
            .push_str(&fragment);
    }

    let reasoning_failed = if state.suppress_thinking {
        false
    } else {
        let reasoning_fallback = match handle_thinking_text(&delta, state, &mut events) {
            Ok(fallback) => fallback,
            Err(()) => {
                events.extend(malformed_stream_error_events(state));
                return events;
            }
        };
        reasoning_fallback
            .map(|text| schedule_text_fragment(text, false, state, &mut events))
            .transpose()
            .is_err()
            || schedule_reasoning_opaque(delta.reasoning_opaque.clone(), state, &mut events)
                .is_err()
    };
    if reasoning_failed
        || handle_tool_calls(&delta, state, &mut events).is_err()
        || source_content
            .map(|text| schedule_text_fragment(text, true, state, &mut events))
            .transpose()
            .is_err()
    {
        events.extend(malformed_stream_error_events(state));
        return events;
    }

    if let Some(finish_reason) = finish_reason {
        if handle_finish(
            &finish_reason,
            state,
            &mut events,
            usage.as_ref(),
            usage_present,
        )
        .is_err()
        {
            events.extend(malformed_stream_error_events(state));
        }
    }

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
        if complete_pending_message(state, &mut events, None).is_ok() {
            return events;
        }
        return terminal_stream_error_events(
            state,
            protocol_error_event("The translated model stream exceeded the output payload limit."),
        );
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

/// Terminal `error` event for a proxy-detected upstream stall.
///
/// A stall is a connection that stayed open and never errored but produced
/// nothing for the whole dead-air budget (see
/// [`crate::libs::sse::sse_stall_timeout`]). That is the same class of transient
/// failure as a mid-flight transport break, so it gets the same retryable
/// `overloaded_error` type rather than `api_error`: without it the client just
/// sees the stream stop, and reports a truncated response instead of retrying.
pub fn stalled_stream_error_event() -> AnthropicStreamEventData {
    AnthropicStreamEventData::Error {
        error: super::anthropic_types::AnthropicErrorBody {
            kind: "overloaded_error".to_string(),
            message: STALLED_STREAM_ERROR_MESSAGE.to_string(),
        },
    }
}

/// Client-visible message for a proxy-detected stall. Shared with the flows that
/// build the event through their own terminators.
pub const STALLED_STREAM_ERROR_MESSAGE: &str =
    "The upstream model stream stopped sending data. This is usually transient — retry the request.";

/// Terminate a translated stream after the proxy detects an upstream stall.
pub fn stalled_stream_error_events(
    state: &mut AnthropicStreamState,
) -> Vec<AnthropicStreamEventData> {
    terminal_stream_error_events(state, stalled_stream_error_event())
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
// State-machine steps (explicit source-order scheduler)
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

fn close_open_content_block(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    if !state.content_block_open {
        return;
    }
    let tool_block_open = is_tool_block_open(state);
    events.push(AnthropicStreamEventData::ContentBlockStop {
        index: state.content_block_index,
    });
    state.content_block_open = false;
    state.content_block_index += 1;
    if tool_block_open {
        state.active_tool_call_index = None;
    }
}

fn emit_text_fragment(
    text: &str,
    source_content: bool,
    already_budgeted: bool,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    if text.is_empty() {
        return Ok(());
    }
    if is_tool_block_open(state) {
        return Err(());
    }
    if !already_budgeted && !state.output_budget.try_reserve(text.len()) {
        return Err(());
    }
    if source_content {
        let start = state.chat_content_emitted.len();
        let end = start.checked_add(text.len()).ok_or(())?;
        if state
            .chat_content_seen
            .as_bytes()
            .get(start..end)
            .is_none_or(|expected| expected != text.as_bytes())
        {
            return Err(());
        }
    }
    close_thinking_block_if_open(state, events);
    if !state.content_block_open {
        events.push(AnthropicStreamEventData::ContentBlockStart {
            index: state.content_block_index,
            content_block: json!({"type":"text","text":""}),
        });
        state.content_block_open = true;
    }
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: state.content_block_index,
        delta: AnthropicContentBlockDelta::TextDelta {
            text: text.to_string(),
        },
    });
    if source_content {
        state.chat_content_emitted.push_str(text);
    }
    Ok(())
}

fn defer_text_fragment(
    text: String,
    source_content: bool,
    state: &mut AnthropicStreamState,
) -> Result<(), ()> {
    if text.is_empty() {
        return Ok(());
    }
    if !state.output_budget.try_reserve(text.len()) {
        return Err(());
    }
    if let Some(AnthropicStreamDeferredOutput::Text {
        text: existing,
        source_content: existing_source,
    }) = state.deferred_output.back_mut()
    {
        if *existing_source == source_content {
            existing.push_str(&text);
            return Ok(());
        }
    }
    state
        .deferred_output
        .push_back(AnthropicStreamDeferredOutput::Text {
            text,
            source_content,
        });
    Ok(())
}

fn defer_reasoning_opaque(signature: String, state: &mut AnthropicStreamState) -> Result<(), ()> {
    let additional = THINKING_TEXT.len().checked_add(signature.len()).ok_or(())?;
    if !state.output_budget.try_reserve(additional) {
        return Err(());
    }
    state
        .deferred_output
        .push_back(AnthropicStreamDeferredOutput::ReasoningOpaque(signature));
    Ok(())
}

/// `completePendingMessage` — flush the queued `message_delta` then `message_stop`.
fn update_pending_message_usage(state: &mut AnthropicStreamState, usage: &AnthropicUsage) {
    if let Some(AnthropicStreamEventData::MessageDelta {
        usage: pending_usage,
        ..
    }) = state.pending_message_delta.as_mut()
    {
        *pending_usage = Some(anthropic_delta_usage(usage, true));
    }
}

fn complete_pending_message(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
    usage: Option<&AnthropicUsage>,
) -> Result<(), ()> {
    if state.terminal_event_emitted {
        state.pending_message_delta = None;
        return Ok(());
    }

    let Some(pending) = state.pending_message_delta.clone() else {
        return Ok(());
    };

    let pending = match (usage, pending) {
        (Some(usage), AnthropicStreamEventData::MessageDelta { delta, usage: _ }) => {
            AnthropicStreamEventData::MessageDelta {
                delta,
                usage: Some(anthropic_delta_usage(usage, true)),
            }
        }
        (_, pending) => pending,
    };
    if let AnthropicStreamEventData::MessageDelta {
        usage: Some(usage), ..
    } = &pending
    {
        let additional = delta_usage_dynamic_payload_bytes(usage)?;
        if !state.output_budget.try_reserve(additional) {
            return Err(());
        }
    }

    let _ = state.pending_message_delta.take();
    events.push(pending);
    events.push(AnthropicStreamEventData::MessageStop);
    state.message_stop_emitted = true;
    state.terminal_event_emitted = true;
    Ok(())
}

fn anthropic_delta_usage(
    usage: &AnthropicUsage,
    include_input_tokens: bool,
) -> AnthropicMessageDeltaUsage {
    AnthropicMessageDeltaUsage {
        input_tokens: include_input_tokens.then_some(usage.input_tokens),
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        service_tier: usage.service_tier.clone(),
        extra: usage.extra.clone(),
    }
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
    state.deferred_output.clear();
    state.pending_message_delta = None;
}

fn flush_reconciled_refusal(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    if state.chat_content_emitted != state.chat_content_seen {
        return Err(());
    }
    let refusal = state.chat_refusal_text.as_deref().unwrap_or_default();
    if refusal.is_empty() || state.chat_content_seen.starts_with(refusal) {
        return Ok(());
    }
    let Some(suffix) = refusal.strip_prefix(&state.chat_content_seen) else {
        // Validation guarantees a prefix relation before a terminal chunk.
        return Err(());
    };
    if suffix.is_empty() {
        return Ok(());
    }
    let suffix = suffix.to_string();
    emit_text_fragment(&suffix, false, false, state, events)
}

fn schedule_terminal_output(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    close_thinking_block_if_open(state, events);
    if is_tool_block_open(state) {
        close_open_content_block(state, events);
    }
    flush_deferred_output(state, events)?;
    if !tool_scheduler_is_complete(state) {
        return Err(());
    }
    flush_reconciled_refusal(state, events)?;
    close_open_content_block(state, events);
    Ok(())
}

/// `handleFinish` — on a finishing chunk, close the open block, flush deferred
/// text, and queue the `message_delta`. Success is emitted only at [DONE]/EOF,
/// after every trailing upstream record has been validated.
fn handle_finish(
    finish_reason: &str,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
    usage: Option<&AnthropicUsage>,
    usage_present: bool,
) -> Result<(), ()> {
    // Already terminated: a well-behaved upstream sends one finishing chunk, but
    // an aggregator could send a second usage-bearing one. Ignore it so we never
    // emit a second message_delta/message_stop after the terminal stop.
    if state.message_stop_emitted {
        return Ok(());
    }
    schedule_terminal_output(state, events)?;

    let usage_is_known = usage.is_some() || state.chat_usage.is_some();
    let mut effective_usage = usage
        .cloned()
        .or_else(|| state.chat_usage.clone())
        .unwrap_or_else(|| {
            empty_chat_completion_usage(
                state
                    .chat_service_tier
                    .as_ref()
                    .and_then(|value| value.as_deref()),
            )
        });
    if effective_usage.service_tier.is_none() {
        effective_usage.service_tier = state
            .chat_service_tier
            .as_ref()
            .and_then(|value| value.clone());
    }
    if let Some(fingerprint) = state
        .chat_system_fingerprint
        .as_ref()
        .and_then(|value| value.as_ref())
    {
        effective_usage
            .extra
            .entry("chat_system_fingerprint".to_string())
            .or_insert_with(|| json!(fingerprint));
    }
    state.pending_message_delta = Some(AnthropicStreamEventData::MessageDelta {
        delta: AnthropicMessageDeltaBody {
            stop_reason: map_openai_stop_reason_to_anthropic(Some(finish_reason))
                .map(|s| s.to_string()),
            stop_sequence: None,
        },
        // A missing upstream usage object means "unknown", not an authoritative
        // zero input count. Omitting input_tokens prevents Claude Code from
        // replacing a previously known workflow count with zero. A later strict
        // include_usage chunk upgrades this pending delta to the full counters.
        usage: Some(anthropic_delta_usage(&effective_usage, usage_is_known)),
    });
    state.chat_finish_reason = Some(finish_reason.to_string());
    if usage_present {
        state.chat_terminal_usage_seen = true;
    }
    Ok(())
}

/// `handleToolCalls`.
fn handle_tool_calls(
    delta: &DeltaView,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    if !delta.has_tool_call_delta() {
        return Ok(());
    }

    close_thinking_block_if_open(state, events);

    for tool_call in &delta.tool_calls {
        let index = tool_call.index;
        reserve_tool_delta_payload(state, tool_call)?;
        if tool_call.first {
            state.tool_call_order.push(index);
            state.tool_calls.insert(
                index,
                AnthropicStreamToolCall {
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    anthropic_block_index: -1,
                    buffered_arguments: Vec::new(),
                    arguments: String::new(),
                    extra: tool_call.extra.clone(),
                    started: false,
                },
            );
        } else if let Some(info) = state.tool_calls.get_mut(&index) {
            if tool_call.id.is_some() {
                info.id = tool_call.id.clone();
            }
            if tool_call.name.is_some() {
                info.name = tool_call.name.clone();
            }
            // Validation already proved there is no conflict.
            let _ = merge_tool_extensions(&mut info.extra, &tool_call.extra);
        }

        if tool_call.first {
            let ready = state
                .tool_calls
                .get(&index)
                .is_some_and(|call| call.id.is_some() && call.name.is_some());
            if state.active_tool_call_index.is_none() && state.deferred_output.is_empty() && ready {
                start_tool_call(index, state, events)?;
            } else {
                state
                    .deferred_output
                    .push_back(AnthropicStreamDeferredOutput::ToolCall(index));
            }
        } else {
            try_start_front_tool_call(state, events)?;
        }

        if let Some(arguments) = &tool_call.arguments {
            if let Some(info) = state.tool_calls.get_mut(&index) {
                info.arguments.push_str(arguments);
                if state.active_tool_call_index == Some(index) {
                    events.push(AnthropicStreamEventData::ContentBlockDelta {
                        index: info.anthropic_block_index,
                        delta: AnthropicContentBlockDelta::InputJsonDelta {
                            partial_json: arguments.clone(),
                        },
                    });
                } else {
                    info.buffered_arguments.push(arguments.clone());
                }
            }
        }
        try_start_front_tool_call(state, events)?;
    }
    Ok(())
}

fn serialized_extension_bytes(extra: &Map<String, Value>) -> Result<usize, ()> {
    if extra.is_empty() {
        return Ok(0);
    }
    serde_json::to_vec(&Value::Object(extra.clone()))
        .map(|value| value.len())
        .map_err(|_| ())
}

fn usage_dynamic_payload_bytes(usage: &AnthropicUsage) -> Result<usize, ()> {
    usage
        .service_tier
        .as_ref()
        .map_or(0, String::len)
        .checked_add(serialized_extension_bytes(&usage.extra)?)
        .ok_or(())
}

fn delta_usage_dynamic_payload_bytes(usage: &AnthropicMessageDeltaUsage) -> Result<usize, ()> {
    usage
        .service_tier
        .as_ref()
        .map_or(0, String::len)
        .checked_add(serialized_extension_bytes(&usage.extra)?)
        .ok_or(())
}

fn reserve_tool_delta_payload(
    state: &mut AnthropicStreamState,
    delta: &ValidatedToolDelta,
) -> Result<(), ()> {
    let existing = state.tool_calls.get(&delta.index);
    let mut additional = delta.arguments.as_ref().map_or(0, String::len);
    if existing.and_then(|call| call.id.as_ref()).is_none() {
        additional = additional
            .checked_add(delta.id.as_ref().map_or(0, String::len))
            .ok_or(())?;
    }
    if existing.and_then(|call| call.name.as_ref()).is_none() {
        additional = additional
            .checked_add(delta.name.as_ref().map_or(0, String::len))
            .ok_or(())?;
    }
    let old_extra_size = existing
        .map(|call| serialized_extension_bytes(&call.extra))
        .transpose()?
        .unwrap_or(0);
    let new_extra_size = if let Some(existing) = existing {
        let mut merged = existing.extra.clone();
        merge_tool_extensions(&mut merged, &delta.extra)?;
        serialized_extension_bytes(&merged)?
    } else {
        serialized_extension_bytes(&delta.extra)?
    };
    additional = additional
        .checked_add(new_extra_size.saturating_sub(old_extra_size))
        .ok_or(())?;
    if !state.output_budget.try_reserve(additional) {
        return Err(());
    }
    Ok(())
}

fn try_start_front_tool_call(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    if state.active_tool_call_index.is_some() {
        return Ok(());
    }
    let Some(AnthropicStreamDeferredOutput::ToolCall(index)) =
        state.deferred_output.front().cloned()
    else {
        return Ok(());
    };
    let ready = state
        .tool_calls
        .get(&index)
        .is_some_and(|call| call.id.is_some() && call.name.is_some());
    if !ready {
        return Ok(());
    }
    let _ = state.deferred_output.pop_front();
    start_tool_call(index, state, events)
}

fn start_tool_call(
    index: i64,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    if state.active_tool_call_index.is_some() {
        return Err(());
    }
    let ready = state.tool_calls.get(&index).is_some_and(|call| {
        !call.started
            && call.id.as_deref().is_some_and(|id| !id.is_empty())
            && call.name.as_deref().is_some_and(|name| !name.is_empty())
    });
    if !ready {
        return Err(());
    }
    close_thinking_block_if_open(state, events);
    close_open_content_block(state, events);
    let info = state.tool_calls.get_mut(&index).ok_or(())?;
    info.anthropic_block_index = state.content_block_index;
    info.started = true;
    let mut block = Map::from_iter([
        ("type".to_string(), json!("tool_use")),
        (
            "id".to_string(),
            json!(info.id.as_deref().unwrap_or_default()),
        ),
        (
            "name".to_string(),
            json!(info.name.as_deref().unwrap_or_default()),
        ),
        ("input".to_string(), json!({})),
    ]);
    block.extend(info.extra.clone());
    events.push(AnthropicStreamEventData::ContentBlockStart {
        index: info.anthropic_block_index,
        content_block: Value::Object(block),
    });
    for partial_json in info.buffered_arguments.drain(..) {
        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: info.anthropic_block_index,
            delta: AnthropicContentBlockDelta::InputJsonDelta { partial_json },
        });
    }
    state.active_tool_call_index = Some(index);
    state.content_block_open = true;
    Ok(())
}

fn flush_deferred_output(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    while let Some(output) = state.deferred_output.pop_front() {
        match output {
            AnthropicStreamDeferredOutput::Text {
                text,
                source_content,
            } => {
                emit_text_fragment(&text, source_content, true, state, events)?;
            }
            AnthropicStreamDeferredOutput::ToolCall(index) => {
                close_open_content_block(state, events);
                start_tool_call(index, state, events)?;
                close_open_content_block(state, events);
            }
            AnthropicStreamDeferredOutput::ReasoningOpaque(signature) => {
                close_open_content_block(state, events);
                emit_complete_reasoning_opaque(&signature, true, events, state)?;
            }
        }
    }
    Ok(())
}

/// Every first-seen tool index must have exactly one scheduler entry unless it
/// is the currently active block.
fn tool_scheduler_is_complete(state: &AnthropicStreamState) -> bool {
    state.tool_call_order.iter().all(|index| {
        state
            .tool_calls
            .get(index)
            .is_some_and(|call| call.started && call.buffered_arguments.is_empty())
    })
}

fn schedule_text_fragment(
    text: String,
    source_content: bool,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    if text.is_empty() {
        return Ok(());
    }
    if state.active_tool_call_index.is_some() || !state.deferred_output.is_empty() {
        return defer_text_fragment(text, source_content, state);
    }
    emit_text_fragment(&text, source_content, false, state, events)
}

fn schedule_reasoning_opaque(
    signature: Option<String>,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    let Some(signature) = signature.filter(|signature| !signature.is_empty()) else {
        return Ok(());
    };
    let signature = encode_chat_reasoning_signature(&signature);
    if state.thinking_block_open {
        return close_thinking_block(state, events, signature, false);
    }
    if state.active_tool_call_index.is_some() || !state.deferred_output.is_empty() {
        return defer_reasoning_opaque(signature, state);
    }
    close_open_content_block(state, events);
    emit_complete_reasoning_opaque(&signature, false, events, state)
}

/// `handleMessageStart`.
fn handle_message_start(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), ()> {
    if state.message_start_sent {
        return Ok(());
    }

    let usage = state.chat_usage.clone().unwrap_or_else(|| {
        empty_chat_completion_usage(
            state
                .chat_service_tier
                .as_ref()
                .and_then(|value| value.as_deref()),
        )
    });
    let message = AnthropicMessageStart {
        id: state.chat_id.clone().expect("validated chat id"),
        kind: "message".to_string(),
        role: "assistant".to_string(),
        content: Vec::new(),
        model: state.chat_model.clone().expect("validated chat model"),
        stop_reason: None,
        stop_sequence: None,
        usage,
        extra: state.chat_top_level_extras.clone(),
    };
    let additional = serialized_extension_bytes(&message.extra)?
        .checked_add(usage_dynamic_payload_bytes(&message.usage)?)
        .ok_or(())?;
    if !state.output_budget.try_reserve(additional) {
        return Err(());
    }

    events.push(AnthropicStreamEventData::MessageStart { message });
    state.message_start_sent = true;
    Ok(())
}

/// `handleReasoningOpaque` — emit a complete thinking block (start, default
/// thinking_delta, signature_delta, stop) for an opaque reasoning blob.
fn emit_complete_reasoning_opaque(
    signature: &str,
    already_budgeted: bool,
    events: &mut Vec<AnthropicStreamEventData>,
    state: &mut AnthropicStreamState,
) -> Result<(), ()> {
    let additional = THINKING_TEXT.len().checked_add(signature.len()).ok_or(())?;
    if !already_budgeted && !state.output_budget.try_reserve(additional) {
        return Err(());
    }
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
    Ok(())
}

/// `handleThinkingText`.
fn handle_thinking_text(
    delta: &DeltaView,
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<Option<String>, ()> {
    // `delta.reasoning_text ?? delta.reasoning_content`
    let reasoning_text = delta
        .reasoning_text
        .clone()
        .or_else(|| delta.reasoning_content.clone());

    let Some(reasoning_text) = reasoning_text.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    // compatible with copilot API returning content->reasoning_text->reasoning_opaque
    // in different deltas; abnormal claude-model server behaviour.
    if state.content_block_open
        || state.active_tool_call_index.is_some()
        || !state.deferred_output.is_empty()
    {
        return Ok(Some(reasoning_text));
    }

    if !state.output_budget.try_reserve(reasoning_text.len()) {
        return Err(());
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
    Ok(None)
}

fn close_thinking_block(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
    signature: String,
    already_budgeted: bool,
) -> Result<(), ()> {
    if !state.thinking_block_open {
        return Ok(());
    }
    if !already_budgeted
        && !signature.is_empty()
        && !state.output_budget.try_reserve(signature.len())
    {
        return Err(());
    }
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: state.content_block_index,
        delta: AnthropicContentBlockDelta::SignatureDelta { signature },
    });
    events.push(AnthropicStreamEventData::ContentBlockStop {
        index: state.content_block_index,
    });
    state.content_block_index += 1;
    state.thinking_block_open = false;
    Ok(())
}

/// `closeThinkingBlockIfOpen`.
fn close_thinking_block_if_open(
    state: &mut AnthropicStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    if !state.thinking_block_open {
        return;
    }
    let _ = close_thinking_block(state, events, String::new(), true);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Older focused state-machine tests predate the audited
    /// ChatCompletionChunk envelope. Supply only the required stable envelope
    /// and first tool-call discriminator so those tests keep focusing on their
    /// intended lifecycle assertion. New contract tests call `super::` directly.
    fn translate_chunk_to_anthropic_events(
        chunk: &Value,
        state: &mut AnthropicStreamState,
    ) -> Vec<AnthropicStreamEventData> {
        if chunk.get("error").is_some_and(|error| !error.is_null()) {
            return super::translate_chunk_to_anthropic_events(chunk, state);
        }
        let mut chunk = chunk.clone();
        let Some(object) = chunk.as_object_mut() else {
            return super::translate_chunk_to_anthropic_events(&chunk, state);
        };
        object
            .entry("id")
            .or_insert_with(|| json!(state.chat_id.as_deref().unwrap_or("test-chunk")));
        object
            .entry("object")
            .or_insert_with(|| json!("chat.completion.chunk"));
        object
            .entry("created")
            .or_insert_with(|| json!(state.chat_created.unwrap_or(1)));
        object
            .entry("model")
            .or_insert_with(|| json!(state.chat_model.as_deref().unwrap_or("test-model")));
        if let Some(choices) = object.get_mut("choices").and_then(Value::as_array_mut) {
            for choice in choices {
                if let Some(choice_object) = choice.as_object_mut() {
                    choice_object.entry("index").or_insert_with(|| json!(0));
                }
                let Some(tool_calls) = choice
                    .get_mut("delta")
                    .and_then(|delta| delta.get_mut("tool_calls"))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                for tool_call in tool_calls {
                    let Some(index) = tool_call.get("index").and_then(nonnegative_i64) else {
                        continue;
                    };
                    if !state.tool_calls.contains_key(&index) {
                        if let Some(tool_call) = tool_call.as_object_mut() {
                            tool_call.entry("type").or_insert_with(|| json!("function"));
                        }
                    }
                }
            }
        }
        super::translate_chunk_to_anthropic_events(&chunk, state)
    }

    fn to_values(events: &[AnthropicStreamEventData]) -> Vec<Value> {
        events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    fn deferred_text(state: &AnthropicStreamState) -> String {
        state
            .deferred_output
            .iter()
            .filter_map(|output| match output {
                AnthropicStreamDeferredOutput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
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

    fn malformed_error_event() -> Value {
        json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": "The upstream model stream returned a malformed event."
            }
        })
    }

    fn pending_success_state() -> AnthropicStreamState {
        let mut state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id": "pending",
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
        state
    }

    fn open_thinking_state() -> AnthropicStreamState {
        let mut state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id": "thinking",
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
        state
    }

    fn open_tool_state() -> AnthropicStreamState {
        let mut state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id": "tool",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "function": {
                                "name": "lookup",
                                "arguments": "{\"q\":"
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            &mut state,
        );
        let deferred = translate_chunk_to_anthropic_events(
            &json!({
                "choices": [{
                    "index": 0,
                    "delta": { "content": "deferred" },
                    "finish_reason": null
                }]
            }),
            &mut state,
        );
        assert!(deferred.is_empty());
        assert!(state.content_block_open);
        assert_eq!(state.active_tool_call_index, Some(0));
        assert_eq!(deferred_text(&state), "deferred");
        assert!(!state.tool_calls.is_empty());
        state
    }

    fn assert_terminal_followups_are_suppressed(state: &mut AnthropicStreamState) {
        for late_chunk in [
            json!({
                "choices": [{
                    "index": 0,
                    "delta": { "content": "late success" },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1,
                    "total_tokens": 2
                }
            }),
            json!({
                "error": {
                    "type": "server_error",
                    "message": "late upstream error"
                }
            }),
            json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1,
                    "total_tokens": 2
                }
            }),
        ] {
            assert!(translate_chunk_to_anthropic_events(&late_chunk, state).is_empty());
        }
        assert!(flush_pending_anthropic_stream_events(state).is_empty());
    }

    fn assert_malformed_events_and_cleanup(
        got: Vec<Value>,
        expected_before_error: Vec<Value>,
        state: &mut AnthropicStreamState,
        chunk: &Value,
        context: &str,
    ) {
        let mut expected = expected_before_error;
        expected.push(malformed_error_event());
        assert_eq!(
            got, expected,
            "{context} state should terminate for malformed nested chunk: {chunk}"
        );
        assert_eq!(
            got.iter()
                .filter(|event| matches!(event["type"].as_str(), Some("error" | "message_stop")))
                .count(),
            1,
            "{context} state emitted more than one terminal event for: {chunk}"
        );
        assert!(!state.content_block_open);
        assert!(!state.thinking_block_open);
        assert!(state.active_tool_call_index.is_none());
        assert!(state.tool_calls.is_empty());
        assert!(state.tool_call_order.is_empty());
        assert!(state.deferred_output.is_empty());
        assert!(state.pending_message_delta.is_none());
        assert!(state.terminal_event_emitted);
        assert!(!state.message_stop_emitted);
        assert_terminal_followups_are_suppressed(state);
    }

    /// Exercise each malformed nested field in every state that previously
    /// allowed corruption to become a later success: fresh/non-pending,
    /// deferred-success pending, open thinking, and open tool/deferred content.
    fn assert_malformed_nested_chunk_is_terminal(chunk: Value) {
        let mut fresh = AnthropicStreamState::default();
        let got = to_values(&translate_chunk_to_anthropic_events(&chunk, &mut fresh));
        assert_malformed_events_and_cleanup(got, vec![], &mut fresh, &chunk, "fresh");

        let mut pending = pending_success_state();
        let got = to_values(&translate_chunk_to_anthropic_events(&chunk, &mut pending));
        assert_malformed_events_and_cleanup(got, vec![], &mut pending, &chunk, "pending");

        let mut thinking = open_thinking_state();
        let got = to_values(&translate_chunk_to_anthropic_events(&chunk, &mut thinking));
        assert_malformed_events_and_cleanup(
            got,
            vec![
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "signature_delta", "signature": "" }
                }),
                json!({ "type": "content_block_stop", "index": 0 }),
            ],
            &mut thinking,
            &chunk,
            "open-thinking",
        );

        let mut tool = open_tool_state();
        let got = to_values(&translate_chunk_to_anthropic_events(&chunk, &mut tool));
        assert_malformed_events_and_cleanup(
            got,
            vec![json!({ "type": "content_block_stop", "index": 0 })],
            &mut tool,
            &chunk,
            "open-tool",
        );
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
        let mut ev = translate_chunk_to_anthropic_events(&finish, &mut state);
        ev.extend(flush_pending_anthropic_stream_events(&mut state));
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
        let mut ev = translate_chunk_to_anthropic_events(&c4, &mut state);
        ev.extend(flush_pending_anthropic_stream_events(&mut state));
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
            "usage": { "prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6 }
        });
        all.extend(to_values(&translate_chunk_to_anthropic_events(
            &finish, &mut state,
        )));
        all.extend(to_values(&flush_pending_anthropic_stream_events(
            &mut state,
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
                "index": 0, "id": "call_1", "function": { "name": "f", "arguments": "{}" }
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
        assert_eq!(deferred_text(&state), "trailing");

        // Finish: tool block closes, deferred text flushed as its own block.
        let c3 = json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        });
        let mut ev = translate_chunk_to_anthropic_events(&c3, &mut state);
        ev.extend(flush_pending_anthropic_stream_events(&mut state));
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
    fn omitted_display_suppresses_streamed_reasoning() {
        let mut state = AnthropicStreamState {
            suppress_thinking: true,
            ..Default::default()
        };
        let events = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "id": "x",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "reasoning_text": "hidden reasoning",
                        "reasoning_opaque": "hidden signature",
                        "content": "visible answer"
                    },
                    "finish_reason": null
                }]
            }),
            &mut state,
        ));

        assert!(events.iter().all(|event| {
            event.pointer("/content_block/type").and_then(Value::as_str) != Some("thinking")
                && event.pointer("/delta/type").and_then(Value::as_str) != Some("thinking_delta")
                && event.pointer("/delta/type").and_then(Value::as_str) != Some("signature_delta")
        }));
        assert!(events.iter().any(|event| {
            event.pointer("/delta/text").and_then(Value::as_str) == Some("visible answer")
        }));
        assert!(!state.thinking_block_open);
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
                    "usage": { "output_tokens": 0 }
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

    /// A proxy-detected stall is the same class of transient failure as a
    /// mid-flight transport break, so it must carry the retryable
    /// `overloaded_error` type. If it were `api_error` the client would treat
    /// the wedged upstream as a permanent fault instead of retrying.
    #[test]
    fn stalled_stream_error_is_retryable_overloaded_error() {
        let value = serde_json::to_value(stalled_stream_error_event()).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], "overloaded_error");
        assert_eq!(value["error"]["message"], STALLED_STREAM_ERROR_MESSAGE);
    }

    /// The stall terminator must close an open content block first (otherwise the
    /// client is left with a dangling block) and must never emit twice.
    #[test]
    fn stalled_stream_closes_open_block_and_errors_once() {
        let mut state = AnthropicStreamState::default();
        let chunk = json!({
            "id": "x", "model": "m",
            "choices": [{ "index": 0, "delta": { "content": "partial" }, "finish_reason": null }]
        });
        let _ = translate_chunk_to_anthropic_events(&chunk, &mut state);

        let events = stalled_stream_error_events(&mut state);
        let kinds: Vec<String> = events
            .iter()
            .map(|e| {
                serde_json::to_value(e).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert!(
            kinds.contains(&"content_block_stop".to_string()),
            "open block must be closed before the terminal error: {kinds:?}"
        );
        assert_eq!(kinds.last().unwrap(), "error");

        // Terminal event is latched: a second call adds nothing.
        assert!(stalled_stream_error_events(&mut state).is_empty());
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
        assert_eq!(deferred_text(&tool_state), "deferred");
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
        assert!(tool_state.deferred_output.is_empty());
        assert!(tool_state.pending_message_delta.is_none());
        assert!(tool_state.terminal_event_emitted);
    }

    #[test]
    fn malformed_delta_and_reasoning_fields_are_terminal_in_every_state() {
        for chunk in [
            json!({
                "choices": [{
                    "delta": { "content": 42 },
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": { "content": [] },
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": { "reasoning_text": {} },
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": { "reasoning_content": 1.5 },
                    "finish_reason": null
                }]
            }),
            json!({
                "choices": [{
                    "delta": { "reasoning_opaque": false },
                    "finish_reason": null
                }]
            }),
        ] {
            assert_malformed_nested_chunk_is_terminal(chunk);
        }
    }

    #[test]
    fn malformed_tool_call_fields_are_terminal_in_every_state() {
        for tool_call in [
            json!({ "index": "bad" }),
            json!({ "index": 0.5 }),
            json!({ "index": -1 }),
            json!({ "index": 0, "id": 123 }),
            json!({ "index": 0, "function": [] }),
            json!({ "index": 0, "function": { "name": 99 } }),
            json!({ "index": 0, "function": { "arguments": {} } }),
        ] {
            assert_malformed_nested_chunk_is_terminal(json!({
                "choices": [{
                    "delta": { "tool_calls": [tool_call] },
                    "finish_reason": null
                }]
            }));
        }
    }

    #[test]
    fn malformed_usage_fields_are_terminal_in_every_state() {
        for chunk in [
            json!({
                "choices": [{ "delta": {}, "finish_reason": null }],
                "usage": { "prompt_tokens": "bad" }
            }),
            json!({
                "choices": [{ "delta": {}, "finish_reason": null }],
                "usage": { "completion_tokens": [] }
            }),
            json!({
                "choices": [{ "delta": {}, "finish_reason": null }],
                "usage": { "total_tokens": {} }
            }),
            json!({
                "choices": [],
                "usage": { "prompt_tokens": 1.5 }
            }),
            json!({
                "choices": [],
                "usage": { "completion_tokens": -1 }
            }),
            json!({
                "choices": [],
                "usage": { "prompt_tokens_details": [] }
            }),
            json!({
                "choices": [],
                "usage": {
                    "prompt_tokens_details": { "cached_tokens": "bad" }
                }
            }),
            json!({
                "choices": [],
                "usage": {
                    "prompt_tokens_details": {
                        "cache_creation_input_tokens": 0.25
                    }
                }
            }),
        ] {
            assert_malformed_nested_chunk_is_terminal(chunk);
        }
    }

    #[test]
    fn legitimate_null_omitted_and_fragmented_nested_fields_remain_valid() {
        let mut state = AnthropicStreamState::default();
        let announced = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "id": "null-and-fragmented",
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": null,
                        "reasoning_text": null,
                        "reasoning_content": null,
                        "reasoning_opaque": null,
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "function": {
                                "name": "lookup",
                                "arguments": null
                            }
                        }]
                    },
                    "finish_reason": null
                }],
                "usage": null
            }),
            &mut state,
        ));
        assert_eq!(announced.len(), 2);
        assert_eq!(announced[0]["type"], "message_start");
        assert_eq!(announced[1]["type"], "content_block_start");
        assert_eq!(announced[1]["content_block"]["id"], "call_1");
        assert_eq!(state.active_tool_call_index, Some(0));

        // Later argument fragments retain the required index while omitting
        // first-delta identity fields.
        for (fragment, expected) in [
            (
                json!({ "index": 0, "function": { "arguments": "{\"city\":" } }),
                "{\"city\":",
            ),
            (
                json!({
                    "index": 0,
                    "function": { "arguments": "\"Paris\"}" }
                }),
                "\"Paris\"}",
            ),
        ] {
            let got = to_values(&translate_chunk_to_anthropic_events(
                &json!({
                    "choices": [{
                        "delta": { "tool_calls": [fragment] },
                        "finish_reason": null
                    }]
                }),
                &mut state,
            ));
            assert_eq!(
                got,
                vec![json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": expected
                    }
                })]
            );
        }

        for no_op in [
            json!({
                "choices": [{
                    "delta": {
                        "content": null,
                        "reasoning_text": null,
                        "reasoning_content": null,
                        "reasoning_opaque": null,
                        "tool_calls": null
                    },
                    "finish_reason": null
                }],
                "usage": null
            }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": null
                    },
                    "finish_reason": null
                }]
            }),
        ] {
            assert!(translate_chunk_to_anthropic_events(&no_op, &mut state).is_empty());
        }

        let mut finish = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 0,
                    "completion_tokens": 0,
                    "total_tokens": 0,
                    "prompt_tokens_details": null
                }
            }),
            &mut state,
        ));
        finish.extend(to_values(&flush_pending_anthropic_stream_events(
            &mut state,
        )));
        assert_eq!(
            finish,
            vec![
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use" },
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "chat_prompt_tokens_details": null
                    }
                }),
                json!({ "type": "message_stop" }),
            ]
        );
        assert!(state.message_stop_emitted);
        assert!(state.terminal_event_emitted);
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
    fn flush_completes_normal_finish_once() {
        // A normal finishing chunk remains pending until the upstream boundary,
        // and repeated boundary flushes must not emit a second terminal close.
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
        assert!(!state.message_stop_emitted);
        assert!(state.pending_message_delta.is_some());

        let got = to_values(&flush_pending_anthropic_stream_events(&mut state));
        assert_eq!(
            got.iter()
                .filter(|event| event["type"] == "message_stop")
                .count(),
            1
        );
        assert!(state.message_stop_emitted);
        assert!(
            flush_pending_anthropic_stream_events(&mut state).is_empty(),
            "a second flush after a clean finish must be a no-op"
        );
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
        let mut got = to_values(&translate_chunk_to_anthropic_events(&finish, &mut state));
        got.extend(to_values(&flush_pending_anthropic_stream_events(
            &mut state,
        )));
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
    fn empty_choices_updates_pending_usage_until_stream_boundary() {
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
                service_tier: None,
                extra: Map::new(),
            }),
        });

        let chunk = json!({
            "choices": [],
            "error": null,
            "usage": { "prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6 },
        });
        let ev = translate_chunk_to_anthropic_events(&chunk, &mut state);
        let got = to_values(&ev);
        assert!(got.is_empty());
        assert!(state.pending_message_delta.is_some());
        assert!(state.chat_terminal_usage_seen);
        assert!(!state.message_stop_emitted);

        let got = to_values(&flush_pending_anthropic_stream_events(&mut state));
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

    #[test]
    fn refusal_accumulation_enforces_response_bound() {
        let exact = "x".repeat(crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES);
        let mut state = AnthropicStreamState {
            chat_id: Some("bounded".to_string()),
            chat_model: Some("m".to_string()),
            chat_created: Some(1),
            chat_refusal_text: Some(exact),
            chat_output_seen: true,
            ..Default::default()
        };
        let empty = DeltaView {
            content: None,
            reasoning_text: None,
            reasoning_content: None,
            reasoning_opaque: None,
            refusal: None,
            tool_calls: Vec::new(),
        };
        let reconciliation =
            reconcile_refusal_content(&empty, &state).expect("exact bound is accepted");
        assert_eq!(
            reconciliation.refusal_len,
            crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
        );

        let events = to_values(&super::translate_chunk_to_anthropic_events(
            &json!({
                "id":"bounded",
                "object":"chat.completion.chunk",
                "created":1,
                "model":"m",
                "choices":[{
                    "index":0,
                    "delta":{"refusal":"x"},
                    "finish_reason":null
                }]
            }),
            &mut state,
        ));
        assert_eq!(events, vec![malformed_error_event()]);
        assert!(state.terminal_event_emitted);
        assert!(!state.message_stop_emitted);
    }

    #[test]
    fn tool_deferred_refusal_scheduler_emits_prefix_before_suffix() {
        let mut state = AnthropicStreamState::default();
        let mut all = Vec::new();
        for chunk in [
            json!({
                "id":"scheduled",
                "model":"m",
                "choices":[{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"call",
                        "function":{"name":"actual","arguments":"{}"}
                    }]},
                    "finish_reason":null
                }]
            }),
            json!({
                "choices":[{
                    "index":0,
                    "delta":{"content":"foo","refusal":"foobar"},
                    "finish_reason":null
                }]
            }),
            json!({
                "choices":[{
                    "index":0,
                    "delta":{},
                    "finish_reason":"content_filter"
                }]
            }),
        ] {
            all.extend(to_values(&translate_chunk_to_anthropic_events(
                &chunk, &mut state,
            )));
        }
        all.extend(to_values(&flush_pending_anthropic_stream_events(
            &mut state,
        )));

        let ordered: Vec<_> = all
            .iter()
            .filter_map(|event| match event["type"].as_str()? {
                "content_block_start" => Some(format!(
                    "start:{}:{}",
                    event["index"],
                    event["content_block"]["type"].as_str().unwrap_or_default()
                )),
                "content_block_delta" => Some(format!(
                    "delta:{}:{}",
                    event["index"],
                    event
                        .pointer("/delta/text")
                        .or_else(|| event.pointer("/delta/partial_json"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                )),
                "content_block_stop" => Some(format!("stop:{}", event["index"])),
                "message_delta" => Some(format!(
                    "terminal:{}",
                    event["delta"]["stop_reason"].as_str().unwrap_or_default()
                )),
                "message_stop" => Some("message_stop".to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ordered,
            vec![
                "start:0:tool_use",
                "delta:0:{}",
                "stop:0",
                "start:1:text",
                "delta:1:foo",
                "delta:1:bar",
                "stop:1",
                "terminal:refusal",
                "message_stop",
            ]
        );
        assert_eq!(state.chat_content_seen, "foo");
        assert_eq!(state.chat_content_emitted, "foo");
        assert_eq!(state.output_budget.used_bytes, 18);
    }

    #[test]
    fn incomplete_tool_content_filter_discards_deferred_text_and_errors_once() {
        let mut state = AnthropicStreamState::default();
        let _ = translate_chunk_to_anthropic_events(
            &json!({
                "id":"incomplete",
                "model":"m",
                "choices":[{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"call",
                        "function":{"name":"actual","arguments":"{\"value\":"}
                    }]},
                    "finish_reason":null
                }]
            }),
            &mut state,
        );
        assert!(translate_chunk_to_anthropic_events(
            &json!({
                "choices":[{
                    "index":0,
                    "delta":{"content":"foo","refusal":"foobar"},
                    "finish_reason":null
                }]
            }),
            &mut state,
        )
        .is_empty());

        let events = to_values(&translate_chunk_to_anthropic_events(
            &json!({
                "choices":[{
                    "index":0,
                    "delta":{},
                    "finish_reason":"content_filter"
                }]
            }),
            &mut state,
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1
        );
        assert!(!events.iter().any(|event| event["type"] == "message_stop"));
        assert!(state.deferred_output.is_empty());
    }

    #[test]
    fn deferred_reasoning_fallback_does_not_overwrite_source_content() {
        let mut state = AnthropicStreamState::default();
        let mut all = Vec::new();
        for chunk in [
            json!({
                "id":"reasoning-order",
                "model":"m",
                "choices":[{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"call",
                        "function":{"name":"actual","arguments":"{}"}
                    }]},
                    "finish_reason":null
                }]
            }),
            json!({
                "choices":[{
                    "index":0,
                    "delta":{"reasoning_text":"thought","content":"foo"},
                    "finish_reason":null
                }]
            }),
            json!({
                "choices":[{
                    "index":0,
                    "delta":{},
                    "finish_reason":"tool_calls"
                }]
            }),
        ] {
            all.extend(to_values(&translate_chunk_to_anthropic_events(
                &chunk, &mut state,
            )));
        }
        all.extend(to_values(&flush_pending_anthropic_stream_events(
            &mut state,
        )));
        let text: String = all
            .iter()
            .filter_map(|event| event.pointer("/delta/text").and_then(Value::as_str))
            .collect();
        assert_eq!(text, "thoughtfoo");
        assert_eq!(state.chat_content_seen, "foo");
        assert_eq!(state.chat_content_emitted, "foo");
        assert_eq!(state.output_budget.used_bytes, 22);
        assert_single_open_block_invariant(&all);
    }

    #[test]
    fn deferred_opaque_reasoning_keeps_block_order_behind_tool() {
        let mut state = AnthropicStreamState::default();
        let mut all = Vec::new();
        for chunk in [
            json!({
                "id":"opaque-order",
                "model":"m",
                "choices":[{
                    "index":0,
                    "delta":{"tool_calls":[{
                        "index":0,
                        "id":"call",
                        "function":{"name":"actual","arguments":"{}"}
                    }]},
                    "finish_reason":null
                }]
            }),
            json!({
                "choices":[{
                    "index":0,
                    "delta":{
                        "reasoning_text":"thought",
                        "reasoning_opaque":"signature",
                        "content":"foo"
                    },
                    "finish_reason":null
                }]
            }),
            json!({
                "choices":[{
                    "index":0,
                    "delta":{},
                    "finish_reason":"tool_calls"
                }]
            }),
        ] {
            all.extend(to_values(&translate_chunk_to_anthropic_events(
                &chunk, &mut state,
            )));
        }
        all.extend(to_values(&flush_pending_anthropic_stream_events(
            &mut state,
        )));
        assert_eq!(
            all.iter()
                .filter_map(|event| match event["type"].as_str()? {
                    "content_block_start" => Some(format!(
                        "start:{}:{}",
                        event["index"],
                        event["content_block"]["type"].as_str().unwrap_or_default()
                    )),
                    "content_block_delta" => Some(format!(
                        "delta:{}:{}",
                        event["index"],
                        event["delta"]["type"].as_str().unwrap_or_default()
                    )),
                    "content_block_stop" => Some(format!("stop:{}", event["index"])),
                    "message_delta" => Some("terminal".to_string()),
                    "message_stop" => Some("message_stop".to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![
                "start:0:tool_use",
                "delta:0:input_json_delta",
                "stop:0",
                "start:1:text",
                "delta:1:text_delta",
                "stop:1",
                "start:2:thinking",
                "delta:2:thinking_delta",
                "delta:2:signature_delta",
                "stop:2",
                "start:3:text",
                "delta:3:text_delta",
                "stop:3",
                "terminal",
                "message_stop",
            ]
        );
        let expected_signature = encode_chat_reasoning_signature("signature");
        assert_eq!(
            all.iter()
                .find(|event| event["delta"]["type"] == "signature_delta")
                .and_then(|event| event["delta"]["signature"].as_str()),
            Some(expected_signature.as_str())
        );
        assert_eq!(state.chat_content_seen, "foo");
        assert_eq!(state.chat_content_emitted, "foo");
        assert_eq!(
            state.output_budget.used_bytes,
            42 + encode_chat_reasoning_signature("signature").len() - "signature".len()
        );
        assert_single_open_block_invariant(&all);
    }

    #[test]
    fn deferred_and_emitted_text_bounds_accept_limit_then_fail_once() {
        let exact = "x".repeat(crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES);

        let mut emitted_state = AnthropicStreamState::default();
        let mut emitted_events = Vec::new();
        emit_text_fragment(
            &exact,
            false,
            false,
            &mut emitted_state,
            &mut emitted_events,
        )
        .expect("exact emitted-text bound is accepted");
        let event_count = emitted_events.len();
        assert_eq!(
            emitted_state.output_budget.used_bytes,
            crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
        );
        assert!(
            emit_text_fragment("x", false, false, &mut emitted_state, &mut emitted_events).is_err()
        );
        assert_eq!(emitted_events.len(), event_count);

        let mut deferred_state = AnthropicStreamState {
            chat_id: Some("deferred-bound".to_string()),
            chat_model: Some("m".to_string()),
            chat_created: Some(1),
            message_start_sent: true,
            content_block_open: true,
            active_tool_call_index: Some(0),
            ..Default::default()
        };
        deferred_state.tool_call_order.push(0);
        deferred_state.tool_calls.insert(
            0,
            AnthropicStreamToolCall {
                id: Some("call".to_string()),
                name: Some("actual".to_string()),
                anthropic_block_index: 0,
                arguments: "{}".to_string(),
                started: true,
                ..Default::default()
            },
        );
        defer_text_fragment(exact, false, &mut deferred_state)
            .expect("exact deferred-text bound is accepted");
        assert_eq!(
            deferred_state.output_budget.used_bytes,
            crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
        );

        let events = to_values(&super::translate_chunk_to_anthropic_events(
            &json!({
                "id":"deferred-bound",
                "object":"chat.completion.chunk",
                "created":1,
                "model":"m",
                "choices":[{
                    "index":0,
                    "delta":{"content":"x"},
                    "finish_reason":null
                }]
            }),
            &mut deferred_state,
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "error")
                .count(),
            1
        );
        assert!(!events.iter().any(|event| event["type"] == "message_stop"));
        assert!(deferred_state.deferred_output.is_empty());
        assert_eq!(
            deferred_state.output_budget.used_bytes,
            crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
        );
    }

    #[test]
    fn repeated_opaque_reasoning_uses_exact_aggregate_budget() {
        const PARTS: usize = 128;
        let carrier_overhead = encode_chat_reasoning_signature("").len();
        let fixed_bytes = (THINKING_TEXT.len() + carrier_overhead) * PARTS;
        let signature_bytes = crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES - fixed_bytes;
        let base = signature_bytes / PARTS;
        let remainder = signature_bytes % PARTS;
        let mut state = AnthropicStreamState::default();
        for index in 0..PARTS {
            let events = translate_chunk_to_anthropic_events(
                &json!({
                    "id":"opaque-budget",
                    "model":"m",
                    "choices":[{
                        "index":0,
                        "delta":{"reasoning_opaque":"s".repeat(base + usize::from(index < remainder))},
                        "finish_reason":null
                    }]
                }),
                &mut state,
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AnthropicStreamEventData::Error { .. })),
                "part {index}"
            );
        }
        assert_eq!(
            state.output_budget.used_bytes,
            crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES
        );
        let finish = translate_chunk_to_anthropic_events(
            &json!({
                "id":"opaque-budget",
                "model":"m",
                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]
            }),
            &mut state,
        );
        assert!(
            !finish
                .iter()
                .any(|event| matches!(event, AnthropicStreamEventData::Error { .. })),
            "finish={finish:?} state={state:?}"
        );
        assert!(flush_pending_anthropic_stream_events(&mut state)
            .iter()
            .any(|event| matches!(event, AnthropicStreamEventData::MessageStop)));
    }
}
