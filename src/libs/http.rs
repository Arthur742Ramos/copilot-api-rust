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
