//! Port of services/providers/provider-proxy.ts: forwards Anthropic /
//! OpenAI-compatible requests to a configured upstream provider and proxies the
//! response back to the client unchanged.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use once_cell::sync::Lazy;

use crate::libs::config::ResolvedProviderConfig;
use crate::libs::error::HttpError;
use crate::libs::http::proxy_from_env_enabled;
use crate::routes::messages::anthropic_types::AnthropicMessagesPayload;
use crate::services::copilot::create_chat_completions::ChatCompletionsPayload;
use crate::services::copilot::create_responses::ResponsesPayload;

/// Env opt-out: when set to `1`, SSRF validation allows provider base URLs that
/// resolve to loopback / link-local / private ranges. Intended for local dev
/// against a localhost provider; never set this in production.
const ALLOW_PRIVATE_PROVIDERS_ENV: &str = "COPILOT_API_ALLOW_PRIVATE_PROVIDERS";

/// Dedicated reqwest client for provider forwarding. Unlike the shared
/// `client()` in http.rs, this one disables redirect-following so an allowed
/// upstream host cannot 302 the request to an internal address (SSRF bypass).
/// The rest of the config mirrors `client()` (connect timeout, idle pool,
/// rustls native roots via Cargo features, proxy-from-env gating).
static PROVIDER_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        // read_timeout bounds the gap between successive reads, NOT the total
        // request duration, so it never caps a healthy long SSE stream that
        // keeps producing bytes but does rescue a stalled-open upstream
        // connection. An overall `.timeout(...)` would truncate legitimate long
        // streams, so it is intentionally absent. Shares the shared client's
        // COPILOT_API_UPSTREAM_READ_TIMEOUT_SECS knob (default 120; 0 disables).
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(read_timeout) = crate::libs::http::upstream_read_timeout() {
        builder = builder.read_timeout(read_timeout);
    }
    if !proxy_from_env_enabled() {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .expect("failed to build provider reqwest client")
});

/// The provider-forwarding HTTP client (redirects disabled).
fn provider_client() -> &'static reqwest::Client {
    &PROVIDER_CLIENT
}

fn allow_private_providers() -> bool {
    std::env::var(ALLOW_PRIVATE_PROVIDERS_ENV)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// True for IPs that must never be reachable through a user-configured provider
/// base URL: loopback (127.0.0.0/8, ::1), link-local (169.254.0.0/16 — the cloud
/// metadata range — and fe80::/10), and the RFC1918 / unique-local private
/// ranges (10/8, 172.16/12, 192.168/16, fc00::/7). Also covers the unspecified
/// address and IPv4-mapped IPv6 forms of the above.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_ipv4(mapped);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || is_unique_local_v6(v6)
                || is_link_local_v6(v6)
        }
    }
}

fn is_blocked_ipv4(v4: Ipv4Addr) -> bool {
    let [a, b, _, _] = v4.octets();
    v4.is_loopback()        // 127.0.0.0/8
        || v4.is_private()  // 10/8, 172.16/12, 192.168/16
        || v4.is_link_local() // 169.254.0.0/16 (cloud metadata)
        // 100.64.0.0/10 — carrier-grade NAT (RFC 6598). Some clouds expose
        // internal/metadata services here (e.g. Alibaba Cloud metadata at
        // 100.100.100.200). `Ipv4Addr::is_shared()` is unstable, so check the
        // range explicitly.
        || (a == 100 && (64..=127).contains(&b))
        || v4.is_unspecified() // 0.0.0.0
        || v4.is_broadcast()
}

/// fc00::/7 — IPv6 unique local addresses (the private-range analogue).
fn is_unique_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// fe80::/10 — IPv6 link-local.
fn is_link_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// Hostnames that name the local machine / internal infra and must be rejected
/// even without DNS resolution.
fn is_blocked_hostname(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h == "ip6-localhost"
        || h == "ip6-loopback"
        // Cloud metadata service hostnames.
        || h == "metadata"
        || h == "metadata.google.internal"
}

/// Validate a user-configured provider base URL before forwarding to it, to
/// mitigate SSRF. Rejects non-http(s) schemes and hosts that point at loopback,
/// link-local (cloud metadata), or private ranges — unless the
/// `COPILOT_API_ALLOW_PRIVATE_PROVIDERS=1` opt-out is set.
///
/// NOTE: for hostnames (not IP literals) this is a best-effort name-based check.
/// We do not perform DNS resolution here, so a name that resolves to an internal
/// IP can still pass. Full resolution + re-check is the ideal but suffers a
/// known TOCTOU gap (DNS can change between the check and the actual connect),
/// so we deliberately keep this to a name + IP-literal check. Redirect-following
/// is disabled on the provider client to close the most common bypass.
#[allow(clippy::result_large_err)]
pub fn validate_upstream_url(url: &str) -> Result<(), HttpError> {
    let reject = |msg: &str| {
        HttpError::new(
            msg.to_string(),
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            String::new(),
        )
    };

    let parsed = url::Url::parse(url.trim())
        .map_err(|e| reject(&format!("Invalid provider base URL: {e}")))?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(reject(&format!(
            "Provider base URL scheme '{scheme}' is not allowed (only http/https)"
        )));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| reject("Provider base URL has no host"))?;

    let allow_private = allow_private_providers();

    // IP literal: check the address directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) && !allow_private {
            return Err(reject(
                "Provider base URL resolves to a blocked (loopback/link-local/private) address",
            ));
        }
        return Ok(());
    }
    // Bracketed IPv6 literal (url::Host strips brackets, but be defensive).
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        if is_blocked_ip(ip) && !allow_private {
            return Err(reject(
                "Provider base URL resolves to a blocked (loopback/link-local/private) address",
            ));
        }
        return Ok(());
    }

    // Hostname: best-effort name-based rejection (see NOTE above).
    if is_blocked_hostname(host) && !allow_private {
        return Err(reject(
            "Provider base URL host is a blocked internal name (e.g. localhost/metadata)",
        ));
    }

    Ok(())
}

/// Request headers copied through to the upstream for every provider type.
const SHARED_FORWARDABLE_HEADERS: [&str; 2] = ["accept", "user-agent"];

/// Additional request headers copied through only for `anthropic` providers.
const ANTHROPIC_FORWARDABLE_HEADERS: [&str; 2] = ["anthropic-version", "anthropic-beta"];

/// Hop-by-hop / encoding headers stripped from the upstream response before it
/// is forwarded to the client.
const STRIPPED_RESPONSE_HEADERS: [&str; 10] = [
    "connection",
    "content-encoding",
    "content-length",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Build the headers sent to the upstream provider. Mirrors
/// `buildProviderUpstreamHeaders`.
pub fn build_provider_upstream_headers(
    cfg: &ResolvedProviderConfig,
    request_headers: &HeaderMap,
) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderName, HeaderValue};

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept", HeaderValue::from_static("application/json"));

    if cfg.auth_type == "x-api-key" {
        if let Ok(v) = HeaderValue::from_str(&cfg.api_key) {
            headers.insert("x-api-key", v);
        }
    } else if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", cfg.api_key)) {
        headers.insert("authorization", v);
    }

    let mut copy_header = |name: &str| {
        if let Some(value) = request_headers.get(name) {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                headers.insert(n, v);
            }
        }
    };

    for name in SHARED_FORWARDABLE_HEADERS {
        copy_header(name);
    }

    if cfg.provider_type != "anthropic" {
        return headers;
    }

    for name in ANTHROPIC_FORWARDABLE_HEADERS {
        copy_header(name);
    }

    headers
}

/// Pass-through proxy response: copy status and headers (minus the stripped
/// set) and stream the upstream body to the client. Mirrors
/// `createProviderProxyResponse`.
pub fn create_provider_proxy_response(upstream: reqwest::Response) -> Response {
    use axum::http::{HeaderName, HeaderValue, StatusCode};

    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if STRIPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            // `append` (not `insert`) so multi-value headers such as
            // `set-cookie` are forwarded in full rather than collapsed to one.
            headers.append(n, v);
        }
    }

    let body = Body::from_stream(upstream.bytes_stream());

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// POST {base_url}/v1/messages. Returns the raw upstream response; the caller
/// inspects the status. Mirrors `forwardProviderMessages`.
pub async fn forward_provider_messages(
    cfg: &ResolvedProviderConfig,
    payload: &AnthropicMessagesPayload,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, HttpError> {
    tracing::info!("<-- model: {}", payload.model);
    validate_upstream_url(&cfg.base_url)?;
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    provider_client()
        .post(format!("{}/v1/messages", cfg.base_url))
        .headers(build_provider_upstream_headers(cfg, request_headers))
        .body(body)
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to forward provider messages: {e}")))
}

/// POST {base_url}/v1/chat/completions. Mirrors
/// `forwardProviderChatCompletions`.
pub async fn forward_provider_chat_completions(
    cfg: &ResolvedProviderConfig,
    payload: &ChatCompletionsPayload,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, HttpError> {
    tracing::info!("<-- model: {}", payload.model);
    validate_upstream_url(&cfg.base_url)?;
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    provider_client()
        .post(format!("{}/v1/chat/completions", cfg.base_url))
        .headers(build_provider_upstream_headers(cfg, request_headers))
        .body(body)
        .send()
        .await
        .map_err(|e| {
            HttpError::internal(format!("Failed to forward provider chat completions: {e}"))
        })
}

/// POST {base_url}/v1/responses. Mirrors `forwardProviderResponses`.
pub async fn forward_provider_responses(
    cfg: &ResolvedProviderConfig,
    payload: &ResponsesPayload,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, HttpError> {
    tracing::info!("<-- model: {}", payload.model);
    validate_upstream_url(&cfg.base_url)?;
    let body = serde_json::to_vec(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    provider_client()
        .post(format!("{}/v1/responses", cfg.base_url))
        .headers(build_provider_upstream_headers(cfg, request_headers))
        .body(body)
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to forward provider responses: {e}")))
}

/// GET {base_url}/v1/models (no body, no model log). Mirrors
/// `forwardProviderModels`.
pub async fn forward_provider_models(
    cfg: &ResolvedProviderConfig,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, HttpError> {
    validate_upstream_url(&cfg.base_url)?;
    provider_client()
        .get(format!("{}/v1/models", cfg.base_url))
        .headers(build_provider_upstream_headers(cfg, request_headers))
        .send()
        .await
        .map_err(|e| HttpError::internal(format!("Failed to forward provider models: {e}")))
}

/// Maximum time a single health probe waits for the upstream to respond.
///
/// PROVIDER_CLIENT sets only connect + read (gap-between-reads) timeouts and
/// deliberately omits an overall `.timeout(...)` so it never truncates a healthy
/// long SSE stream. A health probe has no such constraint, so we MUST cap each
/// probe with an explicit per-request timeout — otherwise a stalled-open
/// upstream (accepts the connection, never replies) would hang the admin call.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Result of a lightweight provider health probe. Kept richer than a boolean so
/// the admin endpoint can distinguish "reachable but rejected us" (401/403),
/// "wrong path/base URL" (404), and "could not connect at all" (DNS/TCP/TLS/
/// timeout) — each is a different operator action.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// The upstream returned an HTTP response with this status code.
    Status(u16),
    /// No HTTP response was obtained. The string is a short, secret-free
    /// category ("timeout", "connect", "request", "blocked"), never the raw
    /// error (which could echo the configured URL).
    Unreachable(String),
}

/// Issue a lightweight `GET {base_url}/v1/models` health probe using the
/// provider's configured auth header, returning the outcome and the round-trip
/// latency. Used by the `/admin/providers/health` endpoint so a bad apiKey /
/// wrong baseUrl / unreachable host surfaces immediately instead of on the first
/// real generation. Never returns the apiKey or raw transport error text.
pub async fn probe_provider_models(cfg: &ResolvedProviderConfig) -> (ProbeOutcome, u128) {
    let start = Instant::now();

    // Reuse the same SSRF guard the real forwarding path applies, so a probe
    // can't be turned into a port scanner against internal addresses.
    if let Err(err) = validate_upstream_url(&cfg.base_url) {
        // Keep the response field a stable category ("blocked") for clean
        // monitoring/alerting; log the detailed reason separately for operators.
        tracing::warn!(
            provider = %cfg.name,
            "provider health probe blocked by SSRF guard: {}",
            err.message
        );
        return (
            ProbeOutcome::Unreachable("blocked".to_string()),
            start.elapsed().as_millis(),
        );
    }

    let headers = build_provider_upstream_headers(cfg, &HeaderMap::new());
    let result = provider_client()
        .get(format!("{}/v1/models", cfg.base_url))
        .headers(headers)
        // See PROBE_TIMEOUT: PROVIDER_CLIENT has no overall request timeout.
        .timeout(PROBE_TIMEOUT)
        .send()
        .await;

    let latency = start.elapsed().as_millis();
    match result {
        Ok(resp) => (ProbeOutcome::Status(resp.status().as_u16()), latency),
        Err(err) => (
            ProbeOutcome::Unreachable(classify_probe_error(&err)),
            latency,
        ),
    }
}

/// Reduce a reqwest error to a short, secret-free category. We never surface the
/// `Display` of the error because it can embed the request URL. The category set
/// is intentionally small and stable ("timeout"/"connect"/"request" here, plus
/// "blocked" from the SSRF guard) so the admin endpoint is easy to alert on;
/// redirects fold into "request" (redirects are disabled on the client anyway).
fn classify_probe_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "timeout".to_string()
    } else if err.is_connect() {
        "connect".to_string()
    } else {
        "request".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg(provider_type: &str, auth_type: &str, api_key: &str) -> ResolvedProviderConfig {
        ResolvedProviderConfig {
            name: "test".to_string(),
            provider_type: provider_type.to_string(),
            base_url: "https://example.com".to_string(),
            api_key: api_key.to_string(),
            auth_type: auth_type.to_string(),
            models: Some(BTreeMap::new()),
            adjust_input_tokens: None,
        }
    }

    #[test]
    fn x_api_key_auth() {
        let headers = build_provider_upstream_headers(
            &cfg("anthropic", "x-api-key", "secret"),
            &HeaderMap::new(),
        );
        assert_eq!(headers.get("x-api-key").unwrap(), "secret");
        assert!(headers.get("authorization").is_none());
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("accept").unwrap(), "application/json");
    }

    #[test]
    fn bearer_auth() {
        let headers = build_provider_upstream_headers(
            &cfg("openai-compatible", "authorization", "secret"),
            &HeaderMap::new(),
        );
        assert_eq!(headers.get("authorization").unwrap(), "Bearer secret");
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn shared_headers_forwarded_and_accept_overwritten() {
        let mut req = HeaderMap::new();
        req.insert("accept", "text/event-stream".parse().unwrap());
        req.insert("user-agent", "agent/1.0".parse().unwrap());
        let headers =
            build_provider_upstream_headers(&cfg("openai-compatible", "authorization", "k"), &req);
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
        assert_eq!(headers.get("user-agent").unwrap(), "agent/1.0");
    }

    #[test]
    fn anthropic_headers_only_for_anthropic() {
        let mut req = HeaderMap::new();
        req.insert("anthropic-version", "2023-06-01".parse().unwrap());
        req.insert("anthropic-beta", "beta-feature".parse().unwrap());

        let anthropic = build_provider_upstream_headers(&cfg("anthropic", "x-api-key", "k"), &req);
        assert_eq!(anthropic.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(anthropic.get("anthropic-beta").unwrap(), "beta-feature");

        let openai =
            build_provider_upstream_headers(&cfg("openai-compatible", "authorization", "k"), &req);
        assert!(openai.get("anthropic-version").is_none());
        assert!(openai.get("anthropic-beta").is_none());
    }

    // Validation tests mutate the process-wide opt-out env var, which is global
    // shared state. Guard with a mutex so they cannot race under the test runner.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn validate_rejects_loopback_ip() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_PRIVATE_PROVIDERS_ENV);
        assert!(validate_upstream_url("http://127.0.0.1").is_err());
        assert!(validate_upstream_url("http://127.0.0.1:8080/v1").is_err());
    }

    #[test]
    fn validate_rejects_link_local_metadata_ip() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_PRIVATE_PROVIDERS_ENV);
        assert!(validate_upstream_url("http://169.254.169.254").is_err());
    }

    #[test]
    fn validate_rejects_cgnat_metadata_ip() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_PRIVATE_PROVIDERS_ENV);
        // 100.64.0.0/10 (carrier-grade NAT) hosts cloud metadata on some clouds,
        // e.g. Alibaba Cloud at 100.100.100.200.
        assert!(validate_upstream_url("http://100.100.100.200").is_err());
        assert!(validate_upstream_url("http://100.64.0.1").is_err());
        assert!(validate_upstream_url("http://100.127.255.255/v1").is_err());
        // Boundaries just outside the range must still be allowed.
        assert!(validate_upstream_url("http://100.63.255.255").is_ok());
        assert!(validate_upstream_url("http://100.128.0.1").is_ok());
    }

    #[test]
    fn validate_rejects_localhost_name() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_PRIVATE_PROVIDERS_ENV);
        assert!(validate_upstream_url("http://localhost").is_err());
        assert!(validate_upstream_url("http://localhost:11434/v1").is_err());
    }

    #[test]
    fn validate_rejects_private_ranges() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_PRIVATE_PROVIDERS_ENV);
        assert!(validate_upstream_url("http://10.0.0.5").is_err());
        assert!(validate_upstream_url("http://172.16.4.2").is_err());
        assert!(validate_upstream_url("http://192.168.1.1").is_err());
        assert!(validate_upstream_url("http://[::1]/v1").is_err());
    }

    #[test]
    fn validate_rejects_non_http_scheme() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_PRIVATE_PROVIDERS_ENV);
        assert!(validate_upstream_url("file:///etc/passwd").is_err());
        assert!(validate_upstream_url("ftp://example.com").is_err());
    }

    #[test]
    fn validate_allows_public_host() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ALLOW_PRIVATE_PROVIDERS_ENV);
        assert!(validate_upstream_url("https://api.example.com").is_ok());
        assert!(validate_upstream_url("https://api.example.com/v1/messages").is_ok());
    }

    #[test]
    fn validate_allows_loopback_when_opt_out_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(ALLOW_PRIVATE_PROVIDERS_ENV, "1");
        assert!(validate_upstream_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_upstream_url("http://localhost:11434").is_ok());
        std::env::remove_var(ALLOW_PRIVATE_PROVIDERS_ENV);
    }

    #[tokio::test]
    async fn probe_blocked_base_url_is_unreachable_without_network() {
        // A non-http(s) scheme is rejected by the SSRF guard regardless of the
        // private-provider opt-out, so the probe reports Unreachable
        // deterministically with no socket opened and no env-var coupling (hence
        // no lock held across the await).
        let mut c = cfg("openai-compatible", "authorization", "secret");
        c.base_url = "ftp://example.com".to_string();
        let (outcome, _latency) = probe_provider_models(&c).await;
        match outcome {
            ProbeOutcome::Unreachable(reason) => {
                assert!(reason.starts_with("blocked"), "reason was: {reason}");
                // The configured apiKey must never leak into the error string.
                assert!(!reason.contains("secret"));
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }
}
