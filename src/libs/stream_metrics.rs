//! Shared streaming-latency metric recorder used by every SSE response path.
//!
//! Both the Anthropic `/v1/messages` translation flows and the native
//! OpenAI-style `/v1/chat/completions` + `/v1/responses` forwarders emit the
//! same two histograms so the latency dashboards cover every streaming route.

use metrics::{gauge, histogram};

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
///   labelled `outcome` = ok | error.
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
pub struct StreamTimer {
    flow: &'static str,
    transport: &'static str,
    start: std::time::Instant,
    ttft_recorded: bool,
    errored: bool,
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
        }
    }

    /// Call as each genuine **content** frame is about to be yielded; records
    /// TTFT on the first such frame. Do NOT call for pings or error frames.
    pub fn on_content_frame(&mut self) {
        if !self.ttft_recorded {
            self.ttft_recorded = true;
            histogram!(
                "proxy_stream_ttft_seconds",
                "flow" => self.flow,
                "transport" => self.transport,
            )
            .record(self.start.elapsed().as_secs_f64());
        }
    }

    /// Mark that the stream is ending via the upstream-error path.
    pub fn mark_error(&mut self) {
        self.errored = true;
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
        let outcome = if self.errored { "error" } else { "ok" };
        histogram!(
            "proxy_stream_complete_seconds",
            "flow" => self.flow,
            "transport" => self.transport,
            "outcome" => outcome,
        )
        .record(self.start.elapsed().as_secs_f64());
    }
}
