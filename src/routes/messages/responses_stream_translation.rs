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
use sha2::{Digest, Sha256};

use crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES;
use crate::libs::tool_search::{format_tool_search_bridge_arguments, BRIDGE_TOOL_SEARCH_NAME};

use super::anthropic_types::{
    AnthropicContentBlockDelta, AnthropicErrorBody, AnthropicMessageDeltaBody,
    AnthropicMessageDeltaUsage, AnthropicMessageStart, AnthropicStreamEventData, AnthropicUsage,
    TranslatedOutputBudget,
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
const MAX_TRANSLATED_ERROR_MESSAGE_BYTES: usize = 1024;
const DEFAULT_TRANSLATED_ERROR_MESSAGE: &str = "The upstream Responses stream reported an error.";

/// Imported from [`super::utils`] so all translation modules share one source of
/// truth for the "Thinking..." placeholder.
use super::utils::THINKING_TEXT;

/// Shared with [`super::responses_translation`] so the byte-exact compaction
/// carrier signature (`cm1#{enc}@{id}`) has a single source of truth — the
/// signature must match exactly for Copilot cache hits, so a divergent second
/// copy would silently corrupt them.
use super::responses_translation::{
    canonical_anthropic_output_item, effective_reasoning_text, encode_compaction_carrier_signature,
    encode_reasoning_signature, optional_nonnull_string_field,
    parse_and_validate_anthropic_output_item, reconcile_tool_search_call_id, stable_tool_use_id,
    validate_created_status, validate_function_arguments, validate_output_item_reconciliation,
    validate_raw_responses_usage, validate_terminal_status, OutputValidationPhase,
    ResponsesTerminalKind, ValidatedResponsesUsage,
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
    pub accumulated_arguments: String,
    pub arguments_done: bool,
    pub started: bool,
    pub done: bool,
}

type OutputItemDigest = [u8; 32];

#[derive(Debug, Clone)]
pub struct OutputItemLifecycle {
    pub item_type: String,
    pub item_id: Option<String>,
    pub done: bool,
    /// The provisional item is needed only until its authoritative `done`
    /// counterpart has been reconciled. Completed payloads are represented by
    /// fixed-size digests so already-rendered text/arguments do not remain
    /// charged as historical retained state.
    pub pending_item: Option<Value>,
    pub pending_item_bytes: usize,
    pub initial_digest: OutputItemDigest,
    pub final_digest: Option<OutputItemDigest>,
    pub done_event_digest: Option<OutputItemDigest>,
    pub completed_incomplete: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ReasoningSummaryPartState {
    pub text: String,
    pub done: bool,
    pub added: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ReasoningItemStreamState {
    pub item_id: Option<String>,
    pub summary_parts: BTreeMap<i64, ReasoningSummaryPartState>,
    pub content_parts: BTreeMap<i64, String>,
}

#[derive(Debug, Clone, Default)]
pub struct OutputTextStreamState {
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RetainedStateOwner {
    SequenceSnapshot,
    CreatedResponseId,
    BlockKey(i64, i64),
    FunctionMetadata(i64),
    FunctionArguments(i64),
    PendingOutputItem(i64),
    OutputItemMetadata(i64),
    OutputItemIdIndex(i64),
    ReasoningItemId(i64),
    ReasoningSummary(i64, i64),
    ReasoningContent(i64, i64),
    OutputTextKey(i64, i64),
    OutputText(i64, i64),
}

#[derive(Debug, Clone, Default)]
pub struct RetainedStateBudget {
    used_bytes: usize,
    owners: HashMap<RetainedStateOwner, usize>,
}

impl RetainedStateBudget {
    #[cfg(test)]
    fn reserve(&mut self, owner: RetainedStateOwner, bytes: usize) -> Result<(), &'static str> {
        if bytes == 0 {
            return Ok(());
        }
        if self.owners.contains_key(&owner) {
            return Err("The Responses stream retained-state owner was reserved more than once.");
        }
        self.replace(owner, bytes)
    }

    #[cfg(test)]
    fn release(&mut self, owner: RetainedStateOwner) -> Result<(), &'static str> {
        if !self.owners.contains_key(&owner) {
            return Err("The Responses stream released an unowned retained-state buffer.");
        }
        self.replace(owner, 0)
    }

    fn replace(&mut self, owner: RetainedStateOwner, bytes: usize) -> Result<(), &'static str> {
        let old = self.owners.get(&owner).copied().unwrap_or(0);
        let without_old = self
            .used_bytes
            .checked_sub(old)
            .ok_or("The Responses stream retained-state accounting underflowed.")?;
        let total = without_old
            .checked_add(bytes)
            .ok_or("The Responses stream exceeded the translation buffer limit.")?;
        if total > MAX_BUFFERED_TRANSLATION_BYTES {
            return Err("The Responses stream exceeded the translation buffer limit.");
        }
        self.used_bytes = total;
        if bytes == 0 {
            self.owners.remove(&owner);
        } else {
            self.owners.insert(owner, bytes);
        }
        debug_assert_eq!(
            self.used_bytes,
            self.owners.values().copied().sum::<usize>()
        );
        Ok(())
    }

    fn clear(&mut self) {
        self.used_bytes = 0;
        self.owners.clear();
    }

    #[cfg(test)]
    fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    #[cfg(test)]
    fn owner_bytes(&self, owner: RetainedStateOwner) -> usize {
        self.owners.get(&owner).copied().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy)]
enum RetainedBudgetMutation {
    Reserve(RetainedStateOwner, usize),
    Replace(RetainedStateOwner, usize),
    Release(RetainedStateOwner),
}

/// A pure preflight plan for one logical Responses translation transition.
///
/// Output bytes and every retained-owner mutation are checked against snapshots
/// of both budgets. [`commit`](Self::commit) mutates the real counters only after
/// all arithmetic, capacity, duplicate-owner, and release-owner checks pass.
/// A failed preflight therefore leaves both budgets byte-for-byte unchanged.
#[derive(Debug, Default)]
struct ResponsesBudgetTransaction {
    output_delta: usize,
    retained_mutations: Vec<RetainedBudgetMutation>,
}

struct ResponsesBudgetCommit {
    output_used_bytes: usize,
    retained_used_bytes: usize,
    retained_updates: HashMap<RetainedStateOwner, Option<usize>>,
}

impl ResponsesBudgetTransaction {
    fn reserve_output(&mut self, additional: usize) -> Result<(), &'static str> {
        self.output_delta = self
            .output_delta
            .checked_add(additional)
            .ok_or("The Responses stream exceeded the translated output payload limit.")?;
        Ok(())
    }

    fn reserve_retained(&mut self, owner: RetainedStateOwner, bytes: usize) {
        if bytes > 0 {
            self.retained_mutations
                .push(RetainedBudgetMutation::Reserve(owner, bytes));
        }
    }

    fn replace_retained(&mut self, owner: RetainedStateOwner, bytes: usize) {
        self.retained_mutations
            .push(RetainedBudgetMutation::Replace(owner, bytes));
    }

    fn release_retained(&mut self, owner: RetainedStateOwner) {
        self.retained_mutations
            .push(RetainedBudgetMutation::Release(owner));
    }

    fn preflight(
        &self,
        state: &ResponsesStreamState,
    ) -> Result<ResponsesBudgetCommit, &'static str> {
        let output_used_bytes = state
            .output_budget
            .used_bytes
            .checked_add(self.output_delta)
            .filter(|total| *total <= MAX_UPSTREAM_RESPONSE_BYTES)
            .ok_or("The Responses stream exceeded the translated output payload limit.")?;

        let mut retained_used_bytes = state.retained_budget.used_bytes;
        let mut retained_updates: HashMap<RetainedStateOwner, Option<usize>> = HashMap::new();
        for mutation in &self.retained_mutations {
            let owner = match mutation {
                RetainedBudgetMutation::Reserve(owner, _)
                | RetainedBudgetMutation::Replace(owner, _)
                | RetainedBudgetMutation::Release(owner) => *owner,
            };
            let current = retained_updates
                .get(&owner)
                .copied()
                .unwrap_or_else(|| state.retained_budget.owners.get(&owner).copied());
            let replacement = match *mutation {
                RetainedBudgetMutation::Reserve(_, bytes) => {
                    if current.is_some() {
                        return Err(
                            "The Responses stream retained-state owner was reserved more than once.",
                        );
                    }
                    Some(bytes)
                }
                RetainedBudgetMutation::Replace(_, bytes) => (bytes > 0).then_some(bytes),
                RetainedBudgetMutation::Release(_) => {
                    if current.is_none() {
                        return Err(
                            "The Responses stream released an unowned retained-state buffer.",
                        );
                    }
                    None
                }
            };
            retained_used_bytes = retained_used_bytes
                .checked_sub(current.unwrap_or(0))
                .ok_or("The Responses stream retained-state accounting underflowed.")?
                .checked_add(replacement.unwrap_or(0))
                .ok_or("The Responses stream exceeded the translation buffer limit.")?;
            retained_updates.insert(owner, replacement);
        }
        if retained_used_bytes > MAX_BUFFERED_TRANSLATION_BYTES {
            return Err("The Responses stream exceeded the translation buffer limit.");
        }
        Ok(ResponsesBudgetCommit {
            output_used_bytes,
            retained_used_bytes,
            retained_updates,
        })
    }

    fn commit(self, state: &mut ResponsesStreamState) -> Result<(), &'static str> {
        let commit = self.preflight(state)?;
        state.output_budget.used_bytes = commit.output_used_bytes;
        state.retained_budget.used_bytes = commit.retained_used_bytes;
        for (owner, replacement) in commit.retained_updates {
            if let Some(bytes) = replacement {
                state.retained_budget.owners.insert(owner, bytes);
            } else {
                state.retained_budget.owners.remove(&owner);
            }
        }
        debug_assert_eq!(
            state.retained_budget.used_bytes,
            state
                .retained_budget
                .owners
                .values()
                .copied()
                .sum::<usize>()
        );
        Ok(())
    }
}

/// Mirrors the TS `ResponsesStreamState`.
#[derive(Debug, Clone)]
pub struct ResponsesStreamState {
    pub message_start_sent: bool,
    pub message_completed: bool,
    pub translation_failed: bool,
    pub created_output_digests: Option<Vec<OutputItemDigest>>,
    pub fallback_model: Option<String>,
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
    pub last_sequence_event_bytes: usize,
    pub retained_budget: RetainedStateBudget,
    pub output_budget: TranslatedOutputBudget,
    pub tracked_reasoning_parts: usize,
    pub tracked_text_parts: usize,
    pub output_text_by_key: HashMap<String, OutputTextStreamState>,
    pub tool_search_name: String,
    pub has_tool_call: bool,
}

impl ResponsesStreamState {
    /// Mirrors the TS `createResponsesStreamState({ toolSearchName })`.
    pub fn new(tool_search_name: Option<String>) -> Self {
        Self::new_with_model(tool_search_name, None)
    }

    pub fn new_with_model(
        tool_search_name: Option<String>,
        fallback_model: Option<String>,
    ) -> Self {
        Self {
            message_start_sent: false,
            message_completed: false,
            translation_failed: false,
            created_output_digests: None,
            fallback_model: fallback_model.filter(|model| !model.trim().is_empty()),
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
            last_sequence_event_bytes: 0,
            retained_budget: RetainedStateBudget::default(),
            output_budget: TranslatedOutputBudget::default(),
            tracked_reasoning_parts: 0,
            tracked_text_parts: 0,
            output_text_by_key: HashMap::new(),
            tool_search_name: tool_search_name
                .filter(|name| !name.trim().is_empty())
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
    let message = if message.is_empty()
        || message.len() > MAX_TRANSLATED_ERROR_MESSAGE_BYTES
        || message.chars().any(char::is_control)
    {
        DEFAULT_TRANSLATED_ERROR_MESSAGE
    } else {
        message
    };
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
    state.translation_failed = true;
    events
}

// ---------------------------------------------------------------------------
// Small accessors
// ---------------------------------------------------------------------------

fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn required_string_field<'a>(
    value: &'a Value,
    field: &str,
    error: &'static str,
) -> Result<&'a str, &'static str> {
    value.get(field).and_then(Value::as_str).ok_or(error)
}

fn required_nonempty_string_field<'a>(
    value: &'a Value,
    field: &str,
    error: &'static str,
) -> Result<&'a str, &'static str> {
    required_string_field(value, field, error).and_then(|field| {
        if field.trim().is_empty() {
            Err(error)
        } else {
            Ok(field)
        }
    })
}

fn optional_string_field<'a>(
    value: &'a Value,
    field: &str,
    error: &'static str,
) -> Result<Option<&'a str>, &'static str> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(error),
    }
}

fn optional_array_field<'a>(
    value: &'a Value,
    field: &str,
    error: &'static str,
) -> Result<Option<&'a Vec<Value>>, &'static str> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(value)) => Ok(Some(value)),
        Some(_) => Err(error),
    }
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

pub(crate) fn validate_event_sequence(
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
    let snapshot_bytes = serde_json::to_vec(event)
        .map_err(|_| "A Responses stream event could not be serialized.")?
        .len();
    state
        .retained_budget
        .replace(RetainedStateOwner::SequenceSnapshot, snapshot_bytes)?;
    state.last_sequence_event_bytes = snapshot_bytes;
    state.last_sequence_number = Some(sequence);
    state.last_sequence_event = Some(event.clone());
    Ok(false)
}

fn replace_retained_state_bytes(
    state: &mut ResponsesStreamState,
    owner: RetainedStateOwner,
    bytes: usize,
) -> Result<(), &'static str> {
    state.retained_budget.replace(owner, bytes)
}

#[cfg(test)]
fn reserve_function_call_metadata_if_new(
    state: &mut ResponsesStreamState,
    output_index: i64,
    tool_call_id: Option<&str>,
    name: &str,
) -> Result<(), &'static str> {
    let mut transaction = ResponsesBudgetTransaction::default();
    plan_function_call_metadata_if_new(state, &mut transaction, output_index, tool_call_id, name)?;
    transaction.commit(state)
}

fn plan_function_call_metadata_if_new(
    state: &ResponsesStreamState,
    transaction: &mut ResponsesBudgetTransaction,
    output_index: i64,
    tool_call_id: Option<&str>,
    name: &str,
) -> Result<(), &'static str> {
    if let Some(existing) = state.function_call_state_by_output_index.get(&output_index) {
        if tool_call_id
            .filter(|id| !id.is_empty())
            .is_some_and(|id| id != existing.tool_call_id)
        {
            return Err("A completed function/tool call changed its call id.");
        }
        if !name.is_empty() && name != existing.name {
            return Err("A completed function/tool call changed its function name.");
        }
        return Ok(());
    }
    let id = stable_tool_use_id(tool_call_id, None, output_index);
    let additional = id
        .len()
        .checked_add(name.len())
        .ok_or("The Responses stream exceeded the translated output payload limit.")?;
    transaction.reserve_output(additional)?;
    transaction.reserve_retained(
        RetainedStateOwner::FunctionMetadata(output_index),
        additional,
    );
    Ok(())
}

fn serialized_json_value_bytes(value: &Value) -> Result<usize, &'static str> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| "A Responses stream value could not be serialized.")
}

fn semantically_ordered_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.iter().map(semantically_ordered_json).collect())
        }
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_unstable();
            let mut ordered = serde_json::Map::new();
            for key in keys {
                ordered.insert(key.clone(), semantically_ordered_json(&object[key]));
            }
            Value::Object(ordered)
        }
        scalar => scalar.clone(),
    }
}

fn raw_output_item_digest(value: &Value) -> Result<OutputItemDigest, &'static str> {
    // `serde_json` preserves insertion order for forwarding, while lifecycle
    // reconciliation has always been semantic: reordering object members must
    // not turn an identical replay into a conflict. Sort object keys only in the
    // fixed-size digest representation; arrays and client-visible wire bytes
    // remain untouched.
    let bytes = serde_json::to_vec(&semantically_ordered_json(value))
        .map_err(|_| "A Responses output item could not be serialized for reconciliation.")?;
    Ok(Sha256::digest(bytes).into())
}

fn canonical_output_item_digest(
    value: &Value,
    phase: OutputValidationPhase,
) -> Result<OutputItemDigest, &'static str> {
    let mut canonical = canonical_anthropic_output_item(value, phase)?;
    if let Some(item) = canonical.as_object_mut() {
        // Copilot re-encrypts opaque IDs and encrypted reasoning payloads
        // independently for added, delta, done, and terminal snapshots. Exclude
        // only those transport envelopes from semantic lifecycle reconciliation.
        item.remove("id");
        item.remove("encrypted_content");
    }
    raw_output_item_digest(&canonical)
}

fn output_array_digests(
    output: &[Value],
    phase: OutputValidationPhase,
) -> Result<Vec<OutputItemDigest>, &'static str> {
    output
        .iter()
        .map(|item| canonical_output_item_digest(item, phase))
        .collect()
}

fn retain_late_output_item_id(
    state: &mut ResponsesStreamState,
    output_index: i64,
    item_id: &str,
) -> Result<(), &'static str> {
    let mut transaction = ResponsesBudgetTransaction::default();
    let update = plan_late_output_item_id(state, &mut transaction, output_index, item_id)?;
    transaction.commit(state)?;
    if update {
        apply_late_output_item_id(state, output_index, item_id);
    }
    Ok(())
}

fn plan_late_output_item_id(
    state: &ResponsesStreamState,
    transaction: &mut ResponsesBudgetTransaction,
    output_index: i64,
    item_id: &str,
) -> Result<bool, &'static str> {
    if item_id.is_empty() {
        return Ok(false);
    }
    if state
        .output_index_by_item_id
        .get(item_id)
        .is_some_and(|index| *index != output_index)
    {
        return Err("An output item event reused an item id at another output index.");
    }
    let Some((item_type, existing_id)) = state
        .output_items_by_index
        .get(&output_index)
        .map(|lifecycle| (lifecycle.item_type.clone(), lifecycle.item_id.clone()))
    else {
        return Err("An output item id arrived without a tracked output item.");
    };
    if let Some(existing_id) = existing_id.as_deref().filter(|id| !id.is_empty()) {
        return if existing_id == item_id {
            Ok(false)
        } else {
            Err("An output item event changed its item id.")
        };
    }

    let metadata_bytes = item_type
        .len()
        .checked_add(item_id.len())
        .ok_or("The Responses stream retained-state size overflowed.")?;
    transaction.replace_retained(
        RetainedStateOwner::OutputItemMetadata(output_index),
        metadata_bytes,
    );
    transaction.reserve_retained(
        RetainedStateOwner::OutputItemIdIndex(output_index),
        item_id.len(),
    );
    if item_type == "reasoning"
        && state
            .reasoning_state_by_output_index
            .contains_key(&output_index)
    {
        transaction.replace_retained(
            RetainedStateOwner::ReasoningItemId(output_index),
            item_id.len(),
        );
    }
    Ok(true)
}

fn apply_late_output_item_id(state: &mut ResponsesStreamState, output_index: i64, item_id: &str) {
    let item_type = state
        .output_items_by_index
        .get(&output_index)
        .expect("late item id was preflighted")
        .item_type
        .clone();
    if item_type == "reasoning"
        && state
            .reasoning_state_by_output_index
            .contains_key(&output_index)
    {
        state
            .reasoning_state_by_output_index
            .get_mut(&output_index)
            .expect("reasoning item checked above")
            .item_id = Some(item_id.to_string());
    }
    state
        .output_index_by_item_id
        .insert(item_id.to_string(), output_index);
    state
        .output_items_by_index
        .get_mut(&output_index)
        .expect("output item checked above")
        .item_id = Some(item_id.to_string());
}

fn block_key(output_index: i64, content_index: i64) -> String {
    format!("{output_index}:{content_index}")
}

fn plan_block_key_reservation(
    state: &ResponsesStreamState,
    transaction: &mut ResponsesBudgetTransaction,
    output_index: i64,
    content_index: i64,
) {
    let key = block_key(output_index, content_index);
    if !state.block_index_by_key.contains_key(&key) {
        transaction.reserve_retained(
            RetainedStateOwner::BlockKey(output_index, content_index),
            key.len(),
        );
    }
}

#[cfg(test)]
fn reserve_thinking_output(
    state: &mut ResponsesStreamState,
    output_index: i64,
    output_bytes: usize,
) -> Result<(), &'static str> {
    let mut transaction = ResponsesBudgetTransaction::default();
    plan_thinking_output(state, &mut transaction, output_index, output_bytes)?;
    transaction.commit(state)
}

fn plan_thinking_output(
    state: &ResponsesStreamState,
    transaction: &mut ResponsesBudgetTransaction,
    output_index: i64,
    output_bytes: usize,
) -> Result<(), &'static str> {
    transaction.reserve_output(output_bytes)?;
    plan_block_key_reservation(state, transaction, output_index, 0);
    Ok(())
}

fn indices_are_contiguous<T>(parts: &BTreeMap<i64, T>) -> bool {
    parts
        .keys()
        .enumerate()
        .all(|(expected, actual)| i64::try_from(expected).ok() == Some(*actual))
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

fn validate_annotations_field(value: &Value) -> Result<(), &'static str> {
    if optional_array_field(
        value,
        "annotations",
        "An annotations field was not an array or null.",
    )?
    .is_some_and(|annotations| annotations.iter().any(|annotation| !annotation.is_object()))
    {
        return Err("An annotation entry was not an object.");
    }
    Ok(())
}

fn reconciled_event_item_id(event: &Value, item: &Value) -> Result<Option<String>, &'static str> {
    let outer = optional_string_field(
        event,
        "item_id",
        "An output item event item_id was not a string or null.",
    )?;
    let inner = optional_string_field(item, "id", "An output item id was not a string or null.")?;
    if let (Some(outer), Some(inner)) = (
        outer.filter(|id| !id.is_empty()),
        inner.filter(|id| !id.is_empty()),
    ) {
        if outer != inner {
            return Err("An output item event item_id did not match its item id.");
        }
    }
    Ok(inner.or(outer).map(str::to_string))
}

fn validate_reasoning_blocks(blocks: &[Value], summary: bool) -> Result<(), &'static str> {
    for block in blocks {
        let Some(block) = block.as_object() else {
            return Err("A reasoning block was not an object.");
        };
        let block = Value::Object(block.clone());
        let block_type = required_nonempty_string_field(
            &block,
            "type",
            "A reasoning block had a missing or invalid type.",
        )?;
        if (summary && block_type != "summary_text")
            || (!summary && !matches!(block_type, "reasoning_text" | "text"))
        {
            return Err("A reasoning block had an unsupported type.");
        }
        required_string_field(
            &block,
            "text",
            "A reasoning block had a missing or invalid text field.",
        )?;
    }
    Ok(())
}

fn validate_known_output_item(
    item: &Value,
    _item_type: &str,
    phase: OutputValidationPhase,
) -> Result<(), &'static str> {
    parse_and_validate_anthropic_output_item(item, phase).map(|_| ())
}

/// A non-empty namespace wins over the required function name.
fn resolve_tool_use_name(item: &Value) -> &str {
    optional_string_field(
        item,
        "namespace",
        "A function_call namespace was not a string or null.",
    )
    .expect("function item validated before name resolution")
    .filter(|namespace| !namespace.trim().is_empty())
    .unwrap_or_else(|| {
        required_nonempty_string_field(
            item,
            "name",
            "A function_call item had a missing, empty, or invalid name.",
        )
        .expect("function item validated before name resolution")
    })
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
    state.block_index_by_key.clear();
    state.function_call_state_by_output_index.clear();
    state.function_call_order.clear();
    state.active_function_call_output_index = None;
    state.output_items_by_index.clear();
    state.output_index_by_item_id.clear();
    state.reasoning_state_by_output_index.clear();
    state.last_sequence_number = None;
    state.last_sequence_event = None;
    state.last_sequence_event_bytes = 0;
    state.retained_budget.clear();
    state.tracked_reasoning_parts = 0;
    state.tracked_text_parts = 0;
    state.output_text_by_key.clear();
    state.created_output_digests = None;
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
            debug_assert_eq!(
                state
                    .retained_budget
                    .owners
                    .get(&RetainedStateOwner::BlockKey(output_index, content_index)),
                Some(&key.len())
            );
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
            debug_assert_eq!(
                state
                    .retained_budget
                    .owners
                    .get(&RetainedStateOwner::BlockKey(output_index, summary_index)),
                Some(&key.len())
            );
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
) -> Result<i64, &'static str> {
    state.has_tool_call = true;

    if !state
        .function_call_state_by_output_index
        .contains_key(&output_index)
    {
        let block_index = state.next_content_block_index;
        state.next_content_block_index += 1;

        let resolved_tool_call_id = stable_tool_use_id(tool_call_id, None, output_index);
        let resolved_name = name
            .filter(|name| !name.trim().is_empty())
            .expect("function/tool-search item validated before block creation")
            .to_string();

        state.function_call_state_by_output_index.insert(
            output_index,
            FunctionCallStreamState {
                block_index,
                tool_call_id: resolved_tool_call_id,
                name: resolved_name,
                consecutive_whitespace_count: 0,
                accumulated_arguments: String::new(),
                arguments_done: false,
                started: false,
                done: false,
            },
        );
        state.function_call_order.push(output_index);
    } else {
        // Metadata is charged to the client-visible budget when the function
        // owner is created. Later lifecycle snapshots may repeat it, but they
        // cannot replace already-emitted or pre-reserved tool identity.
        let existing = state
            .function_call_state_by_output_index
            .get(&output_index)
            .expect("function call state exists");
        if tool_call_id
            .filter(|id| !id.is_empty())
            .is_some_and(|id| id != existing.tool_call_id)
        {
            return Err("A completed function/tool call changed its call id.");
        }
        if name
            .filter(|name| !name.is_empty())
            .is_some_and(|name| name != existing.name)
        {
            return Err("A completed function/tool call changed its function name.");
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

    Ok(block_index)
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
    let Some(call) = state.function_call_state_by_output_index.get(&output_index) else {
        return Err("Received function call arguments without an output item.");
    };
    if call.arguments_done {
        return Err("Function call argument data arrived after arguments.done.");
    }
    let old_len = call.accumulated_arguments.len();
    let new_len = old_len
        .checked_add(arguments.len())
        .ok_or("The Responses stream exceeded the translation buffer limit.")?;
    let mut transaction = ResponsesBudgetTransaction::default();
    transaction.reserve_output(arguments.len())?;
    transaction.replace_retained(RetainedStateOwner::FunctionArguments(output_index), new_len);
    transaction.commit(state)?;
    apply_function_call_argument_append(state, output_index, arguments, events);
    Ok(())
}

fn apply_function_call_argument_append(
    state: &mut ResponsesStreamState,
    output_index: i64,
    arguments: String,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    if arguments.is_empty() {
        return;
    }
    let active = function_call_is_active(state, output_index);
    let call = state
        .function_call_state_by_output_index
        .get_mut(&output_index)
        .expect("function argument append was preflighted");
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
    }
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
    let mut transaction = ResponsesBudgetTransaction::default();
    let suffix = plan_function_call_argument_replacement(
        &mut transaction,
        output_index,
        &current,
        authoritative,
    )?;
    transaction.commit(state)?;
    apply_function_call_argument_replacement(state, output_index, authoritative, &suffix, events);
    Ok(())
}

fn plan_function_call_argument_replacement(
    transaction: &mut ResponsesBudgetTransaction,
    output_index: i64,
    current: &str,
    authoritative: &str,
) -> Result<String, &'static str> {
    let Some(suffix) = authoritative.strip_prefix(current) else {
        return Err("Completed function arguments conflicted with streamed argument deltas.");
    };
    transaction.reserve_output(suffix.len())?;
    transaction.replace_retained(
        RetainedStateOwner::FunctionArguments(output_index),
        authoritative.len(),
    );
    Ok(suffix.to_string())
}

fn apply_function_call_argument_replacement(
    state: &mut ResponsesStreamState,
    output_index: i64,
    authoritative: &str,
    suffix: &str,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    let active = function_call_is_active(state, output_index);
    let block_index = state
        .function_call_state_by_output_index
        .get(&output_index)
        .expect("function call validated above")
        .block_index;
    let call = state
        .function_call_state_by_output_index
        .get_mut(&output_index)
        .expect("function call validated above");
    call.accumulated_arguments = authoritative.to_string();
    if active && !suffix.is_empty() {
        events.push(AnthropicStreamEventData::ContentBlockDelta {
            index: block_index,
            delta: AnthropicContentBlockDelta::InputJsonDelta {
                partial_json: suffix.to_string(),
            },
        });
        state.block_has_delta.insert(block_index);
    }
}

/// Finish the active call and activate buffered parallel calls in first-seen
/// order. Calls already marked done are emitted completely and drained; the
/// first unfinished call remains active for future deltas.
fn advance_function_call_queue(
    state: &mut ResponsesStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), &'static str> {
    if let Some(active) = state.active_function_call_output_index.take() {
        if let Some(fc) = state
            .function_call_state_by_output_index
            .get(&active)
            .cloned()
        {
            release_function_call_state(state, active, &fc)?;
            if open_blocks_has(state, fc.block_index) {
                events.push(AnthropicStreamEventData::ContentBlockStop {
                    index: fc.block_index,
                });
                state.open_blocks.retain(|index| *index != fc.block_index);
                state.block_has_delta.remove(&fc.block_index);
            }
            state.function_call_state_by_output_index.remove(&active);
        }
    }

    loop {
        let next = state.function_call_order.iter().copied().find(|index| {
            state
                .function_call_state_by_output_index
                .contains_key(index)
        });
        let Some(next) = next else {
            return Ok(());
        };

        let fc = state
            .function_call_state_by_output_index
            .get(&next)
            .cloned()
            .expect("queued function call exists");
        let arguments = fc.accumulated_arguments.clone();
        let done = fc.done;
        if done {
            release_function_call_state(state, next, &fc)?;
        }
        state.active_function_call_output_index = Some(next);
        let block_index = open_function_call_block(state, next, None, None, events)?;
        if !arguments.is_empty() {
            events.push(AnthropicStreamEventData::ContentBlockDelta {
                index: block_index,
                delta: AnthropicContentBlockDelta::InputJsonDelta {
                    partial_json: arguments,
                },
            });
            state.block_has_delta.insert(block_index);
        }
        if !done {
            return Ok(());
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

fn release_function_call_state(
    state: &mut ResponsesStreamState,
    output_index: i64,
    call: &FunctionCallStreamState,
) -> Result<(), &'static str> {
    let mut transaction = ResponsesBudgetTransaction::default();
    transaction.release_retained(RetainedStateOwner::FunctionMetadata(output_index));
    if !call.accumulated_arguments.is_empty() {
        transaction.release_retained(RetainedStateOwner::FunctionArguments(output_index));
    }
    transaction.commit(state)
}

fn finish_all_function_calls(
    state: &mut ResponsesStreamState,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), &'static str> {
    for fc in state.function_call_state_by_output_index.values_mut() {
        fc.done = true;
    }
    while !state.function_call_state_by_output_index.is_empty() {
        advance_function_call_queue(state, events)?;
    }
    Ok(())
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
    let Some(id) = get_str(response, "id").filter(|id| !id.trim().is_empty()) else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.created event had a missing or empty response id."),
        );
    };
    if let Err(message) = validate_created_status(response) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    let model = match optional_nonnull_string_field(response, "model") {
        Ok(Some(model)) if !model.trim().is_empty() => model.to_string(),
        Ok(None) => match state.fallback_model.clone() {
            Some(model) => model,
            None => {
                return terminate_responses_stream_with_error(
                    state,
                    build_error_event(
                        "A model-less response.created event had no requested model context.",
                    ),
                )
            }
        },
        Ok(Some(_)) => {
            return terminate_responses_stream_with_error(
                state,
                build_error_event("A response.created event contained an empty model."),
            )
        }
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let usage = match validate_raw_responses_usage(response) {
        Ok(usage) => usage,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let created_output_digests = match response.get("output") {
        None | Some(Value::Null) => None,
        Some(Value::Array(output)) => {
            if output.len() > MAX_TRACKED_OUTPUT_ITEMS {
                return terminate_responses_stream_with_error(
                    state,
                    build_error_event("response.created contained too many output items."),
                );
            }
            match output_array_digests(output, OutputValidationPhase::Added) {
                Ok(digests) => Some(digests),
                Err(message) => {
                    return terminate_responses_stream_with_error(state, build_error_event(message))
                }
            }
        }
        Some(_) => {
            return terminate_responses_stream_with_error(
                state,
                build_error_event("response.created contained non-array output."),
            )
        }
    };
    if let Err(message) =
        replace_retained_state_bytes(state, RetainedStateOwner::CreatedResponseId, id.len())
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    state.created_output_digests = created_output_digests;
    message_start(state, id, &model, usage)
}

/// Mirrors `messageStart`.
fn message_start(
    state: &mut ResponsesStreamState,
    id: &str,
    model: &str,
    usage: ValidatedResponsesUsage,
) -> Vec<AnthropicStreamEventData> {
    state.message_start_sent = true;

    let input_tokens = usage.input_tokens - usage.cached_input_tokens.unwrap_or(0);

    vec![AnthropicStreamEventData::MessageStart {
        message: AnthropicMessageStart {
            id: id.to_string(),
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: Vec::new(),
            model: model.to_string(),
            stop_reason: None,
            stop_sequence: None,
            usage: AnthropicUsage {
                input_tokens,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: usage.cached_input_tokens,
                service_tier: None,
                extra: serde_json::Map::new(),
            },
            extra: serde_json::Map::new(),
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
    let Some(item_type) = item
        .get("type")
        .and_then(Value::as_str)
        .filter(|item_type| !item_type.is_empty())
    else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.output_item.added event had a missing or invalid item type.",
            ),
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
    let item_id = match reconciled_event_item_id(event, item).and_then(|item_id| {
        validate_known_output_item(item, item_type, OutputValidationPhase::Added)?;
        Ok(item_id)
    }) {
        Ok(item_id) => item_id,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };

    if let Some(existing) = state.output_items_by_index.get(&output_index) {
        if !existing.done && existing.pending_item.as_ref() == Some(item) {
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
    }
    let item_bytes = match serialized_json_value_bytes(item) {
        Ok(bytes) => bytes,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let initial_digest = match canonical_output_item_digest(item, OutputValidationPhase::Added) {
        Ok(digest) => digest,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let item_id_bytes = item_id.as_ref().map_or(0, String::len);
    let metadata_bytes = match item_type.len().checked_add(item_id_bytes) {
        Some(bytes) => bytes,
        None => {
            return terminate_responses_stream_with_error(
                state,
                build_error_event("The Responses stream retained-state size overflowed."),
            )
        }
    };
    let details = extract_function_call_details(item, output_index, state);
    let mut transaction = ResponsesBudgetTransaction::default();
    transaction.reserve_retained(
        RetainedStateOwner::PendingOutputItem(output_index),
        item_bytes,
    );
    transaction.reserve_retained(
        RetainedStateOwner::OutputItemMetadata(output_index),
        metadata_bytes,
    );
    if item_id.as_deref().is_some_and(|id| !id.is_empty()) {
        transaction.reserve_retained(
            RetainedStateOwner::OutputItemIdIndex(output_index),
            item_id_bytes,
        );
    }
    if item_type == "reasoning" && item_id_bytes > 0 {
        transaction.reserve_retained(
            RetainedStateOwner::ReasoningItemId(output_index),
            item_id_bytes,
        );
    }
    if let Some(details) = details.as_ref() {
        let resolved_id =
            stable_tool_use_id(details.tool_call_id.as_deref(), None, details.output_index);
        let function_metadata_bytes = match resolved_id.len().checked_add(details.name.len()) {
            Some(bytes) => bytes,
            None => {
                return terminate_responses_stream_with_error(
                    state,
                    build_error_event(
                        "The Responses stream exceeded the translated output payload limit.",
                    ),
                )
            }
        };
        if let Err(message) = transaction.reserve_output(function_metadata_bytes) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        transaction.reserve_retained(
            RetainedStateOwner::FunctionMetadata(output_index),
            function_metadata_bytes,
        );
        if let Some(initial) = details
            .initial_arguments
            .as_deref()
            .filter(|initial| !initial.is_empty())
        {
            if let Err(message) = transaction.reserve_output(initial.len()) {
                return terminate_responses_stream_with_error(state, build_error_event(message));
            }
            transaction.replace_retained(
                RetainedStateOwner::FunctionArguments(output_index),
                initial.len(),
            );
        }
    }
    if let Err(message) = transaction.commit(state) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    if let Some(nonempty_id) = item_id.as_deref().filter(|id| !id.is_empty()) {
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
            pending_item: Some(item.clone()),
            pending_item_bytes: item_bytes,
            initial_digest,
            final_digest: None,
            done_event_digest: None,
            completed_incomplete: false,
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

    let details = match details {
        Some(d) => d,
        None => return events,
    };

    if let Err(message) = open_function_call_block(
        state,
        details.output_index,
        details.tool_call_id.as_deref(),
        Some(&details.name),
        &mut events,
    ) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }

    if let Some(initial) = details.initial_arguments {
        apply_function_call_argument_append(state, details.output_index, initial, &mut events);
    }

    events
}

struct FunctionCallDetails {
    output_index: i64,
    tool_call_id: Option<String>,
    name: String,
    initial_arguments: Option<String>,
}

enum OutputItemLifecycleApplyPlan {
    Existing { late_id_update: bool },
    Implicit { initial_digest: OutputItemDigest },
}

struct OutputItemLifecycleCompletion {
    output_index: i64,
    item_type: String,
    item_id: Option<String>,
    final_digest: OutputItemDigest,
    done_event_digest: OutputItemDigest,
    completed_incomplete: bool,
    apply: OutputItemLifecycleApplyPlan,
}

fn apply_output_item_lifecycle_completion(
    state: &mut ResponsesStreamState,
    completion: &OutputItemLifecycleCompletion,
) {
    match completion.apply {
        OutputItemLifecycleApplyPlan::Existing { late_id_update } => {
            if late_id_update {
                apply_late_output_item_id(
                    state,
                    completion.output_index,
                    completion
                        .item_id
                        .as_deref()
                        .expect("late output item id was preflighted"),
                );
            }
            let lifecycle = state
                .output_items_by_index
                .get_mut(&completion.output_index)
                .expect("existing lifecycle completion was preflighted");
            lifecycle.done = true;
            lifecycle.pending_item = None;
            lifecycle.pending_item_bytes = 0;
            lifecycle.final_digest = Some(completion.final_digest);
            lifecycle.done_event_digest = Some(completion.done_event_digest);
            lifecycle.completed_incomplete = completion.completed_incomplete;
        }
        OutputItemLifecycleApplyPlan::Implicit { initial_digest } => {
            if let Some(nonempty_id) = completion.item_id.as_deref().filter(|id| !id.is_empty()) {
                state
                    .output_index_by_item_id
                    .insert(nonempty_id.to_string(), completion.output_index);
            }
            state.output_items_by_index.insert(
                completion.output_index,
                OutputItemLifecycle {
                    item_type: completion.item_type.clone(),
                    item_id: completion.item_id.clone(),
                    done: true,
                    pending_item: None,
                    pending_item_bytes: 0,
                    initial_digest,
                    final_digest: Some(completion.final_digest),
                    done_event_digest: Some(completion.done_event_digest),
                    completed_incomplete: completion.completed_incomplete,
                },
            );
        }
    }
}

fn extract_function_call_details(
    item: &Value,
    output_index: i64,
    state: &ResponsesStreamState,
) -> Option<FunctionCallDetails> {
    let item_type = item.get("type").and_then(Value::as_str)?;

    if item_type == "tool_search_call" {
        let call_id = optional_string_field(
            item,
            "call_id",
            "A tool_search_call call_id was not a string or null.",
        )
        .expect("tool search item validated before extraction");
        if call_id.is_none_or(str::is_empty) {
            // `call_id` is source-optional and may first appear on the
            // authoritative done item. Delay even when the provisional item has
            // an `id`: JSON translation prefers a final call_id, so emitting the
            // item id now would make the two transports disagree.
            return None;
        }
        let tool_call_id = Some(stable_tool_use_id(call_id, None, output_index));
        return Some(FunctionCallDetails {
            output_index,
            tool_call_id,
            name: state.tool_search_name.clone(),
            initial_arguments: Some(String::new()),
        });
    }

    if item_type == "custom_tool_call" {
        let input = required_string_field(
            item,
            "input",
            "A custom_tool_call item had missing or invalid input.",
        )
        .expect("custom tool item validated before extraction");
        return Some(FunctionCallDetails {
            output_index,
            tool_call_id: Some(
                required_nonempty_string_field(
                    item,
                    "call_id",
                    "A custom_tool_call item had a missing or invalid call_id.",
                )
                .expect("custom tool item validated before extraction")
                .to_string(),
            ),
            name: resolve_tool_use_name(item).to_string(),
            initial_arguments: (!input.is_empty()).then(|| {
                serde_json::to_string(&json!({"input":input}))
                    .expect("serializing custom tool input cannot fail")
            }),
        });
    }

    if item_type != "function_call" {
        return None;
    }

    Some(FunctionCallDetails {
        output_index,
        tool_call_id: Some(
            required_nonempty_string_field(
                item,
                "call_id",
                "A function_call item had a missing, empty, or invalid call_id.",
            )
            .expect("function item validated before extraction")
            .to_string(),
        ),
        name: resolve_tool_use_name(item).to_string(),
        initial_arguments: Some(
            required_string_field(
                item,
                "arguments",
                "A function_call item had missing or invalid arguments.",
            )
            .expect("function item validated before extraction")
            .to_string(),
        ),
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
    let Some(item_type) = item
        .get("type")
        .and_then(Value::as_str)
        .filter(|item_type| !item_type.is_empty())
    else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.output_item.done event had a missing or invalid item type.",
            ),
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
    let item_id = match reconciled_event_item_id(event, item).and_then(|item_id| {
        validate_known_output_item(item, item_type, OutputValidationPhase::Done)?;
        Ok(item_id)
    }) {
        Ok(item_id) => item_id,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };

    let final_digest = match canonical_output_item_digest(item, OutputValidationPhase::Done) {
        Ok(digest) => digest,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let done_event_digest = match raw_output_item_digest(item) {
        Ok(digest) => digest,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let completed_incomplete = item.get("status").and_then(Value::as_str) == Some("incomplete");
    let prior_added_call_id = state
        .output_items_by_index
        .get(&output_index)
        .and_then(|lifecycle| lifecycle.pending_item.as_ref())
        .and_then(|item| item.get("call_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let prior_added_item_id = state
        .output_items_by_index
        .get(&output_index)
        .and_then(|lifecycle| lifecycle.pending_item.as_ref())
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let actual_id = item_id.as_deref().filter(|id| !id.is_empty());
    let mut budget_transaction = ResponsesBudgetTransaction::default();
    let lifecycle_apply;

    if state.output_items_by_index.contains_key(&output_index) {
        let pending_item_bytes;
        {
            let existing = state
                .output_items_by_index
                .get(&output_index)
                .expect("output item checked above");
            if existing.item_type != item_type {
                return terminate_responses_stream_with_error(
                    state,
                    build_error_event(
                        "A response.output_item.done event did not match its added item type.",
                    ),
                );
            }
            if let Some(added) = existing.pending_item.as_ref() {
                if let Err(message) = validate_output_item_reconciliation(added, item) {
                    return terminate_responses_stream_with_error(
                        state,
                        build_error_event(message),
                    );
                }
            }
            if existing.done {
                if existing.done_event_digest == Some(done_event_digest) {
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
            pending_item_bytes = existing.pending_item_bytes;
        }
        if item_type == "function_call" {
            let final_arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .expect("function call item validated above");
            if let Some(call) = state
                .function_call_state_by_output_index
                .get(&output_index)
                .filter(|call| call.arguments_done)
            {
                if call.accumulated_arguments != final_arguments {
                    return terminate_responses_stream_with_error(
                        state,
                        build_error_event(
                            "Function call output_item.done arguments conflicted with arguments.done.",
                        ),
                    );
                }
            }
        }
        let needs_late_id_update = state
            .output_items_by_index
            .get(&output_index)
            .and_then(|lifecycle| lifecycle.item_id.as_deref())
            .is_none_or(str::is_empty);
        let late_id_update = if needs_late_id_update {
            if let Some(actual_id) = actual_id {
                match plan_late_output_item_id(
                    state,
                    &mut budget_transaction,
                    output_index,
                    actual_id,
                ) {
                    Ok(update) => update,
                    Err(message) => {
                        return terminate_responses_stream_with_error(
                            state,
                            build_error_event(message),
                        )
                    }
                }
            } else {
                false
            }
        } else {
            false
        };
        if pending_item_bytes > 0 {
            budget_transaction
                .release_retained(RetainedStateOwner::PendingOutputItem(output_index));
        }
        lifecycle_apply = OutputItemLifecycleApplyPlan::Existing { late_id_update };
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
        let initial_digest = match canonical_output_item_digest(item, OutputValidationPhase::Added)
        {
            Ok(digest) => digest,
            Err(message) => {
                return terminate_responses_stream_with_error(state, build_error_event(message))
            }
        };
        let item_id_bytes = item_id.as_ref().map_or(0, String::len);
        let metadata_bytes = match item_type.len().checked_add(item_id_bytes) {
            Some(bytes) => bytes,
            None => {
                return terminate_responses_stream_with_error(
                    state,
                    build_error_event("The Responses stream retained-state size overflowed."),
                )
            }
        };
        budget_transaction.reserve_retained(
            RetainedStateOwner::OutputItemMetadata(output_index),
            metadata_bytes,
        );
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
            budget_transaction.reserve_retained(
                RetainedStateOwner::OutputItemIdIndex(output_index),
                nonempty_id.len(),
            );
        }
        lifecycle_apply = OutputItemLifecycleApplyPlan::Implicit { initial_digest };
    }
    let lifecycle_completion = OutputItemLifecycleCompletion {
        output_index,
        item_type: item_type.to_string(),
        item_id: item_id.clone(),
        final_digest,
        done_event_digest,
        completed_incomplete,
        apply: lifecycle_apply,
    };

    if item_type == "message" {
        let plans =
            match plan_complete_message_item(item, state, output_index, &mut budget_transaction) {
                Ok(plans) => plans,
                Err(message) => {
                    return terminate_responses_stream_with_error(state, build_error_event(message))
                }
            };
        if let Err(message) = budget_transaction.commit(state) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        apply_output_item_lifecycle_completion(state, &lifecycle_completion);
        for plan in plans {
            apply_output_text_append(state, plan, &mut events);
        }
        if let Err(message) = release_output_text_state(state, output_index) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        return events;
    }

    fn release_output_text_state(
        state: &mut ResponsesStreamState,
        output_index: i64,
    ) -> Result<(), &'static str> {
        let prefix = format!("{output_index}:");
        let keys: Vec<String> = state
            .output_text_by_key
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        let mut releases = Vec::with_capacity(keys.len());
        let mut transaction = ResponsesBudgetTransaction::default();
        for key in &keys {
            let content_index = key
                .strip_prefix(&prefix)
                .and_then(|index| index.parse::<i64>().ok())
                .ok_or("A tracked message content index was invalid.")?;
            let part = state
                .output_text_by_key
                .get(key)
                .expect("output text key was collected above");
            transaction.release_retained(RetainedStateOwner::OutputTextKey(
                output_index,
                content_index,
            ));
            if !part.text.is_empty() {
                transaction
                    .release_retained(RetainedStateOwner::OutputText(output_index, content_index));
            }
            let has_block_key = state.block_index_by_key.contains_key(key);
            if has_block_key {
                transaction
                    .release_retained(RetainedStateOwner::BlockKey(output_index, content_index));
            }
            releases.push((key.clone(), has_block_key));
        }
        transaction.commit(state)?;
        for (key, has_block_key) in releases {
            state.output_text_by_key.remove(&key);
            if has_block_key {
                state.block_index_by_key.remove(&key);
            }
        }
        Ok(())
    }

    if matches!(
        item_type,
        "function_call" | "custom_tool_call" | "tool_search_call"
    ) {
        let tool_search_arguments = if item_type == "tool_search_call" {
            match stringify_tool_search_arguments(
                item.get("arguments")
                    .expect("tool search arguments validated above"),
            ) {
                Ok(arguments) => Some(arguments),
                Err(message) => {
                    return terminate_responses_stream_with_error(state, build_error_event(message))
                }
            }
        } else {
            None
        };
        let (call_id, name, final_arguments) = if item_type == "tool_search_call" {
            let done_call_id = optional_string_field(
                item,
                "call_id",
                "A tool_search_call call_id was not a string or null.",
            )
            .expect("tool search item validated above");
            let resolved_call_id =
                reconcile_tool_search_call_id(prior_added_call_id.as_deref(), done_call_id)
                    .expect("tool search call id reconciled during done validation");
            (
                Some(stable_tool_use_id(
                    resolved_call_id,
                    optional_string_field(item, "id", "A tool_search_call id was invalid.")
                        .expect("tool search item validated above")
                        .or(prior_added_item_id.as_deref()),
                    output_index,
                )),
                state.tool_search_name.clone(),
                tool_search_arguments,
            )
        } else if item_type == "custom_tool_call" {
            (
                Some(
                    required_nonempty_string_field(
                        item,
                        "call_id",
                        "A custom_tool_call item had a missing or invalid call_id.",
                    )
                    .expect("custom tool item validated above")
                    .to_string(),
                ),
                resolve_tool_use_name(item).to_string(),
                Some(
                    serde_json::to_string(&json!({
                        "input":required_string_field(
                            item,
                            "input",
                            "A custom_tool_call item had missing or invalid input.",
                        )
                        .expect("custom tool item validated above")
                    }))
                    .expect("serializing custom tool input cannot fail"),
                ),
            )
        } else {
            (
                Some(
                    required_nonempty_string_field(
                        item,
                        "call_id",
                        "A function_call item had a missing, empty, or invalid call_id.",
                    )
                    .expect("function item validated above")
                    .to_string(),
                ),
                resolve_tool_use_name(item).to_string(),
                Some(
                    required_string_field(
                        item,
                        "arguments",
                        "A function_call item had missing or invalid arguments.",
                    )
                    .expect("function item validated above")
                    .to_string(),
                ),
            )
        };
        let current_arguments = state
            .function_call_state_by_output_index
            .get(&output_index)
            .map(|call| call.accumulated_arguments.clone())
            .unwrap_or_default();
        if let Err(message) = plan_function_call_metadata_if_new(
            state,
            &mut budget_transaction,
            output_index,
            call_id.as_deref(),
            &name,
        ) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        let argument_suffix = if let Some(args) = final_arguments.as_deref() {
            match plan_function_call_argument_replacement(
                &mut budget_transaction,
                output_index,
                &current_arguments,
                args,
            ) {
                Ok(suffix) => Some(suffix),
                Err(message) => {
                    return terminate_responses_stream_with_error(state, build_error_event(message))
                }
            }
        } else {
            None
        };
        if let Err(message) = budget_transaction.commit(state) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        apply_output_item_lifecycle_completion(state, &lifecycle_completion);
        if let Err(message) = open_function_call_block(
            state,
            output_index,
            call_id.as_deref(),
            Some(&name),
            &mut events,
        ) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }

        if let Some(args) = final_arguments {
            apply_function_call_argument_replacement(
                state,
                output_index,
                &args,
                argument_suffix
                    .as_deref()
                    .expect("function arguments were preflighted"),
                &mut events,
            );
        }

        let active = function_call_is_active(state, output_index);
        if let Some(fc) = state
            .function_call_state_by_output_index
            .get_mut(&output_index)
        {
            fc.done = true;
        }
        if active {
            if let Err(message) = advance_function_call_queue(state, &mut events) {
                return terminate_responses_stream_with_error(state, build_error_event(message));
            }
        }
        return events;
    }

    if matches!(item_type, "compaction" | "compaction_summary") {
        let id = optional_string_field(item, "id", "A compaction item id was invalid.")
            .expect("compaction item validated above")
            .unwrap_or("");
        let encrypted_content = required_nonempty_string_field(
            item,
            "encrypted_content",
            "A compaction item had missing, empty, or invalid encrypted_content.",
        )
        .expect("compaction item validated above");
        let signature = encode_compaction_carrier_signature(encrypted_content, id);
        let additional = match THINKING_TEXT.len().checked_add(signature.len()) {
            Some(additional) => additional,
            None => {
                return terminate_responses_stream_with_error(
                    state,
                    build_error_event(
                        "The Responses stream exceeded the translated output payload limit.",
                    ),
                )
            }
        };
        if let Err(message) =
            plan_thinking_output(state, &mut budget_transaction, output_index, additional)
        {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        if let Err(message) = budget_transaction.commit(state) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        apply_output_item_lifecycle_completion(state, &lifecycle_completion);

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
            delta: AnthropicContentBlockDelta::SignatureDelta { signature },
        });
        state.block_has_delta.insert(block_index);
        return events;
    }

    if item_type != "reasoning" {
        if let Err(message) = budget_transaction.commit(state) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        apply_output_item_lifecycle_completion(state, &lifecycle_completion);
        return events;
    }

    let encrypted_content = optional_string_field(
        item,
        "encrypted_content",
        "A reasoning encrypted_content field was not a string or null.",
    )
    .expect("reasoning item validated above");
    let lifecycle_id = state
        .output_items_by_index
        .get(&output_index)
        .and_then(|lifecycle| lifecycle.item_id.clone());
    let id = get_str(item, "id").or(lifecycle_id.as_deref());
    let mut buffered = state
        .reasoning_state_by_output_index
        .get(&output_index)
        .cloned()
        .unwrap_or_default();
    if buffered.item_id.as_deref().is_none_or(str::is_empty)
        && matches!(
            &lifecycle_completion.apply,
            OutputItemLifecycleApplyPlan::Existing {
                late_id_update: true
            }
        )
    {
        buffered.item_id.clone_from(&lifecycle_completion.item_id);
    }
    plan_reasoning_state_release(&mut budget_transaction, output_index, &buffered);
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
    let buffered_summary_parts: Vec<ReasoningSummaryPartState> =
        buffered.summary_parts.into_values().collect();
    let buffered_content_segments: Vec<String> = buffered.content_parts.into_values().collect();
    let final_summary_segments: Vec<String> = item
        .get("summary")
        .and_then(Value::as_array)
        .expect("reasoning summary validated above")
        .iter()
        .map(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .expect("reasoning summary block validated above")
                .to_string()
        })
        .collect();
    let summary_segments =
        if final_summary_segments.is_empty() && !buffered_summary_parts.is_empty() {
            buffered_summary_parts
                .iter()
                .map(|part| part.text.clone())
                .collect()
        } else {
            if final_summary_segments.len() < buffered_summary_parts.len()
                || buffered_summary_parts
                    .iter()
                    .enumerate()
                    .any(|(index, buffered)| {
                        let final_text = &final_summary_segments[index];
                        if buffered.done {
                            final_text != &buffered.text
                        } else {
                            !final_text.starts_with(&buffered.text)
                        }
                    })
            {
                return terminate_responses_stream_with_error(
                    state,
                    build_error_event(
                        "A completed reasoning summary conflicted with streamed summary text.",
                    ),
                );
            }
            final_summary_segments
        };
    let final_content_segments: Vec<String> = item
        .get("content")
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .expect("reasoning content block validated above")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    let content_segments =
        if final_content_segments.is_empty() && !buffered_content_segments.is_empty() {
            buffered_content_segments
        } else {
            if final_content_segments.len() < buffered_content_segments.len()
                || buffered_content_segments
                    .iter()
                    .enumerate()
                    .any(|(index, buffered)| !final_content_segments[index].starts_with(buffered))
            {
                return terminate_responses_stream_with_error(
                    state,
                    build_error_event(
                        "Completed reasoning content conflicted with streamed content deltas.",
                    ),
                );
            }
            final_content_segments
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
        let reasoning_block_key = block_key(output_index, 0);
        let remove_block_key = state.block_index_by_key.contains_key(&reasoning_block_key);
        if remove_block_key {
            budget_transaction.release_retained(RetainedStateOwner::BlockKey(output_index, 0));
        }
        if let Err(message) = budget_transaction.commit(state) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        apply_output_item_lifecycle_completion(state, &lifecycle_completion);
        state.reasoning_state_by_output_index.remove(&output_index);
        if remove_block_key {
            state.block_index_by_key.remove(&reasoning_block_key);
        }
        return events;
    };

    let signature = encode_reasoning_signature(encrypted_content, id);
    let additional = match display_text.len().checked_add(signature.len()) {
        Some(additional) => additional,
        None => {
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "The Responses stream exceeded the translated output payload limit.",
                ),
            )
        }
    };
    if let Err(message) =
        plan_thinking_output(state, &mut budget_transaction, output_index, additional)
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    if let Err(message) = budget_transaction.commit(state) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    apply_output_item_lifecycle_completion(state, &lifecycle_completion);
    state.reasoning_state_by_output_index.remove(&output_index);

    let block_index = open_thinking_block_if_needed(state, output_index, &mut events);
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::ThinkingDelta {
            thinking: display_text,
        },
    });
    state.block_has_delta.insert(block_index);

    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::SignatureDelta { signature },
    });
    state.block_has_delta.insert(block_index);

    let reasoning_block_key = block_key(output_index, 0);
    if state.block_index_by_key.contains_key(&reasoning_block_key) {
        let mut release = ResponsesBudgetTransaction::default();
        release.release_retained(RetainedStateOwner::BlockKey(output_index, 0));
        if let Err(message) = release.commit(state) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
        state.block_index_by_key.remove(&reasoning_block_key);
    }
    events
}

fn plan_reasoning_state_release(
    transaction: &mut ResponsesBudgetTransaction,
    output_index: i64,
    reasoning: &ReasoningItemStreamState,
) {
    if reasoning
        .item_id
        .as_deref()
        .is_some_and(|item_id| !item_id.is_empty())
    {
        transaction.release_retained(RetainedStateOwner::ReasoningItemId(output_index));
    }
    for (summary_index, part) in &reasoning.summary_parts {
        if !part.text.is_empty() {
            transaction.release_retained(RetainedStateOwner::ReasoningSummary(
                output_index,
                *summary_index,
            ));
        }
    }
    for (content_index, text) in &reasoning.content_parts {
        if !text.is_empty() {
            transaction.release_retained(RetainedStateOwner::ReasoningContent(
                output_index,
                *content_index,
            ));
        }
    }
}

struct OutputTextAppendPlan {
    output_index: i64,
    content_index: i64,
    key: String,
    text: String,
    new_part: bool,
}

fn plan_output_text_append(
    state: &ResponsesStreamState,
    transaction: &mut ResponsesBudgetTransaction,
    output_index: i64,
    content_index: i64,
    text: &str,
    planned_new_parts: usize,
) -> Result<OutputTextAppendPlan, &'static str> {
    let key = block_key(output_index, content_index);
    if state
        .output_text_by_key
        .get(&key)
        .is_some_and(|part| part.done)
    {
        return Err("Output text data arrived after output_text.done.");
    }
    let new_part = !state.output_text_by_key.contains_key(&key);
    if new_part {
        let prospective_parts = state
            .tracked_text_parts
            .checked_add(planned_new_parts)
            .and_then(|parts| parts.checked_add(1))
            .ok_or("The Responses stream emitted too many text content parts.")?;
        if prospective_parts > MAX_TRACKED_CONTENT_PARTS {
            return Err("The Responses stream emitted too many text content parts.");
        }
    }
    let old_len = state
        .output_text_by_key
        .get(&key)
        .map_or(0, |part| part.text.len());
    let new_len = old_len
        .checked_add(text.len())
        .ok_or("The Responses stream retained-state size overflowed.")?;

    transaction.reserve_output(text.len())?;
    if new_part {
        transaction.reserve_retained(
            RetainedStateOwner::OutputTextKey(output_index, content_index),
            key.len(),
        );
    }
    transaction.replace_retained(
        RetainedStateOwner::OutputText(output_index, content_index),
        new_len,
    );
    if !text.is_empty() {
        plan_block_key_reservation(state, transaction, output_index, content_index);
    }
    Ok(OutputTextAppendPlan {
        output_index,
        content_index,
        key,
        text: text.to_string(),
        new_part,
    })
}

fn apply_output_text_append(
    state: &mut ResponsesStreamState,
    plan: OutputTextAppendPlan,
    events: &mut Vec<AnthropicStreamEventData>,
) {
    if plan.new_part {
        state.tracked_text_parts += 1;
        state
            .output_text_by_key
            .insert(plan.key.clone(), OutputTextStreamState::default());
    }
    if plan.text.is_empty() {
        return;
    }
    state
        .output_text_by_key
        .get_mut(&plan.key)
        .expect("text part inserted above")
        .text
        .push_str(&plan.text);
    let block_index =
        open_text_block_if_needed(state, plan.output_index, plan.content_index, events);
    events.push(AnthropicStreamEventData::ContentBlockDelta {
        index: block_index,
        delta: AnthropicContentBlockDelta::TextDelta { text: plan.text },
    });
    state.block_has_delta.insert(block_index);
}

fn append_output_text(
    state: &mut ResponsesStreamState,
    output_index: i64,
    content_index: i64,
    text: &str,
    events: &mut Vec<AnthropicStreamEventData>,
) -> Result<(), &'static str> {
    let mut transaction = ResponsesBudgetTransaction::default();
    let plan = plan_output_text_append(
        state,
        &mut transaction,
        output_index,
        content_index,
        text,
        0,
    )?;
    transaction.commit(state)?;
    apply_output_text_append(state, plan, events);
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
    if let Some(done) = state.output_text_by_key.get(&key).filter(|part| part.done) {
        return if done.text == authoritative {
            Ok(())
        } else {
            Err("Completed output text conflicted with output_text.done.")
        };
    }
    let current = state
        .output_text_by_key
        .get(&key)
        .map(|part| part.text.clone())
        .unwrap_or_default();
    let Some(suffix) = authoritative.strip_prefix(&current) else {
        return Err("Completed output text conflicted with streamed text deltas.");
    };
    append_output_text(state, output_index, content_index, suffix, events)
}

fn message_block_text(block: &Value) -> Option<&str> {
    match block.get("type").and_then(Value::as_str) {
        Some("output_text" | "input_text") => block.get("text").and_then(Value::as_str),
        _ => None,
    }
}

fn plan_complete_message_item(
    item: &Value,
    state: &ResponsesStreamState,
    output_index: i64,
    transaction: &mut ResponsesBudgetTransaction,
) -> Result<Vec<OutputTextAppendPlan>, &'static str> {
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or("A completed message item had missing or invalid content.")?;
    let prefix = format!("{output_index}:");
    for key in state
        .output_text_by_key
        .keys()
        .filter(|key| key.starts_with(&prefix))
    {
        let content_index = key
            .strip_prefix(&prefix)
            .and_then(|index| index.parse::<i64>().ok())
            .and_then(|index| usize::try_from(index).ok())
            .ok_or("A tracked message content index was invalid.")?;
        if content
            .get(content_index)
            .and_then(message_block_text)
            .is_none()
        {
            return Err(
                "Streamed output text referenced a content index absent from the completed message.",
            );
        }
    }

    let mut plans = Vec::new();
    let mut planned_new_parts = 0usize;
    for (content_index, block) in content.iter().enumerate() {
        let Some(authoritative) = message_block_text(block) else {
            continue;
        };
        let content_index = i64::try_from(content_index)
            .map_err(|_| "A message content index exceeded the supported integer range.")?;
        let key = block_key(output_index, content_index);
        if let Some(done) = state.output_text_by_key.get(&key).filter(|part| part.done) {
            if done.text == authoritative {
                continue;
            }
            return Err("Completed output text conflicted with output_text.done.");
        }
        let current = state
            .output_text_by_key
            .get(&key)
            .map_or("", |part| part.text.as_str());
        let suffix = authoritative
            .strip_prefix(current)
            .ok_or("Completed output text conflicted with streamed text deltas.")?;
        let plan = plan_output_text_append(
            state,
            transaction,
            output_index,
            content_index,
            suffix,
            planned_new_parts,
        )?;
        if plan.new_part {
            planned_new_parts += 1;
        }
        plans.push(plan);
    }
    Ok(plans)
}

fn handle_function_call_arguments_delta(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let mut events = Vec::new();
    let output_index = match validate_active_output_item(event, state, &["function_call"]) {
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
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.function_call_arguments.delta event contained an empty delta.",
            ),
        );
    }

    if let Err(message) = open_function_call_block(state, output_index, None, None, &mut events) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }

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
    let output_index = match validate_active_output_item(event, state, &["function_call"]) {
        Ok(index) => index,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };
    let Some(final_arguments) = get_str(event, "arguments") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.function_call_arguments.done event was missing its arguments.",
            ),
        );
    };
    if let Err(message) = validate_function_arguments(final_arguments, OutputValidationPhase::Done)
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    if state
        .function_call_state_by_output_index
        .get(&output_index)
        .is_some_and(|call| call.arguments_done)
    {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A function call emitted duplicate arguments.done events."),
        );
    }
    if let Err(message) = open_function_call_block(state, output_index, None, None, &mut events) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    if let Err(message) =
        reconcile_function_call_arguments(state, output_index, final_arguments, &mut events)
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    if let Some(call) = state
        .function_call_state_by_output_index
        .get_mut(&output_index)
    {
        call.arguments_done = true;
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
    if let Err(message) = validate_annotations_field(event) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
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
    let item_id_missing = lifecycle.item_id.as_deref().is_none_or(str::is_empty);
    let actual_id = optional_string_field(
        event,
        "item_id",
        "An incremental output event item_id was not a string or null.",
    )?
    .filter(|id| !id.is_empty());
    if item_id_missing {
        if let Some(actual) = actual_id {
            retain_late_output_item_id(state, output_index, actual)?;
        }
    }
    if let Some(actual_call_id) = optional_string_field(
        event,
        "call_id",
        "An incremental output event call_id was not a string or null.",
    )?
    .filter(|call_id| !call_id.is_empty())
    {
        if let Some(expected_call_id) = state
            .function_call_state_by_output_index
            .get(&output_index)
            .map(|call| call.tool_call_id.as_str())
        {
            if expected_call_id != actual_call_id {
                return Err("An incremental output event call_id did not match its output item.");
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
    let item_id_missing = lifecycle.item_id.as_deref().is_none_or(str::is_empty);
    let actual_id = optional_string_field(
        event,
        "item_id",
        "A reasoning stream event item_id was not a string or null.",
    )?
    .filter(|id| !id.is_empty());
    if item_id_missing {
        if let Some(actual) = actual_id {
            retain_late_output_item_id(state, output_index, actual)?;
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
    if let Some(part) = event.get("part") {
        if let Err(message) = validate_reasoning_blocks(std::slice::from_ref(part), true) {
            return terminate_responses_stream_with_error(state, build_error_event(message));
        }
    }
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
    if !part.text.is_empty() || part.done {
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
        .is_some_and(|part| part.done);
    if already_done {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.reasoning_summary_text.delta event arrived after its done event.",
            ),
        );
    }
    let old_len = state
        .reasoning_state_by_output_index
        .get(&output_index)
        .and_then(|reasoning| reasoning.summary_parts.get(&summary_index))
        .map_or(0, |part| part.text.len());
    let new_len = match old_len.checked_add(delta_text.len()) {
        Some(new_len) => new_len,
        None => {
            return terminate_responses_stream_with_error(
                state,
                build_error_event("The Responses stream retained-state size overflowed."),
            )
        }
    };
    if let Err(message) = replace_retained_state_bytes(
        state,
        RetainedStateOwner::ReasoningSummary(output_index, summary_index),
        new_len,
    ) {
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
    if required_nonempty_string_field(
        event,
        "item_id",
        "A response.reasoning_summary_text.done event had a missing or invalid item_id.",
    )
    .is_err()
    {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A response.reasoning_summary_text.done event had a missing or invalid item_id.",
            ),
        );
    }
    let Some(text) = get_str(event, "text") else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.reasoning_summary_text.done event was missing its text."),
        );
    };
    if let Err(message) = reserve_reasoning_part(state, output_index, summary_index, true) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    state
        .reasoning_state_by_output_index
        .get_mut(&output_index)
        .expect("validated reasoning item")
        .summary_parts
        .entry(summary_index)
        .or_default();
    let previous = state
        .reasoning_state_by_output_index
        .get(&output_index)
        .and_then(|reasoning| reasoning.summary_parts.get(&summary_index))
        .filter(|part| part.done);
    if let Some(previous) = previous {
        if previous.text == text {
            return Vec::new();
        }
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A reasoning summary part emitted conflicting text.done events."),
        );
    }
    let current = state
        .reasoning_state_by_output_index
        .get(&output_index)
        .and_then(|reasoning| reasoning.summary_parts.get(&summary_index))
        .map(|part| part.text.as_str())
        .expect("reasoning part reserved above");
    if !text.starts_with(current) {
        return terminate_responses_stream_with_error(
            state,
            build_error_event(
                "A reasoning summary text.done value conflicted with streamed deltas.",
            ),
        );
    }
    if let Err(message) = replace_retained_state_bytes(
        state,
        RetainedStateOwner::ReasoningSummary(output_index, summary_index),
        text.len(),
    ) {
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
    part.done = true;
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
    let old_len = state
        .reasoning_state_by_output_index
        .get(&output_index)
        .and_then(|reasoning| reasoning.content_parts.get(&content_index))
        .map_or(0, String::len);
    let new_len = match old_len.checked_add(delta.len()) {
        Some(new_len) => new_len,
        None => {
            return terminate_responses_stream_with_error(
                state,
                build_error_event("The Responses stream retained-state size overflowed."),
            )
        }
    };
    if let Err(message) = replace_retained_state_bytes(
        state,
        RetainedStateOwner::ReasoningContent(output_index, content_index),
        new_len,
    ) {
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
    if let Err(message) = validate_annotations_field(event) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    let key = block_key(output_index, content_index);
    if state
        .output_text_by_key
        .get(&key)
        .is_some_and(|part| part.done)
    {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A text content part emitted duplicate output_text.done events."),
        );
    }
    if let Err(message) =
        reconcile_output_text(state, output_index, content_index, text, &mut events)
    {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    state
        .output_text_by_key
        .get_mut(&key)
        .expect("text part reconciled above")
        .done = true;

    events
}

fn handle_response_completed(
    event: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<AnthropicStreamEventData> {
    let Some(terminal_kind) = event
        .get("type")
        .and_then(Value::as_str)
        .and_then(ResponsesTerminalKind::from_event_type)
        .filter(|kind| *kind != ResponsesTerminalKind::Failed)
    else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("An unrecognized Responses event reached terminal handling."),
        );
    };

    let Some(response) = event
        .get("response")
        .filter(|response| response.is_object())
    else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A terminal Responses event contained an invalid response object."),
        );
    };

    let Some(_terminal_id) = get_str(response, "id").filter(|id| !id.trim().is_empty()) else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A terminal Responses event had a missing or empty response id."),
        );
    };
    // Copilot may re-encrypt the opaque response id between created and
    // terminal events. Presence is required, but byte equality is not a stable
    // continuity signal; sequencing and the tracked output lifecycle are.

    if let Err(message) = validate_terminal_status(response, terminal_kind) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    if let Some(end_turn) = response.get("end_turn") {
        if !end_turn.is_null() && !end_turn.is_boolean() {
            return terminate_responses_stream_with_error(
                state,
                build_error_event(
                    "A terminal Responses event contained an invalid end_turn value.",
                ),
            );
        }
    }
    let usage_is_known = response.get("usage").is_some_and(|usage| !usage.is_null());
    let validated_usage = match validate_raw_responses_usage(response) {
        Ok(usage) => usage,
        Err(message) => {
            return terminate_responses_stream_with_error(state, build_error_event(message))
        }
    };

    let mut events = Vec::new();

    let pending_output_item = state.output_items_by_index.values().any(|item| !item.done);
    let pending_function_call = state
        .function_call_state_by_output_index
        .values()
        .any(|call| !call.done);
    let output_indices_contiguous = (0..state.output_items_by_index.len()).all(|index| {
        i64::try_from(index)
            .ok()
            .is_some_and(|index| state.output_items_by_index.contains_key(&index))
    });
    let lifecycle_initial: Vec<OutputItemDigest> = if output_indices_contiguous {
        (0..state.output_items_by_index.len())
            .filter_map(|index| {
                let index = i64::try_from(index).ok()?;
                Some(state.output_items_by_index.get(&index)?.initial_digest)
            })
            .collect()
    } else {
        Vec::new()
    };
    let lifecycle_final: Vec<OutputItemDigest> = if output_indices_contiguous {
        (0..state.output_items_by_index.len())
            .filter_map(|index| {
                let index = i64::try_from(index).ok()?;
                state.output_items_by_index.get(&index)?.final_digest
            })
            .collect()
    } else {
        Vec::new()
    };
    let created_output_mismatch = if let Some(created) = state
        .created_output_digests
        .as_ref()
        .filter(|output| !output.is_empty())
    {
        if lifecycle_final.is_empty() {
            true
        } else {
            created != &lifecycle_initial && created != &lifecycle_final
        }
    } else {
        false
    };
    let terminal_output_mismatch = match response.get("output") {
        None | Some(Value::Null) => false,
        Some(Value::Array(output)) if output.is_empty() => false,
        Some(Value::Array(output)) => {
            match output_array_digests(output, OutputValidationPhase::Done) {
                Ok(digests) => digests != lifecycle_final,
                Err(message) => {
                    return terminate_responses_stream_with_error(state, build_error_event(message))
                }
            }
        }
        Some(_) => true,
    };
    let terminal_item_status_mismatch = terminal_kind == ResponsesTerminalKind::Completed
        && state
            .output_items_by_index
            .values()
            .any(|lifecycle| lifecycle.completed_incomplete);
    if pending_output_item
        || pending_function_call
        || !state.reasoning_state_by_output_index.is_empty()
        || !output_indices_contiguous
        || created_output_mismatch
        || terminal_output_mismatch
        || terminal_item_status_mismatch
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
    let usage = map_responses_usage_delta(validated_usage, usage_is_known);

    if let Err(message) = finish_all_function_calls(state, &mut events) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
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
    let Some(response) = event
        .get("response")
        .filter(|response| response.is_object())
    else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.failed event contained an invalid response object."),
        );
    };
    let Some(_failed_id) = get_str(response, "id").filter(|id| !id.trim().is_empty()) else {
        return terminate_responses_stream_with_error(
            state,
            build_error_event("A response.failed event had a missing or empty response id."),
        );
    };
    if let Err(message) = validate_terminal_status(response, ResponsesTerminalKind::Failed) {
        return terminate_responses_stream_with_error(state, build_error_event(message));
    }
    let mut events = Vec::new();
    close_all_open_blocks(state, &mut events);

    let message = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("The response failed due to an unknown error.");

    events.push(build_error_event(message));
    state.message_completed = true;
    state.translation_failed = true;

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
    state.translation_failed = true;
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
        if response.get("end_turn").and_then(Value::as_bool) == Some(false) && !has_tool_call {
            return Ok(Some("pause_turn".to_string()));
        }
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
                let has_call = has_tool_call
                    || items.iter().any(|item| {
                        matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("function_call" | "custom_tool_call" | "tool_search_call")
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
fn map_responses_usage_delta(
    usage: ValidatedResponsesUsage,
    include_input_tokens: bool,
) -> AnthropicMessageDeltaUsage {
    AnthropicMessageDeltaUsage {
        input_tokens: include_input_tokens
            .then_some(usage.input_tokens - usage.cached_input_tokens.unwrap_or(0)),
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: usage.cached_input_tokens,
        service_tier: None,
        extra: serde_json::Map::new(),
    }
}

/// Mirrors `stringifyToolSearchArguments`.
fn stringify_tool_search_arguments(arguments_value: &Value) -> Result<String, &'static str> {
    serde_json::to_string(&format_tool_search_bridge_arguments(arguments_value))
        .map_err(|_| "Tool search arguments could not be serialized.")
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

    fn budget_snapshot(
        state: &ResponsesStreamState,
    ) -> (usize, usize, HashMap<RetainedStateOwner, usize>) {
        (
            state.output_budget.used_bytes,
            state.retained_budget.used_bytes,
            state.retained_budget.owners.clone(),
        )
    }

    fn fill_remaining_retained_budget(state: &mut ResponsesStreamState, owner: RetainedStateOwner) {
        let remaining = MAX_BUFFERED_TRANSLATION_BYTES - state.retained_budget.used_bytes;
        state
            .retained_budget
            .reserve(owner, remaining)
            .expect("fixture fills retained budget exactly");
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
                "id": "resp_created",
                "model": "gpt-5",
                "usage": {
                    "input_tokens": 10,
                    "input_tokens_details": { "cached_tokens": 2 },
                    "output_tokens": 0,
                    "total_tokens": 10
                }
            }
        });
        let evs = translate_responses_stream_event(&created, &mut state);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AnthropicStreamEventData::MessageStart { message } => {
                assert_eq!(message.id, "resp_created");
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
                    "id":"message-added",
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
            "item_id": "message-delta-1",
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
            "item_id": "message-delta-2",
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
            "item_id": "message-text-done",
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
                    "id":"message-done",
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
                "id": "resp_completed",
                "status": "completed",
                "output": [{
                    "type":"message",
                    "id":"message-terminal",
                    "role":"assistant",
                    "content":[{"type":"output_text","text":"Hello world"}]
                }],
                "usage": {
                    "input_tokens": 10,
                    "input_tokens_details": { "cached_tokens": 2 },
                    "output_tokens": 5,
                    "total_tokens": 15
                }
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
        assert!(state.translation_failed);
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
            "response": { "id":"resp_failed", "error": { "message": "boom" } }
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
        assert!(state.translation_failed);
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
                    "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
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
                "id":"resp_test",
                "status": "incomplete",
                "incomplete_details": {"reason": "content_filter"},
                "usage": {"input_tokens": 1, "output_tokens": 0, "total_tokens": 1}
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
            }) => assert_eq!(
                signature,
                &encode_reasoning_signature(Some("enc"), Some("r1"))
            ),
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
            let expected_signature =
                encode_reasoning_signature(Some("encrypted"), Some("reasoning-id"));
            assert!(events.iter().any(|event| matches!(
                event,
                AnthropicStreamEventData::ContentBlockDelta {
                    delta: AnthropicContentBlockDelta::SignatureDelta { signature },
                    ..
                } if signature == &expected_signature
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
                "item":{"type":"reasoning","id":"reasoning-added","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        for (item_id, delta) in [
            ("reasoning-delta-1", "  "),
            ("reasoning-delta-2", "analysis"),
            ("reasoning-delta-3", "  "),
        ] {
            let events = translate_responses_stream_event(
                &json!({
                    "type":"response.reasoning_summary_text.delta",
                    "output_index":0,
                    "summary_index":0,
                    "item_id":item_id,
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
                    "id":"reasoning-done",
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
            json!({"type":"response.reasoning_summary_text.done","item_id":"reasoning-id","output_index":0,"summary_index":0,"text":"one"}),
            json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1}),
            json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"two"}),
            json!({"type":"response.reasoning_summary_text.done","item_id":"reasoning-id","output_index":0,"summary_index":1,"text":"two"}),
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
                    "encrypted_content":"encrypted",
                    "summary":[]
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
        let expected_signature =
            encode_reasoning_signature(Some("encrypted-content"), Some("reasoning-content"));
        assert!(events.iter().any(|event| matches!(
            event,
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::SignatureDelta { signature },
                ..
            } if signature == &expected_signature
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
            "response":{
                "id":"resp_test",
                "status":"completed",
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
            }
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
    fn identical_replays_dedupe_while_opaque_reasoning_fields_rotate() {
        let mut state = started_state();
        let added = json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{
                "type":"reasoning",
                "id":"dedupe-added",
                "encrypted_content":"opaque-added",
                "summary":[]
            }
        });
        assert!(translate_responses_stream_event(&added, &mut state).is_empty());
        assert!(translate_responses_stream_event(&added, &mut state).is_empty());

        let done = json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":{
                "type":"reasoning",
                "id":"dedupe-done",
                "encrypted_content":"opaque-done",
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
                    "id":"resp-terminal",
                    "status":"completed",
                    "output":[{
                        "type":"reasoning",
                        "id":"dedupe-terminal",
                        "encrypted_content":"opaque-terminal",
                        "summary":[{"type":"summary_text","text":"once"}]
                    }],
                    "usage":null
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
        let remaining = MAX_BUFFERED_TRANSLATION_BYTES - state.retained_budget.used_bytes();
        state
            .retained_budget
            .reserve(RetainedStateOwner::SequenceSnapshot, remaining)
            .expect("fixture fills the retained budget");
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

    #[test]
    fn aggregate_output_budget_counts_utf8_and_fails_once() {
        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{
                    "type":"message",
                    "id":"message-budget",
                    "role":"assistant",
                    "content":[]
                }
            }),
            &mut state,
        )
        .is_empty());
        state.output_budget.used_bytes = MAX_UPSTREAM_RESPONSE_BYTES - "é".len();
        let exact = translate_responses_stream_event(
            &json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "content_index":0,
                "delta":"é"
            }),
            &mut state,
        );
        assert!(exact.iter().any(|event| matches!(
            event,
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::TextDelta { text },
                ..
            } if text == "é"
        )));
        assert_eq!(state.output_budget.used_bytes, MAX_UPSTREAM_RESPONSE_BYTES);

        let overflow = translate_responses_stream_event(
            &json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "content_index":0,
                "delta":"x"
            }),
            &mut state,
        );
        assert_eq!(
            overflow
                .iter()
                .filter(|event| matches!(event, AnthropicStreamEventData::Error { .. }))
                .count(),
            1
        );
        assert!(!overflow
            .iter()
            .any(|event| matches!(event, AnthropicStreamEventData::MessageStop)));
        assert!(state.message_completed);
        assert!(state.block_index_by_key.is_empty());
        assert_eq!(state.retained_budget.used_bytes(), 0);
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "content_index":0,
                "delta":"later"
            }),
            &mut state,
        )
        .is_empty());
    }

    #[test]
    fn sequence_snapshot_replacement_is_byte_accounted() {
        let mut state = create_responses_stream_state(None);
        state
            .retained_budget
            .reserve(RetainedStateOwner::OutputItemMetadata(99), 7)
            .expect("independent retained owner");
        let first = json!({"type":"ping","sequence_number":1,"payload":"first"});
        assert_eq!(validate_event_sequence(&first, &mut state), Ok(false));
        let first_bytes = serde_json::to_vec(&first).unwrap().len();
        assert_eq!(state.last_sequence_event_bytes, first_bytes);
        assert_eq!(state.retained_budget.used_bytes(), 7 + first_bytes);
        assert_eq!(validate_event_sequence(&first, &mut state), Ok(true));
        assert_eq!(state.retained_budget.used_bytes(), 7 + first_bytes);

        let second = json!({"type":"ping","sequence_number":2,"payload":"x"});
        assert_eq!(validate_event_sequence(&second, &mut state), Ok(false));
        let second_bytes = serde_json::to_vec(&second).unwrap().len();
        assert_eq!(state.last_sequence_event_bytes, second_bytes);
        assert_eq!(state.retained_budget.used_bytes(), 7 + second_bytes);
    }

    #[test]
    fn retained_budget_replace_release_and_underflow_are_checked() {
        let mut budget = RetainedStateBudget::default();
        let owner = RetainedStateOwner::FunctionArguments(7);
        budget
            .reserve(owner, MAX_BUFFERED_TRANSLATION_BYTES)
            .expect("exact retained limit");
        assert!(budget.reserve(owner, 1).is_err());
        assert!(budget
            .reserve(RetainedStateOwner::FunctionArguments(8), 1)
            .is_err());
        assert_eq!(budget.used_bytes(), MAX_BUFFERED_TRANSLATION_BYTES);
        budget
            .replace(owner, 3)
            .expect("authoritative replacement releases old bytes");
        assert_eq!(budget.used_bytes(), 3);
        budget.replace(owner, 7).expect("replacement can grow");
        assert_eq!(budget.used_bytes(), 7);
        assert_eq!(budget.owner_bytes(owner), 7);
        budget.release(owner).expect("owner removal releases bytes");
        assert_eq!(budget.used_bytes(), 0);
        assert!(budget.release(owner).is_err());

        let utf8_owner = RetainedStateOwner::ReasoningContent(0, 0);
        budget
            .reserve(utf8_owner, "é".len())
            .expect("UTF-8 bytes are counted");
        assert_eq!(budget.used_bytes(), 2);
        assert!(budget
            .replace(utf8_owner, MAX_BUFFERED_TRANSLATION_BYTES + 1)
            .is_err());
        assert_eq!(
            budget.owner_bytes(utf8_owner),
            2,
            "failed replacement is atomic"
        );
    }

    #[test]
    fn cross_budget_preflight_is_atomic_at_exact_and_plus_one() {
        let mut exact = create_responses_stream_state(None);
        let mut transaction = ResponsesBudgetTransaction::default();
        transaction
            .reserve_output(MAX_UPSTREAM_RESPONSE_BYTES)
            .unwrap();
        transaction.reserve_retained(
            RetainedStateOwner::PendingOutputItem(0),
            MAX_BUFFERED_TRANSLATION_BYTES,
        );
        transaction.commit(&mut exact).expect("exact limits commit");
        assert_eq!(
            budget_snapshot(&exact),
            (
                MAX_UPSTREAM_RESPONSE_BYTES,
                MAX_BUFFERED_TRANSLATION_BYTES,
                HashMap::from([(
                    RetainedStateOwner::PendingOutputItem(0),
                    MAX_BUFFERED_TRANSLATION_BYTES
                )]),
            )
        );

        let before = budget_snapshot(&exact);
        let mut plus_one = ResponsesBudgetTransaction::default();
        plus_one.reserve_output(1).unwrap();
        plus_one.replace_retained(
            RetainedStateOwner::PendingOutputItem(0),
            MAX_BUFFERED_TRANSLATION_BYTES,
        );
        assert!(plus_one.commit(&mut exact).is_err());
        assert_eq!(budget_snapshot(&exact), before);

        let mut retained_full = create_responses_stream_state(None);
        retained_full
            .retained_budget
            .reserve(
                RetainedStateOwner::SequenceSnapshot,
                MAX_BUFFERED_TRANSLATION_BYTES,
            )
            .unwrap();
        let before = budget_snapshot(&retained_full);
        let mut output_room = ResponsesBudgetTransaction::default();
        output_room.reserve_output(1).unwrap();
        output_room.reserve_retained(RetainedStateOwner::OutputItemMetadata(0), 1);
        assert!(output_room.commit(&mut retained_full).is_err());
        assert_eq!(
            budget_snapshot(&retained_full),
            before,
            "retained pressure cannot charge un-emitted output"
        );

        let mut lifecycle_pressure = create_responses_stream_state(None);
        lifecycle_pressure
            .retained_budget
            .reserve(
                RetainedStateOwner::SequenceSnapshot,
                MAX_BUFFERED_TRANSLATION_BYTES - 5,
            )
            .unwrap();
        let before = budget_snapshot(&lifecycle_pressure);
        let mut lifecycle = ResponsesBudgetTransaction::default();
        lifecycle.reserve_retained(RetainedStateOwner::PendingOutputItem(0), 3);
        lifecycle.reserve_retained(RetainedStateOwner::OutputItemMetadata(0), 3);
        assert!(lifecycle.commit(&mut lifecycle_pressure).is_err());
        assert_eq!(
            budget_snapshot(&lifecycle_pressure),
            before,
            "lifecycle owner groups commit all-or-none"
        );

        let mut output_full = create_responses_stream_state(None);
        output_full.output_budget.used_bytes = MAX_UPSTREAM_RESPONSE_BYTES;
        let before = budget_snapshot(&output_full);
        let mut retained_room = ResponsesBudgetTransaction::default();
        retained_room.reserve_output(1).unwrap();
        retained_room.reserve_retained(RetainedStateOwner::OutputItemMetadata(0), 1);
        assert!(retained_room.commit(&mut output_full).is_err());
        assert_eq!(
            budget_snapshot(&output_full),
            before,
            "output pressure cannot create a retained owner"
        );
    }

    #[test]
    fn owner_replacement_growth_and_shrink_are_transactional() {
        let mut state = create_responses_stream_state(None);
        state
            .retained_budget
            .reserve(RetainedStateOwner::FunctionArguments(0), 4)
            .unwrap();

        let mut grow = ResponsesBudgetTransaction::default();
        grow.reserve_output(2).unwrap();
        grow.replace_retained(RetainedStateOwner::FunctionArguments(0), 6);
        grow.commit(&mut state).unwrap();
        assert_eq!(state.output_budget.used_bytes, 2);
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::FunctionArguments(0)),
            6
        );

        let mut shrink = ResponsesBudgetTransaction::default();
        shrink.replace_retained(RetainedStateOwner::FunctionArguments(0), 2);
        shrink.commit(&mut state).unwrap();
        assert_eq!(state.output_budget.used_bytes, 2);
        assert_eq!(state.retained_budget.used_bytes(), 2);

        state
            .retained_budget
            .reserve(
                RetainedStateOwner::SequenceSnapshot,
                MAX_BUFFERED_TRANSLATION_BYTES - 2,
            )
            .unwrap();
        let before = budget_snapshot(&state);
        let mut failed_growth = ResponsesBudgetTransaction::default();
        failed_growth.reserve_output(1).unwrap();
        failed_growth.replace_retained(RetainedStateOwner::FunctionArguments(0), 3);
        assert!(failed_growth.commit(&mut state).is_err());
        assert_eq!(budget_snapshot(&state), before);
    }

    #[test]
    fn active_inactive_and_done_argument_failures_do_not_mutate_budgets() {
        let mut state = create_responses_stream_state(None);
        let mut events = Vec::new();
        for (index, id, name) in [(0, "active", "first"), (1, "inactive", "second")] {
            reserve_function_call_metadata_if_new(&mut state, index, Some(id), name).unwrap();
            open_function_call_block(&mut state, index, Some(id), Some(name), &mut events).unwrap();
        }
        fill_remaining_retained_budget(&mut state, RetainedStateOwner::SequenceSnapshot);
        for index in [0, 1] {
            let before = budget_snapshot(&state);
            let event_count = events.len();
            assert!(append_function_call_arguments(
                &mut state,
                index,
                "x".to_string(),
                &mut events
            )
            .is_err());
            assert_eq!(budget_snapshot(&state), before, "call {index}");
            assert_eq!(events.len(), event_count, "call {index}");
        }

        for index in [0, 1] {
            let mut state = create_responses_stream_state(None);
            let mut events = Vec::new();
            for (call_index, id, name) in [(0, "active", "first"), (1, "inactive", "second")] {
                reserve_function_call_metadata_if_new(&mut state, call_index, Some(id), name)
                    .unwrap();
                open_function_call_block(&mut state, call_index, Some(id), Some(name), &mut events)
                    .unwrap();
            }
            append_function_call_arguments(&mut state, index, "{\"v\":".to_string(), &mut events)
                .unwrap();
            fill_remaining_retained_budget(&mut state, RetainedStateOwner::SequenceSnapshot);
            let before = budget_snapshot(&state);
            let event_count = events.len();
            assert!(
                reconcile_function_call_arguments(&mut state, index, "{\"v\":1}", &mut events,)
                    .is_err()
            );
            assert_eq!(budget_snapshot(&state), before, "done call {index}");
            assert_eq!(events.len(), event_count, "done call {index}");
        }
    }

    #[test]
    fn text_reasoning_signature_compaction_and_block_key_preflights_are_atomic() {
        let mut retained_full = create_responses_stream_state(None);
        fill_remaining_retained_budget(&mut retained_full, RetainedStateOwner::SequenceSnapshot);
        let before = budget_snapshot(&retained_full);
        let mut events = Vec::new();
        assert!(append_output_text(&mut retained_full, 0, 0, "x", &mut events).is_err());
        assert_eq!(budget_snapshot(&retained_full), before);
        assert!(events.is_empty());
        assert!(!retained_full.block_index_by_key.contains_key("0:0"));

        for output_bytes in [
            "reasoning".len() + "enc@id".len(),
            THINKING_TEXT.len() + "cm1#encrypted@id".len(),
        ] {
            let mut state = create_responses_stream_state(None);
            fill_remaining_retained_budget(&mut state, RetainedStateOwner::SequenceSnapshot);
            let before = budget_snapshot(&state);
            assert!(reserve_thinking_output(&mut state, 0, output_bytes).is_err());
            assert_eq!(budget_snapshot(&state), before);
            assert!(!state.block_index_by_key.contains_key("0:0"));
        }

        let mut output_full = create_responses_stream_state(None);
        output_full.output_budget.used_bytes = MAX_UPSTREAM_RESPONSE_BYTES;
        let before = budget_snapshot(&output_full);
        assert!(append_output_text(&mut output_full, 0, 0, "x", &mut events).is_err());
        assert_eq!(budget_snapshot(&output_full), before);
        assert!(reserve_thinking_output(&mut output_full, 0, 1).is_err());
        assert_eq!(budget_snapshot(&output_full), before);
    }

    #[test]
    fn failed_atomic_transition_emits_once_and_terminal_cleanup_releases_owners() {
        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"message","id":"atomic-error","role":"assistant","content":[]}
            }),
            &mut state,
        )
        .is_empty());
        fill_remaining_retained_budget(&mut state, RetainedStateOwner::SequenceSnapshot);
        let output_before = state.output_budget.used_bytes;
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "content_index":0,
                "delta":"not-emitted"
            }),
            &mut state,
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AnthropicStreamEventData::Error { .. }))
                .count(),
            1
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, AnthropicStreamEventData::MessageStop)));
        assert_eq!(
            state.output_budget.used_bytes, output_before,
            "failed payload was never charged"
        );
        assert!(state.translation_failed);
        assert!(state.retained_budget.owners.is_empty());
        assert_eq!(state.retained_budget.used_bytes(), 0);
        assert!(state.block_index_by_key.is_empty());
    }

    #[test]
    fn multi_block_message_done_preflights_all_output_before_emission() {
        let mut state = started_state();
        state.output_budget.used_bytes = MAX_UPSTREAM_RESPONSE_BYTES - 1;
        let output_before = state.output_budget.used_bytes;
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"message",
                    "id":"multi-block-atomic",
                    "role":"assistant",
                    "status":"completed",
                    "content":[
                        {"type":"output_text","text":"a"},
                        {"type":"output_text","text":"b"}
                    ]
                }
            }),
            &mut state,
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AnthropicStreamEventData::Error { .. }))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            AnthropicStreamEventData::ContentBlockDelta { .. }
                | AnthropicStreamEventData::MessageStop
        )));
        assert_eq!(state.output_budget.used_bytes, output_before);
        assert!(state.retained_budget.owners.is_empty());
    }

    #[test]
    fn reasoning_and_compaction_done_fail_without_partial_lifecycle_charge() {
        for item in [
            json!({
                "type":"reasoning",
                "id":"reasoning-atomic",
                "summary":[{"type":"summary_text","text":"reason"}],
                "encrypted_content":"enc",
                "status":"completed"
            }),
            json!({
                "type":"compaction",
                "id":"compaction-atomic",
                "encrypted_content":"encrypted",
                "status":"completed"
            }),
        ] {
            let mut state = started_state();
            state.output_budget.used_bytes = MAX_UPSTREAM_RESPONSE_BYTES;
            let output_before = state.output_budget.used_bytes;
            let events = translate_responses_stream_event(
                &json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":item
                }),
                &mut state,
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AnthropicStreamEventData::Error { .. }))
                    .count(),
                1
            );
            assert!(!events
                .iter()
                .any(|event| matches!(event, AnthropicStreamEventData::MessageStop)));
            assert_eq!(state.output_budget.used_bytes, output_before);
            assert!(state.retained_budget.owners.is_empty());
            assert!(state.output_items_by_index.is_empty());
        }
    }

    #[test]
    fn lifecycle_digests_ignore_object_key_order_but_preserve_array_order() {
        let first: Value = serde_json::from_str(
            r#"{"type":"message","id":"ordered","role":"assistant","content":[],"future":{"a":1,"b":2}}"#,
        )
        .unwrap();
        let reordered: Value = serde_json::from_str(
            r#"{"future":{"b":2,"a":1},"content":[],"role":"assistant","id":"ordered","type":"message"}"#,
        )
        .unwrap();
        assert_eq!(
            raw_output_item_digest(&first),
            raw_output_item_digest(&reordered)
        );
        assert_eq!(
            canonical_output_item_digest(&first, OutputValidationPhase::Added),
            canonical_output_item_digest(&reordered, OutputValidationPhase::Added)
        );

        let array_a = json!({"type":"message","id":"ordered","role":"assistant","content":[
            {"type":"output_text","text":"a"},
            {"type":"output_text","text":"b"}
        ]});
        let array_b = json!({"type":"message","id":"ordered","role":"assistant","content":[
            {"type":"output_text","text":"b"},
            {"type":"output_text","text":"a"}
        ]});
        assert_ne!(
            raw_output_item_digest(&array_a),
            raw_output_item_digest(&array_b)
        );
    }

    #[test]
    fn parallel_function_buffers_reserve_replace_and_release_exactly() {
        let mut state = create_responses_stream_state(None);
        let mut events = Vec::new();
        for (index, id, name) in [(0, "a", "f"), (1, "b", "g")] {
            reserve_function_call_metadata_if_new(&mut state, index, Some(id), name)
                .expect("tool metadata fits");
            open_function_call_block(&mut state, index, Some(id), Some(name), &mut events)
                .expect("tool block opens");
        }
        let metadata_bytes = 4;
        let argument_bytes = MAX_BUFFERED_TRANSLATION_BYTES - metadata_bytes;
        let first_len = argument_bytes / 2;
        let second_len = argument_bytes - first_len;
        append_function_call_arguments(&mut state, 0, "a".repeat(first_len), &mut events)
            .expect("active call fits");
        append_function_call_arguments(&mut state, 1, "b".repeat(second_len), &mut events)
            .expect("inactive call is retained once");
        assert_eq!(
            state.retained_budget.used_bytes(),
            MAX_BUFFERED_TRANSLATION_BYTES
        );
        assert_eq!(state.output_budget.used_bytes, MAX_UPSTREAM_RESPONSE_BYTES);
        assert!(
            append_function_call_arguments(&mut state, 1, "x".to_string(), &mut events).is_err()
        );
        assert_eq!(
            state.retained_budget.used_bytes(),
            MAX_BUFFERED_TRANSLATION_BYTES
        );

        state
            .function_call_state_by_output_index
            .get_mut(&0)
            .expect("first call")
            .done = true;
        advance_function_call_queue(&mut state, &mut events).expect("first call drains");
        assert_eq!(
            state.retained_budget.used_bytes(),
            2 + second_len,
            "only the inactive call remains retained"
        );
        state
            .function_call_state_by_output_index
            .get_mut(&1)
            .expect("second call")
            .done = true;
        advance_function_call_queue(&mut state, &mut events).expect("second call drains");
        assert_eq!(state.retained_budget.used_bytes(), 0);
        assert!(state
            .retained_budget
            .release(RetainedStateOwner::FunctionArguments(1))
            .is_err());
    }

    #[test]
    fn inactive_authoritative_arguments_replace_one_owner_then_drain() {
        let mut state = create_responses_stream_state(None);
        let mut events = Vec::new();
        for (index, id, name) in [(0, "active", "first"), (1, "inactive", "second")] {
            reserve_function_call_metadata_if_new(&mut state, index, Some(id), name)
                .expect("tool metadata fits");
            open_function_call_block(&mut state, index, Some(id), Some(name), &mut events)
                .expect("tool block opens");
        }
        append_function_call_arguments(&mut state, 0, "{}".to_string(), &mut events)
            .expect("active arguments fit");
        append_function_call_arguments(&mut state, 1, "{\"v\":".to_string(), &mut events)
            .expect("inactive prefix fits");
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::FunctionArguments(1)),
            "{\"v\":".len()
        );

        reconcile_function_call_arguments(&mut state, 1, "{\"v\":1}", &mut events)
            .expect("authoritative suffix replaces the inactive buffer");
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::FunctionArguments(1)),
            "{\"v\":1}".len(),
            "the authoritative value owns its current bytes exactly once"
        );
        assert_eq!(
            state.output_budget.used_bytes,
            "active".len()
                + "first".len()
                + "inactive".len()
                + "second".len()
                + "{}".len()
                + "{\"v\":1}".len()
        );

        state
            .function_call_state_by_output_index
            .get_mut(&0)
            .expect("first call")
            .done = true;
        advance_function_call_queue(&mut state, &mut events).expect("first call drains");
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::FunctionArguments(1)),
            "{\"v\":1}".len()
        );
        state
            .function_call_state_by_output_index
            .get_mut(&1)
            .expect("second call")
            .done = true;
        advance_function_call_queue(&mut state, &mut events).expect("second call drains");
        assert_eq!(state.retained_budget.used_bytes(), 0);
    }

    #[test]
    fn reasoning_authority_replaces_and_releases_its_buffer() {
        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"reasoning-owner","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.reasoning_summary_text.delta",
                "item_id":"reasoning-owner",
                "output_index":0,
                "summary_index":0,
                "delta":"é"
            }),
            &mut state,
        )
        .is_empty());
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::ReasoningSummary(0, 0)),
            "é".len()
        );
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.reasoning_summary_text.done",
                "item_id":"reasoning-owner",
                "output_index":0,
                "summary_index":0,
                "text":"éx"
            }),
            &mut state,
        )
        .is_empty());
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::ReasoningSummary(0, 0)),
            "éx".len()
        );
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"reasoning",
                    "id":"reasoning-owner",
                    "summary":[{"type":"summary_text","text":"éx"}],
                    "encrypted_content":"enc",
                    "status":"completed"
                }
            }),
            &mut state,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::SignatureDelta { .. },
                ..
            }
        )));
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::ReasoningSummary(0, 0)),
            0
        );
    }

    #[test]
    fn late_reasoning_id_reservation_is_released_with_completed_state() {
        let mut state = started_state();
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","summary":[]}
            }),
            &mut state,
        )
        .is_empty());
        let events = translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"reasoning",
                    "id":"late-reasoning-id",
                    "summary":[{"type":"summary_text","text":"late"}],
                    "encrypted_content":"enc",
                    "status":"completed"
                }
            }),
            &mut state,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AnthropicStreamEventData::ContentBlockDelta {
                delta: AnthropicContentBlockDelta::SignatureDelta { .. },
                ..
            }
        )));
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::ReasoningItemId(0)),
            0
        );
        assert!(!state.reasoning_state_by_output_index.contains_key(&0));
    }

    #[test]
    fn completed_payload_history_is_released_and_terminal_cleanup_is_empty() {
        let mut state = started_state();
        let extension = "x".repeat(64 * 1024);
        let added = json!({
            "type":"message",
            "id":"history-release",
            "role":"assistant",
            "content":[],
            "future_large_assertion":extension
        });
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":added
            }),
            &mut state,
        )
        .is_empty());
        assert!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::PendingOutputItem(0))
                > 64 * 1024
        );
        assert!(translate_responses_stream_event(
            &json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"message",
                    "id":"history-release",
                    "role":"assistant",
                    "status":"completed",
                    "content":[],
                    "future_large_assertion":extension
                }
            }),
            &mut state,
        )
        .is_empty());
        assert_eq!(
            state
                .retained_budget
                .owner_bytes(RetainedStateOwner::PendingOutputItem(0)),
            0,
            "a completed snapshot is retained only as a fixed-size digest"
        );

        let events = translate_responses_stream_event(
            &json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_test",
                    "status":"completed",
                    "usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}
                }
            }),
            &mut state,
        );
        assert!(matches!(
            events.last(),
            Some(AnthropicStreamEventData::MessageStop)
        ));
        assert_eq!(state.retained_budget.used_bytes(), 0);
        assert!(state.retained_budget.owners.is_empty());
    }
}
