//! OpenAI Responses SSE lifecycle validation.
//!
//! Upstream Responses transports are not trusted to close cleanly. This guard
//! preserves valid events while ensuring malformed JSON, invalid lifecycle
//! ordering, transport failures, and premature EOF end in exactly one native
//! OpenAI error event rather than a fabricated `response.completed`.

use serde_json::{json, Value};

use crate::libs::sse::SseEvent;
use crate::routes::responses::stream_id_sync::{fix_stream_ids, StreamIdTracker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesTerminal {
    Completed,
    Failed,
}

#[derive(Debug)]
pub struct ProcessedResponsesEvent {
    pub frame: String,
    pub value: Value,
    pub terminal: Option<ResponsesTerminal>,
}

#[derive(Debug, Default)]
pub struct ResponsesStreamGuard {
    saw_created: bool,
    terminal: bool,
    response_id: Option<String>,
    model: Option<String>,
}

impl ResponsesStreamGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate and normalize one upstream event.
    ///
    /// Empty data records are ignored. `[DONE]` is not a Responses terminal
    /// event and is therefore rejected unless a terminal event already ended the
    /// stream (callers stop polling as soon as that happens).
    pub fn process(
        &mut self,
        event: &SseEvent,
        ids: &mut StreamIdTracker,
    ) -> Result<Option<ProcessedResponsesEvent>, &'static str> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(None);
        }
        if data == "[DONE]" {
            return Err("The upstream Responses stream ended before a terminal event.");
        }
        if self.terminal {
            return Err("The upstream Responses stream emitted data after its terminal event.");
        }

        let mut value: Value = serde_json::from_str(data)
            .map_err(|_| "The upstream Responses stream returned malformed JSON.")?;
        let event_type = value
            .as_object()
            .ok_or("The upstream Responses stream event must be a JSON object.")?
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or("The upstream Responses stream event is missing its type.")?
            .to_owned();

        if let Some(sse_name) = event.event.as_deref().filter(|name| !name.is_empty()) {
            if sse_name != event_type.as_str() {
                return Err("The upstream Responses stream event name does not match its type.");
            }
        }

        let mut rewrote_terminal_response_id = false;
        let terminal = match event_type.as_str() {
            "response.created" => {
                if self.saw_created {
                    return Err("The upstream Responses stream emitted response.created twice.");
                }
                let response = value
                    .get("response")
                    .and_then(Value::as_object)
                    .ok_or("The response.created event is missing its response object.")?;
                let response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .ok_or("The response.created event is missing its response id.")?;
                self.response_id = Some(response_id.to_owned());
                self.model = response
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.saw_created = true;
                None
            }
            "error" => {
                self.terminal = true;
                Some(ResponsesTerminal::Failed)
            }
            "response.completed" | "response.failed" | "response.incomplete" => {
                if !self.saw_created {
                    return Err(
                        "The upstream Responses stream terminated before response.created.",
                    );
                }
                let response = value
                    .get("response")
                    .and_then(Value::as_object)
                    .ok_or("The terminal Responses event is missing its response object.")?;
                let terminal_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .ok_or("The terminal Responses event is missing its response id.")?
                    .to_owned();
                let canonical_id = self
                    .response_id
                    .as_deref()
                    .expect("response.created established a response id");
                if terminal_id != canonical_id {
                    // Copilot may re-encrypt opaque response IDs between created
                    // and terminal snapshots. Keep the public lifecycle stable;
                    // event ordering and the single terminal still prove that
                    // both envelopes belong to this upstream stream.
                    value
                        .get_mut("response")
                        .and_then(Value::as_object_mut)
                        .expect("terminal response object was validated")
                        .insert("id".to_string(), Value::String(canonical_id.to_owned()));
                    rewrote_terminal_response_id = true;
                }
                self.terminal = true;
                Some(if event_type == "response.completed" {
                    ResponsesTerminal::Completed
                } else {
                    ResponsesTerminal::Failed
                })
            }
            // These metadata/keepalive events may legitimately precede
            // response.created on first-party Codex transports.
            "ping" | "codex.rate_limits" | "response.metadata" => None,
            _ => {
                if !self.saw_created {
                    return Err(
                        "The upstream Responses stream emitted output before response.created.",
                    );
                }
                None
            }
        };

        let normalized;
        let data = if rewrote_terminal_response_id {
            normalized = serde_json::to_string(&value)
                .map_err(|_| "The upstream Responses stream event could not be normalized.")?;
            normalized.as_str()
        } else {
            data
        };
        let processed = fix_stream_ids(data, Some(&event_type), ids);
        let processed_value = serde_json::from_str(&processed).unwrap_or_else(|_| value.clone());
        Ok(Some(ProcessedResponsesEvent {
            frame: build_sse_frame(event.id.as_deref(), Some(&event_type), &processed),
            value: processed_value,
            terminal,
        }))
    }

    /// Build one terminal OpenAI error event. Repeated calls are suppressed.
    pub fn fail(&mut self, code: &'static str, message: &'static str) -> Option<String> {
        if self.terminal {
            return None;
        }
        self.terminal = true;

        let value = if self.saw_created {
            json!({
                "type": "response.failed",
                "response": {
                    "id": self.response_id.clone().unwrap_or_default(),
                    "object": "response",
                    "created_at": 0,
                    "status": "failed",
                    "error": {
                        "type": "server_error",
                        "code": code,
                        "message": message,
                    },
                    "incomplete_details": Value::Null,
                    "model": self.model.clone().unwrap_or_default(),
                    "output": [],
                    "usage": Value::Null,
                }
            })
        } else {
            json!({
                "type": "error",
                "code": code,
                "message": message,
                "param": Value::Null,
            })
        };
        let event_type = value["type"].as_str().unwrap_or("error");
        let data = serde_json::to_string(&value).unwrap_or_else(|_| {
            "{\"type\":\"error\",\"code\":\"server_error\",\"message\":\"The upstream Responses stream failed.\",\"param\":null}".to_string()
        });
        Some(build_sse_frame(None, Some(event_type), &data))
    }
}

/// Build a standards-compliant SSE frame in deterministic field order.
pub fn build_sse_frame(id: Option<&str>, event: Option<&str>, data: &str) -> String {
    let mut frame = String::new();
    if let Some(event) = event {
        frame.push_str("event: ");
        frame.push_str(event);
        frame.push('\n');
    }
    if let Some(id) = id {
        frame.push_str("id: ");
        frame.push_str(id);
        frame.push('\n');
    }
    for line in data.split('\n') {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(event: Option<&str>, data: Value) -> SseEvent {
        SseEvent {
            id: None,
            event: event.map(str::to_owned),
            data: data.to_string(),
        }
    }

    #[test]
    fn accepts_created_output_and_one_completion() {
        let mut guard = ResponsesStreamGuard::new();
        let mut ids = StreamIdTracker::new();
        let created = event(
            Some("response.created"),
            json!({
                "type": "response.created",
                "response": {"id": "resp_1", "model": "gpt-5"}
            }),
        );
        assert!(guard.process(&created, &mut ids).unwrap().is_some());

        let delta = event(
            None,
            json!({"type":"response.output_text.delta","output_index":0,"delta":"hi"}),
        );
        let delta = guard.process(&delta, &mut ids).unwrap().unwrap();
        assert_eq!(delta.terminal, None);
        assert!(delta
            .frame
            .starts_with("event: response.output_text.delta\n"));

        let completed = event(
            Some("response.completed"),
            json!({
                "type":"response.completed",
                "response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1,"output_tokens":1}}
            }),
        );
        assert_eq!(
            guard
                .process(&completed, &mut ids)
                .unwrap()
                .unwrap()
                .terminal,
            Some(ResponsesTerminal::Completed)
        );
        assert!(guard.fail("server_error", "late").is_none());
    }

    #[test]
    fn malformed_and_eof_fail_exactly_once() {
        let mut guard = ResponsesStreamGuard::new();
        let mut ids = StreamIdTracker::new();
        let malformed = SseEvent {
            id: None,
            event: None,
            data: "{not json".to_string(),
        };
        assert!(guard.process(&malformed, &mut ids).is_err());
        let failure = guard
            .fail(
                "invalid_stream",
                "The upstream Responses stream returned malformed data.",
            )
            .unwrap();
        assert!(failure.contains("event: error"));
        assert_eq!(failure.matches("data: ").count(), 1);
        assert!(guard.fail("server_error", "again").is_none());
    }

    #[test]
    fn premature_eof_after_created_uses_response_failed() {
        let mut guard = ResponsesStreamGuard::new();
        let mut ids = StreamIdTracker::new();
        guard
            .process(
                &event(
                    None,
                    json!({
                        "type":"response.created",
                        "response":{"id":"resp_keep","model":"gpt-5"}
                    }),
                ),
                &mut ids,
            )
            .unwrap();
        let failure = guard
            .fail(
                "upstream_eof",
                "The upstream Responses stream ended before a terminal event.",
            )
            .unwrap();
        assert!(failure.starts_with("event: response.failed\n"));
        assert!(failure.contains("\"id\":\"resp_keep\""));
        assert!(!failure.contains("response.completed"));
    }

    #[test]
    fn rejects_terminal_before_created_and_mismatched_event_name() {
        let mut guard = ResponsesStreamGuard::new();
        let mut ids = StreamIdTracker::new();
        let completed = event(
            None,
            json!({"type":"response.completed","response":{"id":"r"}}),
        );
        assert!(guard.process(&completed, &mut ids).is_err());

        let mismatch = event(
            Some("response.output_text.delta"),
            json!({"type":"response.created","response":{"id":"r"}}),
        );
        assert!(guard.process(&mismatch, &mut ids).is_err());
    }

    #[test]
    fn canonicalizes_rotated_terminal_response_id() {
        let mut guard = ResponsesStreamGuard::new();
        let mut ids = StreamIdTracker::new();
        guard
            .process(
                &event(
                    None,
                    json!({
                        "type":"response.created",
                        "response":{"id":"resp_created","model":"gpt-5"}
                    }),
                ),
                &mut ids,
            )
            .unwrap();
        let completed = event(
            None,
            json!({
                "type":"response.completed",
                "response":{"id":"resp_other","status":"completed"}
            }),
        );
        let processed = guard.process(&completed, &mut ids).unwrap().unwrap();
        assert_eq!(processed.terminal, Some(ResponsesTerminal::Completed));
        assert_eq!(processed.value["response"]["id"], "resp_created");
        assert!(processed.frame.contains("\"id\":\"resp_created\""));
        assert!(!processed.frame.contains("resp_other"));
    }

    #[test]
    fn rejects_whitespace_response_ids() {
        let mut ids = StreamIdTracker::new();
        let mut guard = ResponsesStreamGuard::new();
        let created = event(
            None,
            json!({
                "type":"response.created",
                "response":{"id":"   ","model":"gpt-5"}
            }),
        );
        assert!(guard.process(&created, &mut ids).is_err());

        let mut guard = ResponsesStreamGuard::new();
        guard
            .process(
                &event(
                    None,
                    json!({
                        "type":"response.created",
                        "response":{"id":"resp_created","model":"gpt-5"}
                    }),
                ),
                &mut ids,
            )
            .unwrap();
        let completed = event(
            None,
            json!({
                "type":"response.completed",
                "response":{"id":"   ","status":"completed"}
            }),
        );
        assert!(guard.process(&completed, &mut ids).is_err());
    }

    #[test]
    fn frame_orders_event_id_and_multiline_data() {
        assert_eq!(
            build_sse_frame(Some("e1"), Some("response.created"), "line1\nline2"),
            "event: response.created\nid: e1\ndata: line1\ndata: line2\n\n"
        );
    }
}
