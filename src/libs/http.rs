//! Shared `reqwest` HTTP client used for all upstream calls, plus the
//! `--proxy-env` opt-in gate for honoring proxy environment variables.

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Whether to honor proxy environment variables (HTTP_PROXY/HTTPS_PROXY/...).
/// The TS port only installs an env-based proxy dispatcher when `--proxy-env`
/// is passed (src/lib/proxy.ts initProxyFromEnv); without the flag, requests go
/// direct. reqwest would otherwise read those env vars unconditionally, so we
/// gate it here. Must be set before the first `client()` call.
static PROXY_FROM_ENV: AtomicBool = AtomicBool::new(false);

pub fn set_proxy_from_env(enabled: bool) {
    PROXY_FROM_ENV.store(enabled, Ordering::SeqCst);
}

/// Whether proxy-from-env is enabled. Exposed so the provider forwarding client
/// (which is built separately, with redirects disabled) can mirror the same
/// proxy gating as the shared `client()`.
pub fn proxy_from_env_enabled() -> bool {
    PROXY_FROM_ENV.load(Ordering::SeqCst)
}

/// Shared reqwest client. The TS code uses a global monkey-patched `fetch`
/// (electron-fetch / undici with system CA). Here we use a single reqwest
/// client configured with native roots and no global timeout (streaming).
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        // read_timeout bounds the gap between successive reads, NOT the total
        // request duration. A healthy SSE stream that keeps producing bytes
        // (including slow model "thinking" gaps under 120s) is unaffected, but a
        // connection that wedges open with no further data is killed instead of
        // hanging forever. An overall `.timeout(...)` would wrongly cap long
        // legitimate streams, so we deliberately do not use one here.
        .read_timeout(Duration::from_secs(120))
        .pool_idle_timeout(Duration::from_secs(90));
    if !PROXY_FROM_ENV.load(Ordering::SeqCst) {
        builder = builder.no_proxy();
    }
    builder.build().expect("failed to build reqwest client")
});

pub fn client() -> &'static reqwest::Client {
    &CLIENT
}

/// Send a request, retrying ONCE on a genuine connection failure.
///
/// A `reqwest` error where `is_connect()` is true means the TCP/TLS connection
/// to the upstream never established — the request did not reach the model, so
/// retrying cannot duplicate or double-bill a generation. We deliberately do
/// NOT on read/body errors (the connection was live, a stream may have
/// started) or on HTTP 5xx status (those reached the model, so the caller
/// handles them). One short fixed backoff smooths the sporadic stale-keepalive /
/// transient-connect failures seen under concurrency.
///
/// `endpoint` is a bounded label (messages | chat | responses) used only for the
/// retry counter metric. The builder must be cloneable (our bodies are owned
/// `Vec<u8>`, so `try_clone` always succeeds); if it somehow isn't, we send once.
pub async fn send_with_connect_retry(
    builder: reqwest::RequestBuilder,
    endpoint: &'static str,
) -> reqwest::Result<reqwest::Response> {
    let retry = builder.try_clone();
    let first = builder.send().await;
    match first {
        Err(e) if e.is_connect() && retry.is_some() => {
            tracing::warn!("upstream connect error ({endpoint}); retrying once: {e}");
            metrics::counter!("copilot_upstream_retry_total", "endpoint" => endpoint).increment(1);
            tokio::time::sleep(Duration::from_millis(250)).await;
            retry.unwrap().send().await
        }
        other => other,
    }
}

/// Max bytes buffered from a non-streaming upstream JSON response (16 MiB).
/// Generous for any real Copilot completion (output is bounded by `max_tokens`)
/// while preventing a misbehaving/compromised upstream from driving unbounded
/// memory growth. Streaming responses are NOT buffered and are unaffected.
pub const MAX_UPSTREAM_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Read a non-streaming upstream response body into memory with a hard byte cap,
/// then deserialize it as `T`. Mirrors `response.json::<T>()` but bounds the
/// buffer: `reqwest`'s own `.json()`/`.bytes()` read the entire body regardless
/// of size. Returns `Err(message)` on a transport error, an oversize body, or a
/// JSON parse failure — the caller wraps it into the appropriate `HttpError`.
pub async fn read_json_capped<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, String> {
    use futures_util::StreamExt;

    // Fast-fail on an advertised Content-Length over the cap before reading.
    if let Some(len) = response.content_length() {
        if len as usize > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err(format!(
                "upstream response too large: {len} bytes > {MAX_UPSTREAM_RESPONSE_BYTES} cap"
            ));
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("error reading upstream response: {e}"))?;
        if buf.len() + chunk.len() > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err(format!(
                "upstream response exceeded {MAX_UPSTREAM_RESPONSE_BYTES} byte cap"
            ));
        }
        buf.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&buf).map_err(|e| format!("failed to parse upstream response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_body_requests_are_cloneable_for_retry() {
        // send_with_connect_retry depends on try_clone() succeeding so it can
        // replay the request after a connect failure. A request built with an
        // owned byte body (as all our upstream calls are) must be cloneable;
        // streaming bodies would not be, which is why we only use owned Vec<u8>.
        let req = client()
            .post("http://127.0.0.1:0/v1/messages")
            .body(vec![1u8, 2, 3])
            .build()
            .unwrap();
        let rebuilt = client()
            .post("http://127.0.0.1:0/v1/messages")
            .body(vec![1u8, 2, 3]);
        assert!(
            rebuilt.try_clone().is_some(),
            "owned-body requests must be cloneable so the retry can replay them"
        );
        drop(req);
    }
}
