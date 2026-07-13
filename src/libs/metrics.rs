//! Prometheus metrics exposition.
//!
//! Installs a global `PrometheusRecorder` the first time the handle is
//! requested and exposes it for the `/metrics` route. The recorder is rendered
//! in-process (text exposition), so no networking features of
//! `metrics-exporter-prometheus` are required.

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use once_cell::sync::Lazy;

/// Explicit histogram boundaries (seconds) for every `*_seconds` histogram.
///
/// Without this, `metrics-exporter-prometheus` renders histograms as rolling
/// summaries (fixed per-replica quantiles), which can't be re-aggregated with
/// `histogram_quantile` or charted as a heatmap. These boundaries are tuned for
/// LLM proxy latency (sub-second proxy overhead up to multi-second generation).
const SECONDS_BUCKETS: &[f64] = &[
    0.1, 0.25, 0.5, 1.0, 2.0, 2.5, 5.0, 7.5, 10.0, 15.0, 30.0, 60.0,
];

/// Apply the shared `_seconds` bucket set to a builder (best-effort: a bad
/// bucket list would only fall back to default summary rendering).
fn with_buckets(builder: PrometheusBuilder) -> PrometheusBuilder {
    builder
        .set_buckets_for_metric(Matcher::Suffix("_seconds".to_string()), SECONDS_BUCKETS)
        .unwrap_or_else(|_| PrometheusBuilder::new())
}

/// The process-global Prometheus handle. Installing the recorder can only
/// happen once per process; if another recorder is already installed we fall
/// back to building a detached handle so rendering still works.
static HANDLE: Lazy<PrometheusHandle> = Lazy::new(|| match with_buckets(PrometheusBuilder::new())
    .install_recorder()
{
    Ok(handle) => handle,
    Err(_) => with_buckets(PrometheusBuilder::new())
        .build_recorder()
        .handle(),
});

/// Return the global Prometheus handle, installing the recorder on first use.
pub fn metrics_handle() -> &'static PrometheusHandle {
    &HANDLE
}

/// Render the current metrics in Prometheus text exposition format.
pub fn render() -> String {
    metrics_handle().render()
}

/// Emit the `build_info` gauge (constant 1.0) carrying the crate version, git
/// SHA, and build timestamp as labels — the standard Prometheus pattern for
/// annotating dashboards/alerts with the running build. Forces the recorder to
/// install first so the sample is retained. Call once at startup.
pub fn init_build_info() {
    // Touch the handle so the global recorder is installed before we emit.
    let _ = metrics_handle();
    metrics::gauge!(
        "build_info",
        "version" => env!("CARGO_PKG_VERSION"),
        "git_sha" => env!("GIT_SHA"),
        "build_timestamp" => env!("BUILD_TIMESTAMP"),
    )
    .set(1.0);
}

/// Record an upstream Copilot request's latency under
/// `copilot_upstream_request_seconds`, labelled by a bounded `endpoint`
/// (messages | chat | responses | responses_compact | embeddings | models) and
/// a coarse `status` class. This measures
/// time-to-response-headers (the `send().await`), i.e. upstream TTFB — NOT the
/// time to consume a streaming body. Paired with the proxy_stream_* histograms,
/// it lets you separate "slow to respond" from "long but healthy output".
/// `status` is one of: ok, client_error, server_error, transport_error.
pub fn record_upstream_request(endpoint: &'static str, status: UpstreamStatus, elapsed_secs: f64) {
    metrics::histogram!(
        "copilot_upstream_request_seconds",
        "endpoint" => endpoint,
        "status" => status.as_str(),
    )
    .record(elapsed_secs);
    record_request_context_status(status);
}

/// Provider equivalent of [`record_upstream_request`]. Provider names are
/// intentionally not labels: configured aliases are unbounded. Endpoint and
/// coarse status are fixed-cardinality labels shared with the direct metric.
pub fn record_provider_upstream_request(
    endpoint: &'static str,
    status: UpstreamStatus,
    elapsed_secs: f64,
) {
    metrics::histogram!(
        "provider_upstream_request_seconds",
        "endpoint" => endpoint,
        "status" => status.as_str(),
    )
    .record(elapsed_secs);
    record_request_context_status(status);
}

fn record_request_context_status(status: UpstreamStatus) {
    // Feed the per-request triage summary so `request.completed` can correlate
    // the upstream-status class with flow/transport/TTFT. This runs in-scope
    // (during the upstream `send().await`), so the task-local context is live;
    // last writer wins so a retried request reflects its final status.
    if let Some(ctx) = crate::libs::request_context::request_context_store() {
        ctx.set_upstream_status(status.as_str());
    }
}

/// Coarse outcome class for an upstream request (keeps the metric label set
/// bounded — never the raw status code).
#[derive(Clone, Copy)]
pub enum UpstreamStatus {
    Ok,
    ClientError,
    ServerError,
    TransportError,
}

impl UpstreamStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            UpstreamStatus::Ok => "ok",
            UpstreamStatus::ClientError => "client_error",
            UpstreamStatus::ServerError => "server_error",
            UpstreamStatus::TransportError => "transport_error",
        }
    }

    /// Classify an HTTP status code into the coarse bucket.
    pub fn from_code(code: u16) -> Self {
        match code {
            200..=399 => UpstreamStatus::Ok,
            400..=499 => UpstreamStatus::ClientError,
            _ => UpstreamStatus::ServerError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_histograms_render_with_explicit_buckets() {
        // Recording a `*_seconds` histogram must produce Prometheus `_bucket`
        // lines (an explicit histogram), not summary `quantile` lines — proving
        // SECONDS_BUCKETS is applied so histogram_quantile works downstream.
        //
        // init_build_info() installs the global recorder, mirroring startup
        // (run_server calls it before serving). Metrics emitted before any
        // recorder is installed are dropped, so this must come first.
        init_build_info();
        record_upstream_request("messages", UpstreamStatus::Ok, 3.2);
        let out = render();
        assert!(
            out.contains("copilot_upstream_request_seconds_bucket"),
            "expected explicit histogram buckets, got:\n{out}"
        );
        assert!(
            !out.contains("copilot_upstream_request_seconds{quantile"),
            "histogram should not render as a summary"
        );
        // The chosen boundaries should be present (e.g. the 5s bucket).
        assert!(out.contains("le=\"5\""), "expected the 5s bucket boundary");
        // build_info should also be present.
        assert!(out.contains("build_info"), "expected build_info gauge");
    }
}
