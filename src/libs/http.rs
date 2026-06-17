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

/// The `endpoint` label values for upstream calls that route through
/// [`send_with_connect_retry`]. Defined as named constants and used at BOTH the
/// call sites and the pre-registration below, so a new/renamed endpoint can't
/// silently reintroduce the "counter series only appears after the first retry"
/// gap. `RETRY_ENDPOINTS` is built from the same constants.
pub mod retry_endpoint {
    pub const MESSAGES: &str = "messages";
    pub const CHAT: &str = "chat";
    pub const RESPONSES: &str = "responses";
    pub const EMBEDDINGS: &str = "embeddings";
    pub const MODELS: &str = "models";
}

/// Every endpoint label that routes through [`send_with_connect_retry`], used to
/// pre-register the retry counter. Built from [`retry_endpoint`] so it stays in
/// lockstep with the call sites.
pub const RETRY_ENDPOINTS: [&str; 5] = [
    retry_endpoint::MESSAGES,
    retry_endpoint::CHAT,
    retry_endpoint::RESPONSES,
    retry_endpoint::EMBEDDINGS,
    retry_endpoint::MODELS,
];

/// Register `copilot_upstream_retry_total{endpoint=...}` at 0 for every known
/// endpoint so the series exists from startup. Without this the counter only
/// appears after the first connect failure, which makes `rate()`/`increase()`
/// and "retries > N" alerts read "no data" instead of 0 — exactly when the first
/// upstream failure occurs. `increment(0)` registers without changing the value.
/// Call once at startup (after the recorder is installed).
pub fn preregister_retry_metrics() {
    for endpoint in RETRY_ENDPOINTS {
        metrics::counter!("copilot_upstream_retry_total", "endpoint" => endpoint).increment(0);
    }
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
/// `endpoint` is a bounded label (messages | chat | responses | embeddings |
/// models) used only for the retry counter metric. The builder must be cloneable (our bodies are owned
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

/// Max accepted *request* body size (32 MiB). The single source of truth for both
/// the router's `DefaultBodyLimit` (which caps the buffered request body) and the
/// zstd middleware's decompressed-output ceiling, so a decompressed body can never
/// exceed the same advertised per-request bound.
pub const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

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

/// Max bytes buffered from a non-2xx upstream *error* body (1 MiB). Error bodies
/// are only used for envelope-sniffing, message-lifting, and client display, so a
/// tighter bound than [`MAX_UPSTREAM_RESPONSE_BYTES`] is fine — truncation can't
/// lose meaningful error text — while still keeping the error path from buffering
/// a multi-GB body a misbehaving upstream might return.
pub const MAX_UPSTREAM_ERROR_BYTES: usize = 1024 * 1024;

/// Append `chunk` to `buf` without exceeding `cap` bytes. Returns `true` if the
/// cap was hit (the chunk was truncated or dropped), signalling the caller to
/// stop reading. Pulled out of [`read_text_capped`] so the truncation boundary
/// is unit-testable without a live response.
fn append_capped(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) -> bool {
    let remaining = cap.saturating_sub(buf.len());
    if remaining == 0 {
        return true;
    }
    let take = remaining.min(chunk.len());
    buf.extend_from_slice(&chunk[..take]);
    take < chunk.len()
}

/// Read an upstream response body into a bounded `String`, truncating at
/// [`MAX_UPSTREAM_ERROR_BYTES`]. Mirrors `response.text()` but caps the buffer so
/// the error path enjoys the same memory protection as [`read_json_capped`].
/// Bytes are decoded with `from_utf8_lossy`; a transport error yields whatever was
/// read so far (error display is best-effort).
pub async fn read_text_capped(response: reqwest::Response) -> String {
    use futures_util::StreamExt;

    let mut buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => break,
        };
        if append_capped(&mut buf, &chunk, MAX_UPSTREAM_ERROR_BYTES) {
            break;
        }
    }

    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_capped_truncates_at_cap() {
        // A chunk that overflows the cap is truncated to exactly the cap and
        // signals the caller to stop.
        let mut buf = Vec::new();
        assert!(!append_capped(&mut buf, b"hello", 10));
        assert_eq!(buf, b"hello");
        // This chunk would overflow the 10-byte cap; only 5 bytes fit.
        assert!(append_capped(&mut buf, b"world!!!", 10));
        assert_eq!(buf, b"helloworld");
        assert_eq!(buf.len(), 10);
        // Once full, further chunks are dropped and keep signalling stop.
        assert!(append_capped(&mut buf, b"more", 10));
        assert_eq!(buf.len(), 10);
    }

    #[test]
    fn append_capped_keeps_reading_under_cap() {
        // A chunk that fits leaves room and does not signal stop.
        let mut buf = Vec::new();
        assert!(!append_capped(&mut buf, b"abc", 1024));
        assert!(!append_capped(&mut buf, b"def", 1024));
        assert_eq!(buf, b"abcdef");
    }

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

    #[test]
    fn retry_counter_is_preregistered_at_zero() {
        // After startup pre-registration, the retry counter series must exist at
        // 0 for every known endpoint so dashboards/alerts read 0, not "no data".
        crate::libs::metrics::init_build_info(); // installs the recorder
        preregister_retry_metrics();
        let out = crate::libs::metrics::render();
        for endpoint in RETRY_ENDPOINTS {
            assert!(
                out.contains(&format!(
                    "copilot_upstream_retry_total{{endpoint=\"{endpoint}\"}} 0"
                )),
                "expected pre-registered retry counter at 0 for endpoint={endpoint}, got:\n{out}"
            );
        }
    }
}
