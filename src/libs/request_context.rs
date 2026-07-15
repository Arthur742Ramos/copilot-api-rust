use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Mirrors src/lib/request-context.ts. The TS code uses Node's AsyncLocalStorage
/// to carry per-request metadata through the async call tree. The Rust analogue
/// is a tokio task-local; `request_context_store()` reads the current value.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub trace_id: String,
    pub start_time: u128,
    pub user_agent: String,
    pub session_affinity: Option<String>,
    pub parent_session_id: Option<String>,
    /// Interior-mutable attribution token for the matched API key (its label, or
    /// a stable fingerprint when unlabeled — never the raw key). The context
    /// itself is installed immutably by the trace layer before auth runs, so the
    /// auth layer fills this `OnceLock` (shared across clones via the `Arc`) once
    /// the key is matched. Read later by the token-usage recorder.
    pub api_key_label: Arc<OnceLock<String>>,
    /// Stable fingerprint of the matched raw API key. Unlike the optional
    /// human-readable label, this is safe to use as an authorization principal
    /// for owner-scoped local resources.
    pub api_key_owner_id: Arc<OnceLock<String>>,
    /// Shared, mutable per-request triage summary. Cloning a `RequestContext`
    /// (e.g. `request_context_store()`) shares the SAME `Arc`, so every layer —
    /// the api_flows flow selection, the `StreamTimer`, and the
    /// `record_upstream_request` sites — writes into one summary that is emitted
    /// exactly once as the `request.completed` event. Added last so existing
    /// field-by-field construction stays a single additive line.
    pub summary: Arc<Mutex<RequestSummary>>,
}

/// Per-request triage data that today only exists as aggregate Prometheus
/// histograms. Collected across the request's layers and flushed once into the
/// `request.completed` log line so per-request triage doesn't require
/// `RUST_LOG=debug` + replay.
#[derive(Debug, Clone, Default)]
pub struct RequestSummary {
    /// Concrete upstream model id (e.g. `gpt-4o`), once a flow has been selected.
    pub model: Option<String>,
    /// Dispatch flow: `chat_completions` | `responses` | `messages`.
    pub flow: Option<&'static str>,
    /// Transport: `translated` (per-event translation) | `native` (raw forward).
    pub transport: Option<&'static str>,
    /// Whether the response was an SSE stream.
    pub streaming: bool,
    /// Coarse upstream status class: `ok` | `client_error` | `server_error` |
    /// `transport_error` (last upstream call wins, so retries reflect the final).
    pub upstream_status: Option<&'static str>,
    /// Time-to-first-token in milliseconds (streaming responses only).
    pub ttft_ms: Option<u64>,
    /// Terminal outcome: `ok` | `error`.
    pub outcome: Option<&'static str>,
    /// Guards against double-emission: the first emitter wins.
    pub emitted: bool,
}

impl RequestContext {
    /// Construct a context with an empty (not-yet-filled) attribution cell and a
    /// fresh, empty triage summary.
    pub fn new(
        trace_id: String,
        start_time: u128,
        user_agent: String,
        session_affinity: Option<String>,
        parent_session_id: Option<String>,
    ) -> Self {
        RequestContext {
            trace_id,
            start_time,
            user_agent,
            session_affinity,
            parent_session_id,
            api_key_label: Arc::new(OnceLock::new()),
            api_key_owner_id: Arc::new(OnceLock::new()),
            summary: Arc::new(Mutex::new(RequestSummary::default())),
        }
    }

    /// Lock the summary, recovering from a poisoned mutex (a panic while another
    /// layer held the lock must not wedge the whole request).
    fn summary_lock(&self) -> std::sync::MutexGuard<'_, RequestSummary> {
        self.summary.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record the selected dispatch flow + transport + concrete model. Called
    /// once per request from the api_flows handlers (in-scope, before streaming).
    pub fn set_flow(&self, model: &str, flow: &'static str, transport: &'static str) {
        let mut s = self.summary_lock();
        s.model = Some(model.to_string());
        s.flow = Some(flow);
        s.transport = Some(transport);
    }

    /// Mark the response streaming and stamp flow/transport without touching the
    /// model. Used by the `StreamTimer` (which knows flow/transport but not the
    /// model) so native streams without a flow handler still get a headline.
    pub fn set_flow_transport_streaming(&self, flow: &'static str, transport: &'static str) {
        let mut s = self.summary_lock();
        s.streaming = true;
        if s.flow.is_none() {
            s.flow = Some(flow);
        }
        if s.transport.is_none() {
            s.transport = Some(transport);
        }
    }

    /// Record the selected flow/transport/model for a NON-streaming response
    /// without touching the `streaming` flag. Used by native (non-translated)
    /// handlers so non-streaming native requests still record a headline and
    /// emit exactly one `request.completed`. First flow writer wins so a later,
    /// less-specific label can't clobber an api_flows selection.
    pub fn set_flow_transport_model_non_streaming(
        &self,
        model: &str,
        flow: &'static str,
        transport: &'static str,
    ) {
        let mut s = self.summary_lock();
        if s.flow.is_none() {
            s.flow = Some(flow);
        }
        if s.transport.is_none() {
            s.transport = Some(transport);
        }
        if s.model.is_none() {
            s.model = Some(model.to_string());
        }
    }

    /// Record time-to-first-token (first content frame) unless already recorded.
    pub fn set_ttft_ms(&self, ttft_ms: u64) {
        let mut s = self.summary_lock();
        if s.ttft_ms.is_none() {
            s.ttft_ms = Some(ttft_ms);
        }
    }

    /// Record the coarse upstream-status class. Last writer wins so that a
    /// retried request reflects the status that actually produced the response.
    pub fn set_upstream_status(&self, status: &'static str) {
        self.summary_lock().upstream_status = Some(status);
    }

    /// Set the terminal outcome unless one was already recorded.
    pub fn set_outcome_if_unset(&self, outcome: &'static str) {
        let mut s = self.summary_lock();
        if s.outcome.is_none() {
            s.outcome = Some(outcome);
        }
    }
}

const TRACE_ID_MAX_LENGTH: usize = 64;

tokio::task_local! {
    static REQUEST_CONTEXT: RequestContext;
}

/// Run `fut` with `context` installed as the current request context.
pub async fn run_with_context<F, T>(context: RequestContext, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    REQUEST_CONTEXT.scope(context, fut).await
}

/// Equivalent of `requestContext.getStore()` — returns a clone of the current
/// context if one is installed for this task.
pub fn request_context_store() -> Option<RequestContext> {
    REQUEST_CONTEXT.try_with(|ctx| ctx.clone()).ok()
}

/// Fill the current context's API-key attribution cell (label or fingerprint —
/// never the raw key). Called by the auth layer, which runs inside the trace
/// layer's task-local scope. First write wins; later writes are ignored.
pub fn set_request_api_key_label(label: String) {
    let _ = REQUEST_CONTEXT.try_with(|ctx| {
        let _ = ctx.api_key_label.set(label);
    });
}

pub fn set_request_api_key_owner_id(owner_id: String) {
    let _ = REQUEST_CONTEXT.try_with(|ctx| {
        let _ = ctx.api_key_owner_id.set(owner_id);
    });
}

/// Read the API-key attribution token filled by the auth layer for the current
/// request, if any. Returns `None` when no context is installed or no key matched
/// (e.g. unauthenticated requests when no keys are configured).
pub fn request_api_key_label() -> Option<String> {
    REQUEST_CONTEXT
        .try_with(|ctx| ctx.api_key_label.get().cloned())
        .ok()
        .flatten()
}

pub fn request_api_key_owner_id() -> Option<String> {
    REQUEST_CONTEXT
        .try_with(|ctx| ctx.api_key_owner_id.get().cloned())
        .ok()
        .flatten()
}

pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn to_base36(mut n: u128) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(ALPHABET[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

pub fn generate_trace_id() -> String {
    let timestamp = to_base36(now_millis());
    // Mirror Math.random().toString(36).slice(2, 8): six base36 chars.
    let random: u64 = rand::random();
    let mut random_part = to_base36(random as u128);
    random_part.truncate(6);
    while random_part.len() < 6 {
        random_part.push('0');
    }
    format!("{timestamp}-{random_part}")
}

/// `\w[\w.-]*` — first char is a word char, rest are word/`.`/`-`.
fn is_valid_trace_id(candidate: &str) -> bool {
    let mut chars = candidate.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

pub fn resolve_trace_id(trace_id: Option<&str>) -> String {
    let candidate = trace_id.map(|s| s.trim()).unwrap_or("");
    if candidate.is_empty()
        || candidate.len() > TRACE_ID_MAX_LENGTH
        || !is_valid_trace_id(candidate)
    {
        return generate_trace_id();
    }
    candidate.to_string()
}

// ---------------------------------------------------------------------------
// request.completed summary event
// ---------------------------------------------------------------------------

/// The assembled, flat field set of one `request.completed` event. Factored out
/// of the emit path so the assembly is unit-testable without a live server: the
/// pure [`RequestCompletedFields::assemble`] takes an explicit `now_ms`, and
/// [`RequestCompletedFields::emit`] does the (side-effecting) tracing call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestCompletedFields {
    pub trace_id: String,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub model: Option<String>,
    pub flow: Option<&'static str>,
    pub transport: Option<&'static str>,
    pub streaming: bool,
    pub upstream_status: Option<&'static str>,
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    pub outcome: &'static str,
}

impl RequestCompletedFields {
    /// Build the flat field set from the request context + collected summary.
    /// Pure given `now_ms` (millis since the epoch) so tests can pin the clock.
    pub fn assemble(ctx: &RequestContext, summary: &RequestSummary, now_ms: u128) -> Self {
        let total_ms = now_ms.saturating_sub(ctx.start_time) as u64;
        Self {
            trace_id: ctx.trace_id.clone(),
            session_id: ctx.session_affinity.clone(),
            parent_session_id: ctx.parent_session_id.clone(),
            model: summary.model.clone(),
            flow: summary.flow,
            transport: summary.transport,
            streaming: summary.streaming,
            upstream_status: summary.upstream_status,
            ttft_ms: summary.ttft_ms,
            total_ms,
            outcome: summary.outcome.unwrap_or("ok"),
        }
    }

    /// Emit the single structured `request.completed` tracing event. Greppable
    /// in both the human and `COPILOT_API_LOG_FORMAT=json` formatters: the
    /// message and the `event` field are both the literal `request.completed`.
    pub fn emit(&self) {
        tracing::info!(
            target: "request_completed",
            event = "request.completed",
            trace_id = %self.trace_id,
            session_id = self.session_id.as_deref(),
            parent_session_id = self.parent_session_id.as_deref(),
            model = self.model.as_deref(),
            flow = self.flow,
            transport = self.transport,
            streaming = self.streaming,
            upstream_status = self.upstream_status,
            ttft_ms = self.ttft_ms,
            total_ms = self.total_ms,
            outcome = self.outcome,
            "request.completed"
        );
    }
}

/// Emit the `request.completed` event for `ctx` exactly once. The `emitted`
/// flag in the shared summary makes this idempotent across the two call sites
/// (the middleware for non-streaming responses, the `StreamTimer` drop for
/// streams), so a misordered double-call can never double-log.
pub fn emit_request_completed(ctx: &RequestContext) {
    let fields = {
        let mut s = ctx.summary_lock();
        if s.emitted {
            return;
        }
        s.emitted = true;
        RequestCompletedFields::assemble(ctx, &s, now_millis())
    };
    fields.emit();
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    fn ctx_with_summary(summary: RequestSummary) -> RequestContext {
        RequestContext {
            trace_id: "trace-abc".to_string(),
            start_time: 1_000,
            user_agent: "test-agent".to_string(),
            session_affinity: Some("session-1".to_string()),
            parent_session_id: Some("parent-9".to_string()),
            api_key_label: Arc::new(OnceLock::new()),
            api_key_owner_id: Arc::new(OnceLock::new()),
            summary: Arc::new(Mutex::new(summary)),
        }
    }

    #[test]
    fn assemble_maps_every_field() {
        let summary = RequestSummary {
            model: Some("gpt-4o".to_string()),
            flow: Some("chat_completions"),
            transport: Some("translated"),
            streaming: true,
            upstream_status: Some("ok"),
            ttft_ms: Some(42),
            outcome: Some("ok"),
            emitted: false,
        };
        let ctx = ctx_with_summary(summary.clone());
        let fields = RequestCompletedFields::assemble(&ctx, &summary, 1_350);

        assert_eq!(
            fields,
            RequestCompletedFields {
                trace_id: "trace-abc".to_string(),
                session_id: Some("session-1".to_string()),
                parent_session_id: Some("parent-9".to_string()),
                model: Some("gpt-4o".to_string()),
                flow: Some("chat_completions"),
                transport: Some("translated"),
                streaming: true,
                upstream_status: Some("ok"),
                ttft_ms: Some(42),
                total_ms: 350,
                outcome: "ok",
            }
        );
    }

    #[test]
    fn outcome_defaults_to_ok_and_clock_never_underflows() {
        let summary = RequestSummary::default();
        let ctx = ctx_with_summary(summary.clone());
        // now_ms earlier than start_time must not panic/underflow.
        let fields = RequestCompletedFields::assemble(&ctx, &summary, 0);
        assert_eq!(fields.total_ms, 0);
        assert_eq!(fields.outcome, "ok");
    }

    #[test]
    fn emit_request_completed_is_idempotent() {
        let ctx = ctx_with_summary(RequestSummary {
            flow: Some("messages"),
            ..RequestSummary::default()
        });
        emit_request_completed(&ctx);
        assert!(ctx.summary_lock().emitted);
        // Second call is a no-op (flag already set); must not panic.
        emit_request_completed(&ctx);
        assert!(ctx.summary_lock().emitted);
    }

    #[test]
    fn non_streaming_helper_records_headline_without_marking_streaming() {
        // Native non-streaming handlers stamp flow/model/transport here so the
        // trace middleware's `has_flow` guard emits a single request.completed.
        let ctx = ctx_with_summary(RequestSummary::default());
        ctx.set_flow_transport_model_non_streaming("gpt-4o", "chat_completions", "native");
        {
            let s = ctx.summary_lock();
            assert_eq!(s.flow, Some("chat_completions"));
            assert_eq!(s.transport, Some("native"));
            assert_eq!(s.model.as_deref(), Some("gpt-4o"));
            assert!(!s.streaming, "non-streaming path must not set streaming");
        }
        // First flow writer wins: an api_flows selection is not clobbered.
        ctx.set_flow_transport_model_non_streaming("other", "responses", "native");
        let s = ctx.summary_lock();
        assert_eq!(s.flow, Some("chat_completions"));
        assert_eq!(s.model.as_deref(), Some("gpt-4o"));
    }
}
