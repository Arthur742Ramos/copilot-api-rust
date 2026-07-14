//! Collect a validated Responses SSE stream into one typed terminal result.
//!
//! The ordinary streaming translator remains the lifecycle authority. This
//! module retains only complete `response.output_item.done` snapshots so a
//! non-streaming Anthropic caller can receive JSON even when Codex requires SSE.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use serde_json::{Map, Value};

use crate::libs::error::AppError;
use crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES;
use crate::routes::messages::anthropic_types::AnthropicStreamEventData;
use crate::routes::messages::responses_stream_translation::{
    translate_responses_stream_event, validate_event_sequence, ResponsesStreamState,
};
use crate::routes::messages::responses_translation::{
    validate_complete_responses_result, validate_raw_responses_usage,
};
use crate::services::copilot::create_responses::{ResponsesEventStream, ResponsesResult};

const MAX_COLLECTED_OUTPUT_ITEMS: usize = 4_096;

#[allow(clippy::result_large_err)]
pub(crate) async fn collect_responses_stream_result_with_usage_observer<F>(
    mut upstream: ResponsesEventStream,
    error_message_prefix: &str,
    requested_model: Option<&str>,
    mut observe_valid_terminal_usage: F,
) -> Result<ResponsesResult, AppError>
where
    F: FnMut(&Value),
{
    let mut state = ResponsesStreamState::new_with_model(None, requested_model.map(str::to_string));
    let mut done_items = BTreeMap::<i64, Value>::new();
    let mut retained_item_bytes = 0usize;
    let mut terminal_response = None::<Value>;
    let mut created_fields = Map::<String, Value>::new();
    let mut usage_observed = false;

    while let Some(next) = upstream.next().await {
        let event =
            next.map_err(|error| collector_error(error_message_prefix, &error.to_string()))?;

        if event.event.as_deref() == Some("ping") || event.data.is_empty() {
            continue;
        }
        if event.data == "[DONE]" {
            break;
        }

        let mut parsed: Value = serde_json::from_str(&event.data).map_err(|_| {
            collector_error(error_message_prefix, "returned a malformed JSON event")
        })?;
        let event_type = parsed
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                collector_error(
                    error_message_prefix,
                    "returned an event without a valid type",
                )
            })?
            .to_string();
        if let Some(sse_name) = event.event.as_deref() {
            if sse_name != event_type {
                return Err(collector_error(
                    error_message_prefix,
                    "returned mismatched SSE and JSON event types",
                ));
            }
        }

        if matches!(
            event_type.as_str(),
            "codex.rate_limits" | "response.metadata"
        ) {
            validate_event_sequence(&parsed, &mut state)
                .map_err(|message| collector_error(error_message_prefix, message))?;
            if event_type == "codex.rate_limits" {
                crate::libs::codex_rate_limit::log_codex_rate_limits_event(&parsed);
            }
            continue;
        }

        let is_terminal = matches!(
            event_type.as_str(),
            "response.completed" | "response.incomplete" | "response.failed"
        );
        if is_terminal && !usage_observed {
            if let Some(response) = parsed.get("response") {
                if validate_raw_responses_usage(response).is_ok() {
                    observe_valid_terminal_usage(response);
                    usage_observed = true;
                }
            }
        }

        if matches!(
            event_type.as_str(),
            "response.completed" | "response.incomplete"
        ) {
            if terminal_output_is_authoritative(&parsed) {
                done_items.clear();
                retained_item_bytes = 0;
            }
            normalize_terminal_event(
                &mut parsed,
                &event_type,
                &created_fields,
                &done_items,
                retained_item_bytes,
                requested_model,
                error_message_prefix,
            )?;
        }

        let translated = translate_responses_stream_event(&parsed, &mut state);
        if let Some(message) = translated.iter().find_map(translated_error_message) {
            return Err(collector_error(error_message_prefix, message));
        }

        if event_type == "response.created" {
            created_fields = capture_created_fields(parsed.get("response"));
        } else if event_type == "response.output_item.done" {
            collect_done_item(
                &parsed,
                &mut done_items,
                &mut retained_item_bytes,
                error_message_prefix,
            )?;
        }

        if is_terminal {
            terminal_response = parsed
                .as_object_mut()
                .and_then(|event| event.remove("response"));
            done_items.clear();
            break;
        }
    }

    if terminal_response.is_none() || !state.message_completed || state.translation_failed {
        return Err(collector_error(
            error_message_prefix,
            "ended without a valid terminal response",
        ));
    }

    let terminal = terminal_response.ok_or_else(|| {
        collector_error(
            error_message_prefix,
            "ended without a terminal response object",
        )
    })?;

    let result: ResponsesResult = serde_json::from_value(terminal).map_err(|error| {
        collector_error(
            error_message_prefix,
            &format!("returned an invalid terminal response: {error}"),
        )
    })?;
    validate_complete_responses_result(&result)
        .map_err(|error| collector_error(error_message_prefix, &error.to_string()))?;
    Ok(result)
}

fn terminal_output_is_authoritative(event: &Value) -> bool {
    match event.pointer("/response/output") {
        Some(Value::Array(output)) => !output.is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

fn capture_created_fields(response: Option<&Value>) -> Map<String, Value> {
    let Some(response) = response.and_then(Value::as_object) else {
        return Map::new();
    };
    ["id", "object", "created_at", "model"]
        .into_iter()
        .filter_map(|key| {
            response
                .get(key)
                .cloned()
                .map(|value| (key.to_string(), value))
        })
        .collect()
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn normalize_terminal_event(
    event: &mut Value,
    event_type: &str,
    created_fields: &Map<String, Value>,
    done_items: &BTreeMap<i64, Value>,
    retained_item_bytes: usize,
    requested_model: Option<&str>,
    error_message_prefix: &str,
) -> Result<(), AppError> {
    let response = event
        .get_mut("response")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            collector_error(
                error_message_prefix,
                "ended without a terminal response object",
            )
        })?;

    for key in ["object", "created_at"] {
        if response.get(key).is_none_or(Value::is_null) {
            if let Some(value) = created_fields.get(key) {
                response.insert(key.to_string(), value.clone());
            }
        }
    }

    let created_model = created_fields
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty());
    match response.get("model") {
        None | Some(Value::Null) => {
            if let Some(model) =
                created_model.or_else(|| requested_model.filter(|model| !model.trim().is_empty()))
            {
                response.insert("model".to_string(), Value::String(model.to_string()));
            }
        }
        Some(Value::String(model)) if !model.trim().is_empty() => {
            if created_model.is_some_and(|created| created != model) {
                return Err(collector_error(
                    error_message_prefix,
                    "terminal response model did not match response.created",
                ));
            }
        }
        Some(_) => {}
    }

    if matches!(response.get("status"), None | Some(Value::Null)) {
        let status = if event_type == "response.completed" {
            "completed"
        } else {
            "incomplete"
        };
        response.insert("status".to_string(), Value::String(status.to_string()));
    }

    let output_is_reconstructable = match response.get("output") {
        None | Some(Value::Null) => true,
        Some(Value::Array(output)) => output.is_empty(),
        Some(_) => false,
    };
    if output_is_reconstructable {
        if done_items.is_empty() {
            response.insert("output".to_string(), Value::Array(Vec::new()));
        } else {
            ensure_contiguous_indexes(done_items, error_message_prefix)?;
            let base_bytes = serde_json::to_vec(&response)
                .map_err(|error| collector_error(error_message_prefix, &error.to_string()))?
                .len();
            if base_bytes
                .checked_add(retained_item_bytes)
                .is_none_or(|bytes| bytes > MAX_UPSTREAM_RESPONSE_BYTES)
            {
                return Err(collector_error(
                    error_message_prefix,
                    "exceeded its reconstructed response budget",
                ));
            }
            response.insert(
                "output".to_string(),
                Value::Array(done_items.values().cloned().collect()),
            );
        }
    }

    let response_bytes = serde_json::to_vec(&response)
        .map_err(|error| collector_error(error_message_prefix, &error.to_string()))?
        .len();
    if response_bytes > MAX_UPSTREAM_RESPONSE_BYTES {
        return Err(collector_error(
            error_message_prefix,
            "exceeded its reconstructed response budget",
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn collect_done_item(
    event: &Value,
    done_items: &mut BTreeMap<i64, Value>,
    retained_item_bytes: &mut usize,
    error_message_prefix: &str,
) -> Result<(), AppError> {
    let output_index = event
        .get("output_index")
        .and_then(Value::as_i64)
        .filter(|index| *index >= 0)
        .ok_or_else(|| {
            collector_error(
                error_message_prefix,
                "returned output_item.done without a valid output_index",
            )
        })?;
    let item = event.get("item").cloned().ok_or_else(|| {
        collector_error(
            error_message_prefix,
            "returned output_item.done without an item",
        )
    })?;

    if let Some(existing) = done_items.get(&output_index) {
        if existing == &item {
            return Ok(());
        }
        return Err(collector_error(
            error_message_prefix,
            "returned conflicting output_item.done snapshots",
        ));
    }
    if done_items.len() >= MAX_COLLECTED_OUTPUT_ITEMS {
        return Err(collector_error(
            error_message_prefix,
            "returned too many output items",
        ));
    }

    let item_bytes = serde_json::to_vec(&item)
        .map_err(|error| collector_error(error_message_prefix, &error.to_string()))?
        .len();
    let next_bytes = retained_item_bytes.checked_add(item_bytes).ok_or_else(|| {
        collector_error(
            error_message_prefix,
            "overflowed its retained output budget",
        )
    })?;
    if next_bytes > MAX_UPSTREAM_RESPONSE_BYTES {
        return Err(collector_error(
            error_message_prefix,
            "exceeded its retained output budget",
        ));
    }

    *retained_item_bytes = next_bytes;
    done_items.insert(output_index, item);
    Ok(())
}

#[allow(clippy::result_large_err)]
fn ensure_contiguous_indexes(
    items: &BTreeMap<i64, Value>,
    error_message_prefix: &str,
) -> Result<(), AppError> {
    for (expected, actual) in items.keys().enumerate() {
        if *actual != expected as i64 {
            return Err(collector_error(
                error_message_prefix,
                "returned sparse output item indexes",
            ));
        }
    }
    Ok(())
}

fn translated_error_message(event: &AnthropicStreamEventData) -> Option<&str> {
    match event {
        AnthropicStreamEventData::Error { error } => Some(error.message.as_str()),
        _ => None,
    }
}

fn collector_error(prefix: &str, detail: &str) -> AppError {
    AppError::Other(anyhow::anyhow!("{prefix}: {detail}"))
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        task::{Context, Poll},
    };

    use futures_util::{stream, Stream};
    use serde_json::json;

    use super::*;
    use crate::libs::sse::SseEvent;

    fn event(kind: &str, value: Value) -> Result<SseEvent, std::io::Error> {
        Ok(SseEvent {
            event: Some(kind.to_string()),
            data: value.to_string(),
            ..Default::default()
        })
    }

    fn done() -> Result<SseEvent, std::io::Error> {
        Ok(SseEvent {
            data: "[DONE]".to_string(),
            ..Default::default()
        })
    }

    fn boxed(events: Vec<Result<SseEvent, std::io::Error>>) -> ResponsesEventStream {
        Box::pin(stream::iter(events))
    }

    struct PendingDropStream {
        dropped: Arc<AtomicBool>,
        polled: Arc<tokio::sync::Notify>,
    }

    impl Stream for PendingDropStream {
        type Item = Result<SseEvent, std::io::Error>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.polled.notify_one();
            Poll::Pending
        }
    }

    impl Drop for PendingDropStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn created() -> Value {
        json!({
            "type":"response.created",
            "response":{
                "id":"resp_collect",
                "object":"response",
                "created_at":1,
                "model":"gpt-5.4",
                "output":[],
                "status":"in_progress"
            }
        })
    }

    fn completed(output: Value) -> Value {
        json!({
            "type":"response.completed",
            "response":{
                "id":"resp_collect",
                "object":"response",
                "created_at":1,
                "model":"gpt-5.4",
                "output":output,
                "status":"completed",
                "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
            }
        })
    }

    #[tokio::test]
    async fn collects_done_items_in_output_index_order_and_observes_usage_once() {
        let second = json!({
            "type":"message","id":"msg_2","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"second","annotations":[]}]
        });
        let first = json!({
            "type":"message","id":"msg_1","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"first","annotations":[]}]
        });
        let observed = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&observed);
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":1,"item":second}),
                ),
                event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":0,"item":first}),
                ),
                event("response.completed", completed(json!([]))),
                done(),
            ]),
            "collector",
            Some("gpt-5.4"),
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();

        assert_eq!(result.output.len(), 2);
        assert_eq!(
            serde_json::to_value(&result.output[0]).unwrap()["id"],
            "msg_1"
        );
        assert_eq!(
            serde_json::to_value(&result.output[1]).unwrap()["id"],
            "msg_2"
        );
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn collects_function_call_for_nonstream_translation() {
        let call = json!({
            "type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup",
            "arguments":"{\"q\":\"rust\"}","status":"completed"
        });
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":0,"item":call}),
                ),
                event("response.completed", completed(json!([]))),
                done(),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(&result.output[0]).unwrap()["call_id"],
            "call_1"
        );
    }

    #[tokio::test]
    async fn terminal_less_fails_and_valid_terminal_returns_immediately() {
        let missing = collect_responses_stream_result_with_usage_observer(
            boxed(vec![event("response.created", created()), done()]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(missing.is_err());

        let late = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event("response.completed", completed(json!([]))),
                event("response.output_text.delta", json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"late"})),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(late.is_ok());
    }

    #[tokio::test]
    async fn malformed_mismatched_and_failed_events_fail_closed() {
        let malformed = collect_responses_stream_result_with_usage_observer(
            boxed(vec![Ok(SseEvent {
                event: Some("response.created".into()),
                data: "{".into(),
                ..Default::default()
            })]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(malformed.is_err());

        let mismatched = collect_responses_stream_result_with_usage_observer(
            boxed(vec![event("wrong.type", created())]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(mismatched.is_err());

        let failed = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event("response.failed", json!({
                    "type":"response.failed",
                    "response":{"id":"resp_collect","model":"gpt-5.4","output":[],"status":"failed","error":{"message":"boom"}}
                })),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(failed.is_err());
    }

    #[tokio::test]
    async fn partial_completed_terminal_uses_created_fields_and_defaults() {
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event(
                    "response.created",
                    json!({
                        "type":"response.created",
                        "response":{
                            "id":"resp_partial",
                            "object":"response",
                            "created_at":1,
                            "model":"gpt-5.4-2026-06-01",
                            "output":[],
                            "status":"in_progress"
                        }
                    }),
                ),
                event(
                    "response.completed",
                    json!({
                        "type":"response.completed",
                        "response":{
                            "id":"resp_partial",
                            "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
                        }
                    }),
                ),
            ]),
            "collector",
            Some("gpt-5.4"),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(result.model, "gpt-5.4-2026-06-01");
        assert_eq!(result.status, "completed");
        assert!(result.output.is_empty());
        assert_eq!(result.object, "response");
    }

    #[tokio::test]
    async fn response_metadata_before_created_is_accepted_and_sequenced() {
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event(
                    "response.metadata",
                    json!({"type":"response.metadata","sequence_number":0,"metadata":{"x":1}}),
                ),
                event("response.created", created()),
                event("response.completed", completed(json!([]))),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(result.status, "completed");
    }

    #[tokio::test]
    async fn malformed_terminal_identity_status_and_model_fail_closed() {
        let cases = [
            json!({
                "type":"response.completed",
                "response":{
                    "model":"gpt-5.4","output":[],"status":"completed",
                    "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
                }
            }),
            json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_collect","model":"gpt-5.4","output":[],"status":7,
                    "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
                }
            }),
            json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_collect","model":"gpt-5.4","output":[],"status":"",
                    "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
                }
            }),
            json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_collect","model":"different-model","output":[],"status":"completed",
                    "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
                }
            }),
        ];
        for terminal in cases {
            let result = collect_responses_stream_result_with_usage_observer(
                boxed(vec![
                    event("response.created", created()),
                    event("response.completed", terminal),
                ]),
                "collector",
                None,
                |_| {},
            )
            .await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn matching_nonempty_terminal_output_is_authoritative() {
        let item = json!({
            "type":"message","id":"m1","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"done","annotations":[]}]
        });
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event(
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done","output_index":0,"item":item.clone()
                    }),
                ),
                event("response.completed", completed(json!([item]))),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(result.output.len(), 1);
    }

    #[tokio::test]
    async fn conflicting_or_malformed_terminal_output_fails_closed() {
        let done_item = json!({
            "type":"message","id":"m1","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"done","annotations":[]}]
        });
        for terminal_output in [
            json!([{
                "type":"message","id":"different","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"conflict","annotations":[]}]
            }]),
            json!({"not":"an array"}),
        ] {
            let result = collect_responses_stream_result_with_usage_observer(
                boxed(vec![
                    event("response.created", created()),
                    event(
                        "response.output_item.done",
                        json!({
                            "type":"response.output_item.done",
                            "output_index":0,
                            "item":done_item.clone()
                        }),
                    ),
                    event("response.completed", completed(terminal_output)),
                ]),
                "collector",
                None,
                |_| {},
            )
            .await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn rate_limit_events_participate_in_global_sequence_order() {
        let mut created = created();
        created["sequence_number"] = json!(0);
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created),
                event(
                    "codex.rate_limits",
                    json!({"type":"codex.rate_limits","sequence_number":2}),
                ),
                event(
                    "response.output_item.done",
                    json!({
                        "type":"response.output_item.done",
                        "sequence_number":1,
                        "output_index":0,
                        "item":{
                            "type":"message","id":"m1","role":"assistant",
                            "status":"completed","content":[]
                        }
                    }),
                ),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn combined_terminal_and_done_item_budget_is_bounded() {
        let large = "x".repeat(9 * 1024 * 1024);
        let item = json!({
            "type":"message","id":"m1","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":large,"annotations":[]}]
        });
        let mut terminal = completed(json!([]));
        terminal["response"]["large_terminal_extension"] =
            Value::String("y".repeat(9 * 1024 * 1024));
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":0,"item":item}),
                ),
                event("response.completed", terminal),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn supported_incomplete_terminal_is_collected() {
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event("response.incomplete", json!({
                    "type":"response.incomplete",
                    "response":{
                        "id":"resp_collect","object":"response","created_at":1,"model":"gpt-5.4",
                        "output":[],"status":"incomplete",
                        "incomplete_details":{"reason":"max_output_tokens"},
                        "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
                    }
                })),
                done(),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(result.status, "incomplete");
    }

    #[tokio::test]
    async fn valid_terminal_usage_is_observed_before_returning() {
        let observed = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&observed);
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event("response.completed", completed(json!([]))),
                event(
                    "response.output_text.delta",
                    json!({
                        "type":"response.output_text.delta",
                        "output_index":0,
                        "content_index":0,
                        "delta":"late"
                    }),
                ),
            ]),
            "collector",
            None,
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn sparse_done_item_indexes_fail_closed() {
        let item = json!({
            "type":"message","id":"m1","role":"assistant","status":"completed","content":[]
        });
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":1,"item":item}),
                ),
                event("response.completed", completed(json!([]))),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancelling_collection_drops_owned_upstream_stream() {
        let dropped = Arc::new(AtomicBool::new(false));
        let polled = Arc::new(tokio::sync::Notify::new());
        let upstream: ResponsesEventStream = Box::pin(PendingDropStream {
            dropped: Arc::clone(&dropped),
            polled: Arc::clone(&polled),
        });
        let task = tokio::spawn(collect_responses_stream_result_with_usage_observer(
            upstream,
            "collector",
            None,
            |_| {},
        ));
        tokio::time::timeout(std::time::Duration::from_secs(1), polled.notified())
            .await
            .expect("collector polled upstream");

        task.abort();
        let _ = task.await;
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn conflicting_duplicate_done_item_fails() {
        let first = json!({"type":"message","id":"m1","role":"assistant","status":"completed","content":[]});
        let second = json!({"type":"message","id":"m2","role":"assistant","status":"completed","content":[]});
        let result = collect_responses_stream_result_with_usage_observer(
            boxed(vec![
                event("response.created", created()),
                event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":0,"item":first}),
                ),
                event(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":0,"item":second}),
                ),
            ]),
            "collector",
            None,
            |_| {},
        )
        .await;
        assert!(result.is_err());
    }
}
