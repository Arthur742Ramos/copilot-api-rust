//! Shared streaming-latency metric recorder used by every SSE response path.
//!
//! Both the Anthropic `/v1/messages` translation flows and the native
//! OpenAI-style `/v1/chat/completions` + `/v1/responses` forwarders emit the
//! same two histograms so the latency dashboards cover every streaming route.

use metrics::{gauge, histogram};

use crate::libs::request_context::{emit_request_completed, RequestContext};

/// How the stream's bytes were produced, so the two measurement methods are
/// distinguishable on the same metric. `translated` = the messages flows that
/// translate per-event (precise TTFT); `native` = the raw byte forwarders whose
/// TTFT is a coarser first-chunk approximation.
pub mod transport {
    pub const TRANSLATED: &str = "translated";
    pub const NATIVE: &str = "native";
}

/// Records streaming-latency metrics for one SSE response:
/// - `proxy_stream_ttft_seconds` — time to the first **content** frame yielded
///   to the client (recorded once). Keep-alive pings and terminal error frames
///   do NOT count, so this reflects time-to-first-token, not first-byte.
/// - `proxy_stream_complete_seconds` — total stream duration, recorded on drop
///   so it fires on every exit (clean end, upstream error, or client disconnect),
///   labelled `outcome` = ok | error | cancelled.
///
/// Both histograms carry a bounded `flow` (chat_completions | responses |
/// messages) and `transport` (translated | native) label.
///
/// While alive, the timer also holds the `proxy_streams_active` gauge up by one
/// (same `flow`/`transport` labels), decrementing in `Drop`. The gauge is the
/// count of streams currently held open: a stream wedged on a silent upstream
/// keeps its timer alive, so it stays counted here even though its completion
/// histogram has not yet recorded (that only fires once the stream actually
/// ends). A gauge that rises without falling is therefore the signal that
/// streams are stuck or leaking — which the drop-recorded completion histogram
/// cannot surface on its own.
///
pub struct StreamTimer {
    flow: &'static str,
    transport: &'static str,
    start: std::time::Instant,
    ttft_recorded: bool,
    errored: bool,
    /// Set by [`mark_finished`] when the stream reaches its terminal frame.
    /// Drives the "ok" vs "cancelled" outcome on drop.
    finished: bool,
    /// When attached, the timer also feeds the shared per-request summary
    /// (TTFT/transport/outcome) and emits the single `request.completed` event
    /// for this stream on drop. Captured eagerly by the caller (while the
    /// task-local context is still in scope) so it survives into the
    /// later-polled stream body. `None` leaves the timer metrics-only.
    request_context: Option<RequestContext>,
}

impl StreamTimer {
    pub fn new(flow: &'static str, transport: &'static str) -> Self {
        gauge!(
            "proxy_streams_active",
            "flow" => flow,
            "transport" => transport,
        )
        .increment(1.0);
        Self {
            flow,
            transport,
            start: std::time::Instant::now(),
            ttft_recorded: false,
            errored: false,
            finished: false,
            request_context: None,
        }
    }

    /// Attach the captured request context so this stream contributes to, and
    /// emits, the `request.completed` summary line. Stamps the flow/transport
    /// onto the shared summary immediately so the headline is populated even if
    /// no content frame ever arrives (e.g. a stream that errors at the head).
    pub fn with_request_context(mut self, ctx: Option<RequestContext>) -> Self {
        if let Some(ctx) = &ctx {
            ctx.set_flow_transport_streaming(self.flow, self.transport);
        }
        self.request_context = ctx;
        self
    }

    /// Call as each genuine **content** frame is about to be yielded; records
    /// TTFT on the first such frame. Do NOT call for pings or error frames.
    pub fn on_content_frame(&mut self) {
        if !self.ttft_recorded {
            self.ttft_recorded = true;
            let ttft = self.start.elapsed();
            histogram!(
                "proxy_stream_ttft_seconds",
                "flow" => self.flow,
                "transport" => self.transport,
            )
            .record(ttft.as_secs_f64());
            if let Some(ctx) = &self.request_context {
                ctx.set_ttft_ms(ttft.as_millis() as u64);
            }
        }
    }

    /// Mark that the stream is ending via the upstream-error path.
    pub fn mark_error(&mut self) {
        self.errored = true;
    }

    /// Call at the `[DONE]` / terminal frame to mark the stream as cleanly
    /// completed. If this is never called the stream is treated as `cancelled`
    /// (client disconnected before the final frame was flushed).
    pub fn mark_finished(&mut self) {
        self.finished = true;
    }
}

impl Drop for StreamTimer {
    fn drop(&mut self) {
        gauge!(
            "proxy_streams_active",
            "flow" => self.flow,
            "transport" => self.transport,
        )
        .decrement(1.0);
        let outcome = if self.errored {
            "error"
        } else if self.finished {
            "ok"
        } else {
            "cancelled"
        };
        histogram!(
            "proxy_stream_complete_seconds",
            "flow" => self.flow,
            "transport" => self.transport,
            "outcome" => outcome,
        )
        .record(self.start.elapsed().as_secs_f64());

        // Streaming responses return their HEAD to the middleware before TTFT /
        // outcome are known, so the single `request.completed` line for a stream
        // is emitted here — at the terminal step — rather than in the middleware.
        if let Some(ctx) = self.request_context.take() {
            ctx.set_outcome_if_unset(outcome);
            emit_request_completed(&ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::request_context::{RequestContext, RequestSummary};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::prelude::*;

    #[derive(Default)]
    struct Captured {
        events: Vec<CapturedEvent>,
    }

    struct CapturedEvent {
        message: Option<String>,
        fields: HashMap<String, String>,
    }

    struct CaptureLayer(Arc<Mutex<Captured>>);

    #[derive(Default)]
    struct FieldVisitor {
        message: Option<String>,
        fields: HashMap<String, String>,
    }

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let rendered = format!("{value:?}");
            if field.name() == "message" {
                self.message = Some(rendered);
            } else {
                self.fields.insert(field.name().to_string(), rendered);
            }
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            // Route the message identically to `record_debug` so the captured
            // `message` lands in the same slot regardless of which visitor method
            // tracing chooses for the event's format string. Otherwise a string
            // message would silently land in `fields["message"]`, leaving
            // `self.message` empty and letting a real emission slip past a
            // `message`-only assertion (a false negative).
            if field.name() == "message" {
                self.message = Some(value.to_string());
            } else {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .events
                .push(CapturedEvent {
                    message: visitor.message,
                    fields: visitor.fields,
                });
        }
    }

    fn test_context() -> RequestContext {
        RequestContext {
            trace_id: "trace-stream".to_string(),
            start_time: crate::libs::request_context::now_millis(),
            user_agent: "test".to_string(),
            session_affinity: Some("sess-7".to_string()),
            parent_session_id: None,
            api_key_label: std::sync::Arc::new(std::sync::OnceLock::new()),
            api_key_owner_id: std::sync::Arc::new(std::sync::OnceLock::new()),
            summary: Arc::new(Mutex::new(RequestSummary::default())),
        }
    }

    /// A streaming request must emit EXACTLY ONE `request.completed` event, with
    /// the flow/transport/TTFT/upstream-status headline populated — driven purely
    /// through the StreamTimer lifecycle, no live server required.
    #[test]
    fn streaming_request_emits_exactly_one_request_completed() {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(Arc::clone(&captured)));

        tracing::subscriber::with_default(subscriber, || {
            let ctx = test_context();
            // Upstream responded ok (recorded in-scope before the stream body).
            ctx.set_upstream_status("ok");

            let mut timer = StreamTimer::new("messages", transport::TRANSLATED)
                .with_request_context(Some(ctx.clone()));
            // First content frame -> TTFT recorded into the shared summary.
            timer.on_content_frame();
            // Mark stream as cleanly finished so outcome is "ok".
            timer.mark_finished();
            // Terminal step: dropping the timer emits the single summary line.
            drop(timer);

            // Idempotency: the shared summary's `emitted` flag is now set.
            assert!(ctx.summary.lock().unwrap().emitted);
        });

        let captured = captured.lock().unwrap_or_else(|p| p.into_inner());
        let completed: Vec<&CapturedEvent> = captured
            .events
            .iter()
            .filter(|e| {
                e.message.as_deref() == Some("request.completed")
                    || e.fields.get("event").map(String::as_str) == Some("request.completed")
            })
            .collect();

        assert_eq!(
            completed.len(),
            1,
            "expected exactly one request.completed event, got {}",
            completed.len()
        );
        let e = completed[0];
        assert_eq!(
            e.fields.get("event").map(String::as_str),
            Some("request.completed")
        );
        assert_eq!(e.fields.get("flow").map(String::as_str), Some("messages"));
        assert_eq!(
            e.fields.get("transport").map(String::as_str),
            Some("translated")
        );
        assert_eq!(e.fields.get("streaming").map(String::as_str), Some("true"));
        assert_eq!(
            e.fields.get("upstream_status").map(String::as_str),
            Some("ok")
        );
        assert_eq!(e.fields.get("outcome").map(String::as_str), Some("ok"));
        assert!(
            e.fields.contains_key("ttft_ms"),
            "ttft_ms must be present on a streaming completion"
        );
        assert_eq!(
            e.fields.get("trace_id").map(String::as_str),
            Some("trace-stream")
        );
    }

    /// A timer with no attached context stays metrics-only and emits no event.
    #[test]
    fn timer_without_context_emits_no_event() {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(Arc::clone(&captured)));
        tracing::subscriber::with_default(subscriber, || {
            let mut timer = StreamTimer::new("messages", transport::TRANSLATED);
            timer.on_content_frame();
            drop(timer);
        });
        let captured = captured.lock().unwrap_or_else(|p| p.into_inner());
        // Assert on the structured `event` field (always recorded via
        // `record_str`), not just the format-string `message`, so a stray
        // emission can't slip past on account of how tracing renders the message.
        assert!(
            captured.events.iter().all(|e| {
                e.fields.get("event").map(String::as_str) != Some("request.completed")
                    && e.message.as_deref() != Some("request.completed")
            }),
            "a context-less timer must emit no request.completed event"
        );
    }

    /// A stream dropped without `mark_finished()` records outcome = "cancelled"
    /// — models a client disconnect before the `[DONE]` frame arrived.
    #[test]
    fn stream_cancelled_emits_cancelled_outcome() {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(Arc::clone(&captured)));

        tracing::subscriber::with_default(subscriber, || {
            let ctx = test_context();
            ctx.set_upstream_status("ok");

            let mut timer =
                StreamTimer::new("messages", transport::TRANSLATED).with_request_context(Some(ctx));
            // Deliver some content frames...
            timer.on_content_frame();
            // ...but never call mark_finished() — simulating client disconnect.
            drop(timer);
        });

        let captured = captured.lock().unwrap_or_else(|p| p.into_inner());
        let completed: Vec<&CapturedEvent> = captured
            .events
            .iter()
            .filter(|e| {
                e.message.as_deref() == Some("request.completed")
                    || e.fields.get("event").map(String::as_str) == Some("request.completed")
            })
            .collect();

        assert_eq!(completed.len(), 1, "expected exactly one request.completed");
        assert_eq!(
            completed[0].fields.get("outcome").map(String::as_str),
            Some("cancelled"),
            "outcome must be 'cancelled' when mark_finished was never called"
        );
    }
}
