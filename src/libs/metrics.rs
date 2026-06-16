//! Prometheus metrics exposition.
//!
//! Installs a global `PrometheusRecorder` the first time the handle is
//! requested and exposes it for the `/metrics` route. The recorder is rendered
//! in-process (text exposition), so no networking features of
//! `metrics-exporter-prometheus` are required.

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use once_cell::sync::Lazy;

/// The process-global Prometheus handle. Installing the recorder can only
/// happen once per process; if another recorder is already installed we fall
/// back to building a detached handle so rendering still works.
static HANDLE: Lazy<PrometheusHandle> = Lazy::new(|| {
    let builder = PrometheusBuilder::new();
    match builder.install_recorder() {
        Ok(handle) => handle,
        Err(_) => PrometheusBuilder::new().build_recorder().handle(),
    }
});

/// Return the global Prometheus handle, installing the recorder on first use.
pub fn metrics_handle() -> &'static PrometheusHandle {
    &HANDLE
}

/// Render the current metrics in Prometheus text exposition format.
pub fn render() -> String {
    metrics_handle().render()
}
