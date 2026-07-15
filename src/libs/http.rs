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

/// Default upstream read-timeout (seconds): the maximum gap between successive
/// reads before a stalled-open connection is killed. Bounds a wedged upstream
/// without capping a healthy long stream.
pub const DEFAULT_UPSTREAM_READ_TIMEOUT_SECS: u64 = 120;

/// Connection-establishment deadline shared by HTTP and WebSocket transports.
pub const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound retained idle sockets for any one upstream host. Active streaming
/// connections are unaffected; this only prevents burst-created keep-alives from
/// consuming desktop file-descriptor headroom after the burst drains.
pub const UPSTREAM_POOL_MAX_IDLE_PER_HOST: usize = 8;

/// The upstream read-timeout, overridable via `COPILOT_API_UPSTREAM_READ_TIMEOUT_SECS`.
///
/// `read_timeout` bounds the gap between successive reads, NOT the total request
/// duration, so it never caps a healthy long SSE stream that keeps producing
/// bytes — but a very long model "thinking" gap that exceeds it is killed as if
/// the connection had wedged. Operators seeing spurious ~120s stall failures on
/// legitimately slow generations can raise this; a value of `0` disables the
/// read timeout entirely (a wedged connection then relies on the pool idle
/// timeout instead). Read once when each client is first built. Shared by both
/// [`client`] and the provider-forwarding client so they stay consistent.
pub fn upstream_read_timeout() -> Option<Duration> {
    let secs = std::env::var("COPILOT_API_UPSTREAM_READ_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_UPSTREAM_READ_TIMEOUT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Shared reqwest client. The TS code uses a global monkey-patched `fetch`
/// (electron-fetch / undici with system CA). Here we use a single reqwest
/// client configured with native roots and no global timeout (streaming).
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        // read_timeout bounds the gap between successive reads, NOT the total
        // request duration. A healthy SSE stream that keeps producing bytes
        // (including slow model "thinking" gaps under the timeout) is unaffected,
        // but a connection that wedges open with no further data is killed
        // instead of hanging forever. An overall `.timeout(...)` would wrongly
        // cap long legitimate streams, so we deliberately do not use one here.
        // The window is configurable via COPILOT_API_UPSTREAM_READ_TIMEOUT_SECS
        // (default 120; 0 disables it).
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(UPSTREAM_POOL_MAX_IDLE_PER_HOST);
    if let Some(read_timeout) = upstream_read_timeout() {
        builder = builder.read_timeout(read_timeout);
    }
    if !PROXY_FROM_ENV.load(Ordering::SeqCst) {
        builder = builder.no_proxy();
    }
    builder.build().expect("failed to build reqwest client")
});

pub fn client() -> &'static reqwest::Client {
    &CLIENT
}

/// Serialize a JSON request into a reference-counted body.
///
/// Upstream retry and inline-auth-replay closures must clone the body before
/// each send. `Bytes::clone` is O(1), unlike cloning the serialized `Vec<u8>`,
/// which copied the complete conversation even on the first attempt.
pub fn serialize_json_body<T: serde::Serialize + ?Sized>(
    value: &T,
) -> Result<bytes::Bytes, serde_json::Error> {
    serde_json::to_vec(value).map(bytes::Bytes::from)
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
    pub const CODEX: &str = "codex";
}

/// Every endpoint label that routes through [`send_with_connect_retry`], used to
/// pre-register the retry counter. Built from [`retry_endpoint`] so it stays in
/// lockstep with the call sites.
pub const RETRY_ENDPOINTS: [&str; 6] = [
    retry_endpoint::MESSAGES,
    retry_endpoint::CHAT,
    retry_endpoint::RESPONSES,
    retry_endpoint::EMBEDDINGS,
    retry_endpoint::MODELS,
    retry_endpoint::CODEX,
];

/// The `reason` label values for `copilot_upstream_retry_total`. `connect` is a
/// genuine TCP/TLS connect failure (the request never reached the model);
/// `status` is a retryable upstream HTTP status ({429, 502, 503, 504}) observed
/// before any body streamed. Used at both the increment sites and the
/// pre-registration so the two series always exist together.
const RETRY_REASONS: [&str; 2] = [RETRY_REASON_CONNECT, RETRY_REASON_STATUS];

const RETRY_REASON_CONNECT: &str = "connect";
const RETRY_REASON_STATUS: &str = "status";

/// Register `copilot_upstream_retry_total{endpoint=...,reason=...}` at 0 for
/// every known endpoint/reason pair so the series exist from startup. Without
/// this the counter only appears after the first retry, which makes
/// `rate()`/`increase()` and "retries > N" alerts read "no data" instead of 0 —
/// exactly when the first upstream failure occurs. `increment(0)` registers
/// without changing the value. Call once at startup (after the recorder is
/// installed).
pub fn preregister_retry_metrics() {
    for endpoint in RETRY_ENDPOINTS {
        for reason in RETRY_REASONS {
            metrics::counter!("copilot_upstream_retry_total", "endpoint" => endpoint, "reason" => reason)
                .increment(0);
        }
    }
    // The WebSocket path falls back to HTTP only before an application frame is
    // sent. Keep this transport-resilience series visible at zero too.
    for provider in ["copilot", "codex"] {
        metrics::counter!(
            "copilot_responses_websocket_fallback_total",
            "provider" => provider
        )
        .increment(0);
    }
    metrics::counter!(
        "copilot_responses_websocket_attempt_total",
        "provider" => "codex"
    )
    .increment(0);
    metrics::counter!(
        "copilot_responses_websocket_stream_error_total",
        "provider" => "codex"
    )
    .increment(0);
    metrics::counter!(
        "copilot_responses_websocket_cancel_total",
        "provider" => "codex"
    )
    .increment(0);
    for outcome in ["completed", "failed", "incomplete", "error", "unknown"] {
        metrics::counter!(
            "copilot_responses_websocket_terminal_total",
            "provider" => "codex",
            "outcome" => outcome
        )
        .increment(0);
    }
}

/// Send a request, retrying ONCE on a genuine connection failure.
///
/// Superseded by [`send_with_retry`] (which also retries on transient upstream
/// statuses) for the live call sites, but retained as the minimal connect-only
/// helper for callers that must never retry on a status.
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
/// models | codex) used only for the retry counter metric. The builder must be
/// cloneable (our bodies are owned `Bytes`, so `try_clone` always succeeds); if
/// it somehow isn't, we send once.
pub async fn send_with_connect_retry(
    builder: reqwest::RequestBuilder,
    endpoint: &'static str,
) -> reqwest::Result<reqwest::Response> {
    let retry = builder.try_clone();
    let first = builder.send().await;
    match first {
        Err(e) if e.is_connect() && retry.is_some() => {
            tracing::warn!("upstream connect error ({endpoint}); retrying once: {e}");
            metrics::counter!("copilot_upstream_retry_total", "endpoint" => endpoint, "reason" => RETRY_REASON_CONNECT)
                .increment(1);
            tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_BASE_MS)).await;
            retry.unwrap().send().await
        }
        other => other,
    }
}

/// Default number of *retries* (extra attempts beyond the first) for
/// [`send_with_retry`] — 1, i.e. ~2 attempts total. Operators can raise it via
/// `COPILOT_API_UPSTREAM_MAX_RETRIES`; the value is clamped to
/// [`MAX_UPSTREAM_MAX_RETRIES`] so a stray large env value can't turn a single
/// mid-conversation request into a long stall storm.
pub const DEFAULT_UPSTREAM_MAX_RETRIES: u32 = 1;

/// Hard ceiling on the configurable retry count. Bounds total added latency to a
/// few short backoffs even if `COPILOT_API_UPSTREAM_MAX_RETRIES` is set absurdly
/// high.
const MAX_UPSTREAM_MAX_RETRIES: u32 = 5;

/// Base backoff before the first retry of a retryable upstream response (ms).
/// Deliberately small (~250ms) and distinct from token.rs's 15s token-refresh
/// cadence: a mid-conversation retry must feel near-instant, not introduce a
/// multi-second stall the client perceives as a hang.
const RETRY_BACKOFF_BASE_MS: u64 = 250;

/// Maximum random jitter added on top of the exponential base (ms), to
/// de-correlate retries from many concurrent requests all bouncing off the same
/// briefly-overloaded upstream (thundering-herd avoidance).
const RETRY_BACKOFF_JITTER_MS: u64 = 250;

/// Sane cap (secs) on a honored `Retry-After` delay. Overloaded upstreams can
/// advertise large values; we don't want to park a live client request for
/// minutes, so we clamp to something a user will still wait through.
const MAX_RETRY_AFTER_SECS: u64 = 5;

/// Whether `status` is retryable under `policy`. 429 (rate-limit) is always
/// retried. 502/503/504 (transient gateway errors) are retried only when
/// `policy.retry_on_transient_5xx` is set — opt-in to avoid double-billing
/// on billable generation endpoints. A bare 500 is never retried (often
/// deterministic / double-bill risk).
fn is_retryable_status(status: u16, policy: &RetryPolicy) -> bool {
    if status == 429 {
        return true;
    }
    if matches!(status, 502..=504) {
        return policy.retry_on_transient_5xx;
    }
    false
}

/// Resolve the effective retry count from the environment, clamped to
/// [`MAX_UPSTREAM_MAX_RETRIES`]. The override is effectively static for a running
/// process (like [`upstream_read_timeout`]), so it is parsed once and cached
/// rather than re-read on every request in the hot path. The pure parse lives in
/// [`parse_max_retries`] so the fallbacks stay testable without touching env.
fn upstream_max_retries() -> u32 {
    static CACHED: Lazy<u32> = Lazy::new(|| {
        parse_max_retries(
            std::env::var("COPILOT_API_UPSTREAM_MAX_RETRIES")
                .ok()
                .as_deref(),
        )
    });
    *CACHED
}

/// Parse the `COPILOT_API_UPSTREAM_MAX_RETRIES` override, falling back to
/// [`DEFAULT_UPSTREAM_MAX_RETRIES`] on absent/garbage input and clamping to
/// [`MAX_UPSTREAM_MAX_RETRIES`]. Split from [`upstream_max_retries`] so it is
/// testable without mutating process env.
fn parse_max_retries(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_UPSTREAM_MAX_RETRIES)
        .min(MAX_UPSTREAM_MAX_RETRIES)
}

/// Deterministic exponential backoff base for retry `attempt` (0-indexed:
/// `attempt = 0` is the wait before the first retry), WITHOUT jitter. Factored
/// out so the growth curve is unit-testable; the live path adds up to
/// [`RETRY_BACKOFF_JITTER_MS`] of random jitter on top. The shift is bounded so a
/// large configured retry count can't overflow.
fn retry_backoff_base_ms(attempt: u32) -> u64 {
    RETRY_BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.min(10))
}

/// Combine the deterministic exponential base with a (caller-supplied) jitter
/// value. Kept pure — the live path passes a random `jitter_ms`, tests pass a
/// fixed one — so the exact delay is assertable.
fn retry_backoff_with_jitter(attempt: u32, jitter_ms: u64) -> Duration {
    Duration::from_millis(retry_backoff_base_ms(attempt).saturating_add(jitter_ms))
}

/// Parse a `Retry-After` header value as a delay, honoring the numeric
/// "delta-seconds" form (the only form Copilot/Codex upstreams emit). The
/// HTTP-date form is intentionally unhandled (it never appears here) and yields
/// `None`, falling back to exponential backoff. The result is capped at
/// [`MAX_RETRY_AFTER_SECS`] so a hostile or badly-overloaded upstream can't park
/// a request for minutes. Pure, so the parse + cap are unit-testable.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let secs: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(secs.min(MAX_RETRY_AFTER_SECS)))
}

/// How [`send_with_retry`] should bound its retries. Carried explicitly (rather
/// than read inline) so call sites stay uniform and a future caller can opt into
/// a tighter/looser budget without a new function.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Extra attempts beyond the first send.
    pub max_retries: u32,
    /// When `true`, 502/503/504 transient errors are retried in addition to
    /// 429 (rate-limit), which is always retried.
    /// Default is `false` (opt-in) because retrying billable generation
    /// endpoints risks double-billing on partially-processed requests.
    pub retry_on_transient_5xx: bool,
}

impl RetryPolicy {
    /// Policy derived from the environment (`COPILOT_API_UPSTREAM_MAX_RETRIES`
    /// and `COPILOT_API_UPSTREAM_RETRY_5XX`), the standard policy for the
    /// retryable upstream call sites.
    pub fn from_env() -> Self {
        let retry_5xx = std::env::var("COPILOT_API_UPSTREAM_RETRY_5XX")
            .ok()
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes"
            })
            .unwrap_or(false);
        Self {
            max_retries: upstream_max_retries(),
            retry_on_transient_5xx: retry_5xx,
        }
    }

    /// Conservative policy for billable generation endpoints: never retry
    /// transient 5xx errors to avoid potential double-billing. 429 is still
    /// retried (it indicates rate-limiting, not a completed charge).
    pub fn billable_generation() -> Self {
        Self {
            max_retries: upstream_max_retries(),
            retry_on_transient_5xx: false,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Compute the backoff before a retry: honor a parsed `Retry-After` when present,
/// otherwise exponential base + random jitter. Returns the delay and `true` if it
/// came from `Retry-After` (for logging). Pure given `retry_after` and `jitter_ms`.
fn next_retry_delay(
    attempt: u32,
    retry_after: Option<Duration>,
    jitter_ms: u64,
) -> (Duration, bool) {
    match retry_after {
        Some(d) => (d, true),
        None => (retry_backoff_with_jitter(attempt, jitter_ms), false),
    }
}

/// Send a request, retrying on a genuine connection failure OR a retryable
/// upstream HTTP status ({429, 502, 503, 504}).
///
/// CRITICAL SAFETY: a retry only ever happens *before any response byte has
/// streamed*. The HTTP status line is observable as soon as `send().await`
/// resolves — before the body is consumed — so retrying a retryable status drops
/// the not-yet-read response and replays the request without risking a partial,
/// already-billed generation. We deliberately do NOT retry on read/body errors
/// mid-stream (the connection was live and output may have flowed; replaying
/// could double-bill). Connect failures are safe for the same reason as
/// [`send_with_connect_retry`]: the request never reached the model.
///
/// Backoff is small and jittered (~250ms base, see [`RETRY_BACKOFF_BASE_MS`]); a
/// `Retry-After` header on a retryable status is honored and capped. `endpoint`
/// is a bounded label used only for the retry counter. Bodies are owned `Bytes`,
/// so `try_clone` always succeeds; if it somehow doesn't, the request is sent
/// once with no retry.
pub async fn send_with_retry(
    builder: reqwest::RequestBuilder,
    endpoint: &'static str,
    policy: RetryPolicy,
) -> reqwest::Result<reqwest::Response> {
    let max_retries = policy.max_retries;
    let mut current = builder;
    let mut attempt: u32 = 0;
    loop {
        // Clone for a possible replay BEFORE consuming `current` in send().
        let next = (attempt < max_retries)
            .then(|| current.try_clone())
            .flatten();
        let result = current.send().await;

        // Decide whether this outcome is retryable, and why. Only the status
        // line / connect error is inspected — no body is read here, so no stream
        // byte has flowed.
        let reason: Option<&'static str> = match &result {
            Err(e) if e.is_connect() => Some(RETRY_REASON_CONNECT),
            Ok(resp) if is_retryable_status(resp.status().as_u16(), &policy) => {
                Some(RETRY_REASON_STATUS)
            }
            _ => None,
        };

        let Some(reason) = reason else {
            return result;
        };
        let Some(next) = next else {
            // Out of retries (or un-cloneable): surface the last outcome.
            return result;
        };

        // Honor Retry-After only for a status response; a connect error has no
        // response to read it from.
        let retry_after = match &result {
            Ok(resp) => resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after),
            Err(_) => None,
        };
        let jitter_ms = rand::random::<u64>() % (RETRY_BACKOFF_JITTER_MS + 1);
        let (delay, from_header) = next_retry_delay(attempt, retry_after, jitter_ms);

        match &result {
            Err(e) => tracing::warn!(
                "upstream connect error ({endpoint}); retry {}/{max_retries} in {}ms: {e}",
                attempt + 1,
                delay.as_millis()
            ),
            Ok(resp) => tracing::warn!(
                "upstream retryable status {} ({endpoint}); retry {}/{max_retries} in {}ms{}",
                resp.status().as_u16(),
                attempt + 1,
                delay.as_millis(),
                if from_header { " (Retry-After)" } else { "" }
            ),
        }
        metrics::counter!("copilot_upstream_retry_total", "endpoint" => endpoint, "reason" => reason)
            .increment(1);
        tokio::time::sleep(delay).await;
        current = next;
        attempt += 1;
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

/// Read a non-streaming upstream response body into memory with a hard byte cap.
/// Unlike `reqwest::Response::bytes`, this cannot allocate without bound when a
/// compromised or misconfigured provider returns an enormous body.
pub async fn read_bytes_capped(response: reqwest::Response) -> Result<bytes::Bytes, String> {
    read_bytes_capped_with_max(response, MAX_UPSTREAM_RESPONSE_BYTES).await
}

/// Read a response body with a caller-supplied byte cap. Use [`read_bytes_capped`]
/// for standard upstream API responses; use this directly when a different limit
/// applies (e.g. binary self-update downloads).
pub async fn read_bytes_capped_with_max(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<bytes::Bytes, String> {
    use futures_util::StreamExt;

    // Fast-fail on an advertised Content-Length over the cap before reading.
    if let Some(len) = response.content_length() {
        if len > max_bytes as u64 {
            return Err(format!(
                "upstream response too large: {len} bytes > {max_bytes} cap"
            ));
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("error reading upstream response: {e}"))?;
        if chunk.len() > max_bytes.saturating_sub(buf.len()) {
            return Err(format!("upstream response exceeded {max_bytes} byte cap"));
        }
        buf.extend_from_slice(&chunk);
    }

    Ok(bytes::Bytes::from(buf))
}

/// Read a capped non-streaming body and deserialize it as `T`.
pub async fn read_json_capped<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, String> {
    let bytes = read_bytes_capped(response).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("failed to parse upstream response: {e}"))
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

    #[tokio::test]
    async fn read_bytes_capped_rejects_oversized_content_length_before_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_UPSTREAM_RESPONSE_BYTES + 1
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let response = client()
            .get(format!("http://{addr}/oversized"))
            .send()
            .await
            .expect("response headers");
        let error = read_bytes_capped(response)
            .await
            .expect_err("oversized response must fail");
        assert!(error.contains("too large"));
        server.await.expect("test server task");
    }

    #[test]
    fn owned_body_requests_are_cloneable_for_retry() {
        // send_with_connect_retry depends on try_clone() succeeding so it can
        // replay the request after a connect failure. A request built with an
        // owned byte body (as all our upstream calls are) must be cloneable;
        // streaming bodies would not be.
        let body = serialize_json_body(&serde_json::json!({"messages": ["hello"]})).unwrap();
        let cloned = body.clone();
        assert_eq!(
            body.as_ptr(),
            cloned.as_ptr(),
            "retry-body clones must share the serialized allocation"
        );
        let req = client()
            .post("http://127.0.0.1:0/v1/messages")
            .body(body)
            .build()
            .unwrap();
        let rebuilt = client().post("http://127.0.0.1:0/v1/messages").body(cloned);
        assert!(
            rebuilt.try_clone().is_some(),
            "owned-body requests must be cloneable so the retry can replay them"
        );
        drop(req);
    }

    #[test]
    fn retry_counter_is_preregistered_at_zero() {
        // After startup pre-registration, the retry counter series must exist at
        // 0 for every known endpoint/reason pair so dashboards/alerts read 0, not
        // "no data".
        crate::libs::metrics::init_build_info(); // installs the recorder
        preregister_retry_metrics();
        let out = crate::libs::metrics::render();
        for endpoint in RETRY_ENDPOINTS {
            for reason in RETRY_REASONS {
                assert!(
                    out.contains(&format!(
                        "copilot_upstream_retry_total{{endpoint=\"{endpoint}\",reason=\"{reason}\"}} 0"
                    )),
                    "expected pre-registered retry counter at 0 for endpoint={endpoint} reason={reason}, got:\n{out}"
                );
            }
        }
        assert!(out.contains("copilot_responses_websocket_fallback_total{provider=\"codex\"} 0"));
    }

    #[test]
    fn only_known_transient_statuses_are_retryable() {
        let permissive = RetryPolicy {
            max_retries: 3,
            retry_on_transient_5xx: true,
        };
        let strict = RetryPolicy {
            max_retries: 3,
            retry_on_transient_5xx: false,
        };

        // 429 is always retried regardless of policy.
        assert!(is_retryable_status(429, &permissive));
        assert!(is_retryable_status(429, &strict));

        // With 5xx opt-in, 502/503/504 are retried.
        for code in [502, 503, 504] {
            assert!(
                is_retryable_status(code, &permissive),
                "{code} should be retryable with retry_on_transient_5xx=true"
            );
        }

        // Without 5xx opt-in, 502/503/504 are NOT retried.
        for code in [502, 503, 504] {
            assert!(
                !is_retryable_status(code, &strict),
                "{code} should not be retried without retry_on_transient_5xx"
            );
        }

        // 500 is deliberately excluded (often deterministic / double-bill risk),
        // and success / 4xx-other are not retried regardless of policy.
        for code in [200, 400, 401, 403, 404, 418, 500, 501] {
            assert!(
                !is_retryable_status(code, &permissive),
                "{code} should not be retryable"
            );
        }
    }

    #[test]
    fn parse_max_retries_clamps_and_falls_back() {
        assert_eq!(parse_max_retries(None), DEFAULT_UPSTREAM_MAX_RETRIES);
        assert_eq!(
            parse_max_retries(Some("not-a-number")),
            DEFAULT_UPSTREAM_MAX_RETRIES
        );
        assert_eq!(parse_max_retries(Some("")), DEFAULT_UPSTREAM_MAX_RETRIES);
        assert_eq!(parse_max_retries(Some(" 3 ")), 3);
        assert_eq!(parse_max_retries(Some("0")), 0);
        // Absurd values are clamped to the ceiling.
        assert_eq!(parse_max_retries(Some("9999")), MAX_UPSTREAM_MAX_RETRIES);
    }

    #[test]
    fn backoff_grows_exponentially_with_jitter() {
        // attempt 0 -> base, attempt 1 -> 2*base, attempt 2 -> 4*base (no jitter).
        assert_eq!(
            retry_backoff_with_jitter(0, 0),
            Duration::from_millis(RETRY_BACKOFF_BASE_MS)
        );
        assert_eq!(
            retry_backoff_with_jitter(1, 0),
            Duration::from_millis(RETRY_BACKOFF_BASE_MS * 2)
        );
        assert_eq!(
            retry_backoff_with_jitter(2, 0),
            Duration::from_millis(RETRY_BACKOFF_BASE_MS * 4)
        );
        // Jitter is added on top of the deterministic base.
        assert_eq!(
            retry_backoff_with_jitter(0, 100),
            Duration::from_millis(RETRY_BACKOFF_BASE_MS + 100)
        );
        // A large attempt count must not overflow the shift.
        let _ = retry_backoff_base_ms(u32::MAX);
    }

    #[test]
    fn parse_retry_after_handles_seconds_and_caps() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after("  1  "), Some(Duration::from_secs(1)));
        // Capped to the sane ceiling.
        assert_eq!(
            parse_retry_after("120"),
            Some(Duration::from_secs(MAX_RETRY_AFTER_SECS))
        );
        // HTTP-date form and garbage are not honored.
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn next_retry_delay_prefers_retry_after() {
        // Retry-After wins over computed backoff and is flagged as header-sourced.
        let (d, from_header) = next_retry_delay(3, Some(Duration::from_secs(2)), 9999);
        assert_eq!(d, Duration::from_secs(2));
        assert!(from_header);
        // Without a header, falls back to exponential base + jitter.
        let (d, from_header) = next_retry_delay(0, None, 50);
        assert_eq!(d, Duration::from_millis(RETRY_BACKOFF_BASE_MS + 50));
        assert!(!from_header);
    }
}
