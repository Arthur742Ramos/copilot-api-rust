use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use std::sync::Mutex;

use crate::libs::api_config::is_opencode_oauth_app;
use crate::libs::config::{get_raw_provider_config, set_provider_config, ProviderConfig};
use crate::libs::credential_store::{
    read_codex_credentials, read_github_token, write_codex_credentials, write_github_token,
};
use crate::libs::error::HttpError;
use crate::libs::http::{send_with_retry, RetryPolicy};
use crate::libs::oauth::codex::{
    is_codex_credentials_expired, refresh_codex_credentials, CodexCredentials, CODEX_API_BASE_URL,
};
use crate::libs::state;
use crate::services::github::get_copilot_token::get_copilot_token;
use crate::services::github::get_copilot_usage::get_copilot_usage;
use crate::services::github::get_device_code::get_device_code;
use crate::services::github::get_user::get_github_user;
use crate::services::github::poll_access_token::poll_access_token;

// --- Refresh loop controllers (mirror AbortController in token.ts) ----------

struct LoopController {
    aborted: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

static COPILOT_REFRESH: Lazy<Mutex<Option<LoopController>>> = Lazy::new(|| Mutex::new(None));
static CODEX_REFRESH: Lazy<Mutex<Option<LoopController>>> = Lazy::new(|| Mutex::new(None));

// --- Inline-refresh coalescing guards ---------------------------------------
//
// A stale/revoked token can self-heal on the very request that hit the 401
// rather than waiting for the background refresh loop. Both the inline 401 path
// and the background loop funnel through `force_refresh_*` under a per-token
// async `lock`, so N concurrent 401s coalesce into at most ONE upstream refresh.
// `deadline_ms` is the single source of truth for the next scheduled refresh;
// the background loop reads it every iteration so an inline refresh resyncs it
// (the loop just waits out the freshly-bumped deadline instead of double-firing).

/// Per-token refresh guard: an async lock that serializes refreshes plus the
/// shared next-refresh deadline (unix millis) the background loop polls.
struct TokenRefreshGuard {
    lock: tokio::sync::Mutex<()>,
    deadline_ms: AtomicI64,
}

impl TokenRefreshGuard {
    const fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::const_new(()),
            deadline_ms: AtomicI64::new(0),
        }
    }
}

static COPILOT_REFRESH_GUARD: TokenRefreshGuard = TokenRefreshGuard::new();
static CODEX_REFRESH_GUARD: TokenRefreshGuard = TokenRefreshGuard::new();

/// Whether an upstream status is HTTP 401 Unauthorized — the only status the
/// inline token-aware replay path acts on. Pure so it is unit-testable.
fn is_unauthorized(status: u16) -> bool {
    status == 401
}

/// Whether an inline refresh can be skipped because the token already rotated
/// out from under the caller (a concurrent 401 or the background loop already
/// refreshed). `stale` is the token the caller's failing request used; `current`
/// is what state holds now. A non-empty `current` that differs from `stale`
/// means a fresh token is already installed, so there is nothing to do. Pure so
/// the coalescing decision is unit-testable without a live upstream.
fn refresh_already_done(stale: &str, current: &str) -> bool {
    !current.is_empty() && current != stale
}

pub fn stop_copilot_refresh_loop() {
    if let Some(controller) = COPILOT_REFRESH.lock().unwrap().take() {
        controller.aborted.store(true, Ordering::SeqCst);
        controller.handle.abort();
    }
}

pub fn stop_codex_refresh_loop() {
    if let Some(controller) = CODEX_REFRESH.lock().unwrap().take() {
        controller.aborted.store(true, Ordering::SeqCst);
        controller.handle.abort();
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// --- Codex credential helpers ----------------------------------------------

fn apply_codex_credentials(credentials: &CodexCredentials) {
    state::with_state_mut(|s| {
        s.codex_access_token = Some(credentials.access_token.clone());
        s.codex_refresh_token = Some(credentials.refresh_token.clone());
        s.codex_expires_at = Some(credentials.expires_at);
        s.codex_account_id = Some(credentials.account_id.clone());
    });
    tracing::debug!("Codex credentials loaded successfully");
    if state::with_state(|s| s.show_token) {
        tracing::info!("Codex access token: {}", credentials.access_token);
    }
}

fn get_loaded_codex_credentials() -> Option<CodexCredentials> {
    state::with_state(|s| {
        match (
            &s.codex_access_token,
            &s.codex_refresh_token,
            s.codex_expires_at,
            &s.codex_account_id,
        ) {
            (Some(access), Some(refresh), Some(expires), Some(account)) => Some(CodexCredentials {
                access_token: access.clone(),
                refresh_token: refresh.clone(),
                expires_at: expires,
                account_id: account.clone(),
            }),
            _ => None,
        }
    })
}

fn sync_codex_provider_config(enabled: Option<bool>) {
    let existing = get_raw_provider_config("codex").unwrap_or_default();
    let next = ProviderConfig {
        provider_type: Some("openai-responses".to_string()),
        enabled: enabled.or(existing.enabled),
        base_url: Some(CODEX_API_BASE_URL.to_string()),
        auth_type: Some("oauth2".to_string()),
        api_key: existing.api_key,
        models: existing.models,
        capabilities: existing.capabilities,
        adjust_input_tokens: existing.adjust_input_tokens,
        extra: existing.extra,
    };
    if let Err(e) = set_provider_config("codex", next) {
        tracing::warn!("Failed to sync codex provider config: {e}");
    }
}

pub async fn persist_codex_credentials(
    credentials: &CodexCredentials,
    enable_provider: bool,
) -> Result<(), anyhow::Error> {
    write_codex_credentials(credentials).await?;
    sync_codex_provider_config(if enable_provider { Some(true) } else { None });
    apply_codex_credentials(credentials);
    Ok(())
}

// --- Copilot token setup ----------------------------------------------------

pub async fn setup_copilot_token() -> Result<(), anyhow::Error> {
    if is_opencode_oauth_app() {
        let github_token = state::with_state(|s| s.github_token.clone());
        let github_token =
            github_token.ok_or_else(|| anyhow::anyhow!("opencode token not found"))?;
        state::with_state_mut(|s| s.copilot_token = Some(github_token.clone()));
        tracing::debug!("GitHub Copilot token set from opencode auth token");
        if state::with_state(|s| s.show_token) {
            tracing::info!("Copilot token: {github_token}");
        }
        stop_copilot_refresh_loop();
        return Ok(());
    }

    let snapshot = state::snapshot();
    let token_response = get_copilot_token(&snapshot).await?;
    state::with_state_mut(|s| s.copilot_token = Some(token_response.token.clone()));

    tracing::debug!("GitHub Copilot Token fetched successfully!");
    if state::with_state(|s| s.show_token) {
        tracing::info!("Copilot token: {}", token_response.token);
    }

    stop_copilot_refresh_loop();

    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_clone = aborted.clone();
    let refresh_in = token_response.refresh_in;
    let handle = tokio::spawn(async move {
        run_copilot_refresh_loop(refresh_in, aborted_clone).await;
    });
    *COPILOT_REFRESH.lock().unwrap() = Some(LoopController { aborted, handle });
    Ok(())
}

pub async fn setup_codex_token() -> Result<(), anyhow::Error> {
    let loaded = get_loaded_codex_credentials();
    if let Some(ref creds) = loaded {
        if !is_codex_credentials_expired(creds.expires_at, None) {
            if CODEX_REFRESH.lock().unwrap().is_some() {
                return Ok(());
            }
            apply_codex_credentials(creds);
        }
    }

    let credentials = match loaded {
        Some(c) => c,
        None => read_codex_credentials().await?.ok_or_else(|| {
            anyhow::anyhow!(
                "Codex credentials not found. Run `copilot-api auth login --provider codex` first."
            )
        })?,
    };

    sync_codex_provider_config(None);

    let next_credentials = if is_codex_credentials_expired(credentials.expires_at, None) {
        tracing::debug!("Refreshing expired Codex credentials");
        let refreshed = refresh_codex_credentials(&credentials).await?;
        persist_codex_credentials(&refreshed, false).await?;
        refreshed
    } else {
        credentials
    };

    apply_codex_credentials(&next_credentials);
    stop_codex_refresh_loop();

    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_clone = aborted.clone();
    let handle = tokio::spawn(async move {
        run_codex_refresh_loop(aborted_clone).await;
    });
    *CODEX_REFRESH.lock().unwrap() = Some(LoopController { aborted, handle });
    Ok(())
}

// --- Refresh timing ---------------------------------------------------------

const REFRESH_POLL_INTERVAL_MS: i64 = 15_000;
const EARLY_REFRESH_BUFFER_MS: i64 = 60_000;
const RETRY_REFRESH_DELAY_MS: i64 = 15_000;
const MAX_RETRY_REFRESH_DELAY_MS: i64 = 600_000;
const RETRY_REFRESH_JITTER_MS: i64 = 15_000;
const MIN_REFRESH_DELAY_MS: i64 = 1_000;
/// Fallback token lifetime (seconds) when the upstream omits or zeroes
/// `refresh_in`. GitHub's Copilot token endpoint normally returns ~1500s; a
/// missing/`0` value is treated as this default rather than producing a ~1s
/// hot-refresh loop that would hammer the token endpoint (and risk a 429 that
/// disables every proxied request). Positive-but-small values are left alone —
/// they signal a genuinely imminent expiry and are clamped by MIN below.
const DEFAULT_REFRESH_IN_SECS: i64 = 1_500;
/// Upper bound on an accepted `refresh_in` (24h). `refresh_in` is taken directly
/// from upstream JSON; a pathological value would overflow `refresh_in * 1000`
/// (panicking the spawned refresh task in debug, or wrapping to a ~1s hot loop in
/// release — the very thing the floor below guards against). Clamping before the
/// multiply keeps the arithmetic in range. No real GitHub token lives this long.
const MAX_REFRESH_IN_SECS: i64 = 24 * 60 * 60;

pub fn get_refresh_deadline_ms(refresh_in: i64, now_ms: i64) -> i64 {
    // A non-positive refresh_in is missing/invalid, not "refresh now": substitute
    // a sane default so an upstream that omits the field can't drive a hot loop.
    // A pathologically large value is clamped so `refresh_in * 1000` can't overflow.
    let refresh_in = if refresh_in <= 0 {
        DEFAULT_REFRESH_IN_SECS
    } else {
        refresh_in.min(MAX_REFRESH_IN_SECS)
    };
    now_ms
        + std::cmp::max(
            refresh_in * 1000 - EARLY_REFRESH_BUFFER_MS,
            MIN_REFRESH_DELAY_MS,
        )
}

pub fn get_refresh_poll_delay_ms(refresh_at_ms: i64, now_ms: i64) -> i64 {
    (refresh_at_ms - now_ms).clamp(0, REFRESH_POLL_INTERVAL_MS)
}

/// Bounded refresh-outcome counter. `token` is a fixed enum (`copilot`/`codex`)
/// and `result` is `success`/`failure`, so the label set stays small.
fn record_refresh_result(token: &'static str, success: bool) {
    let result = if success { "success" } else { "failure" };
    metrics::counter!("copilot_token_refresh_total", "token" => token, "result" => result)
        .increment(1);
}

/// Gauge of the next scheduled refresh deadline (unix millis) per token kind.
/// Lets alerting catch a refresh loop that has stalled or fallen far behind.
fn record_refresh_deadline(token: &'static str, refresh_at_ms: i64) {
    metrics::gauge!("copilot_token_refresh_deadline_ms", "token" => token)
        .set(refresh_at_ms as f64);
}

// --- Force refresh (shared by inline 401 path and the background loop) -------

/// Force a Copilot token refresh, coalesced via [`COPILOT_REFRESH_GUARD`].
///
/// `stale` is the token the caller's request used (the one that 401'd, or the
/// loop's current token). Under the guard we re-check state: if the token has
/// already rotated to something else, the refresh is a no-op (a concurrent 401
/// or the loop beat us to it). Otherwise we exchange the GitHub token for a fresh
/// Copilot token, install it, and resync the shared deadline so the background
/// loop waits out the new lifetime instead of immediately re-refreshing.
///
/// The opencode oauth app has no Copilot token-exchange path (the Copilot token
/// *is* the GitHub token), so an inline refresh cannot help there; we surface an
/// error so the caller skips the replay and forwards the original 401.
pub async fn force_refresh_copilot_token(stale: &str) -> Result<(), anyhow::Error> {
    if is_opencode_oauth_app() {
        return Err(anyhow::anyhow!(
            "opencode oauth token cannot be refreshed inline"
        ));
    }

    let _guard = COPILOT_REFRESH_GUARD.lock.lock().await;

    let current = state::with_state(|s| s.copilot_token.clone()).unwrap_or_default();
    if refresh_already_done(stale, &current) {
        tracing::debug!("Copilot token already refreshed; coalescing");
        return Ok(());
    }

    tracing::debug!("Force-refreshing Copilot token");
    let snapshot = state::snapshot();
    match get_copilot_token(&snapshot).await {
        Ok(resp) => {
            state::with_state_mut(|s| s.copilot_token = Some(resp.token.clone()));
            let refresh_at_ms = get_refresh_deadline_ms(resp.refresh_in, now_millis());
            COPILOT_REFRESH_GUARD
                .deadline_ms
                .store(refresh_at_ms, Ordering::SeqCst);
            record_refresh_result("copilot", true);
            record_refresh_deadline("copilot", refresh_at_ms);
            tracing::debug!("Copilot token refreshed");
            if state::with_state(|s| s.show_token) {
                tracing::info!("Refreshed Copilot token: {}", resp.token);
            }
            Ok(())
        }
        Err(e) => {
            record_refresh_result("copilot", false);
            Err(anyhow::Error::new(e))
        }
    }
}

/// Force a Codex credential refresh, coalesced via [`CODEX_REFRESH_GUARD`].
/// Codex auth is a DISTINCT oauth2 path from the Copilot token; `stale` is the
/// access token the caller's request used. Mirrors
/// [`force_refresh_copilot_token`]'s coalescing + deadline-resync contract.
pub async fn force_refresh_codex_token(stale: &str) -> Result<(), anyhow::Error> {
    let _guard = CODEX_REFRESH_GUARD.lock.lock().await;

    let current = state::with_state(|s| s.codex_access_token.clone()).unwrap_or_default();
    if refresh_already_done(stale, &current) {
        tracing::debug!("Codex credentials already refreshed; coalescing");
        return Ok(());
    }

    let (expires_at, refresh_token) =
        state::with_state(|s| (s.codex_expires_at, s.codex_refresh_token.clone()));
    let (expires_at, refresh_token) = match (expires_at, refresh_token) {
        (Some(e), Some(r)) => (e, r),
        _ => return Err(anyhow::anyhow!("Codex refresh credentials not loaded")),
    };

    tracing::debug!("Force-refreshing Codex credentials");
    let current_credentials = state::with_state(|s| CodexCredentials {
        access_token: s.codex_access_token.clone().unwrap_or_default(),
        refresh_token: refresh_token.clone(),
        expires_at,
        account_id: s.codex_account_id.clone().unwrap_or_default(),
    });

    match refresh_codex_credentials(&current_credentials).await {
        Ok(credentials) => {
            persist_codex_credentials(&credentials, false).await?;
            let refresh_at_ms = std::cmp::max(
                credentials.expires_at - EARLY_REFRESH_BUFFER_MS,
                now_millis(),
            );
            CODEX_REFRESH_GUARD
                .deadline_ms
                .store(refresh_at_ms, Ordering::SeqCst);
            record_refresh_result("codex", true);
            record_refresh_deadline("codex", refresh_at_ms);
            tracing::debug!("Codex credentials refreshed");
            Ok(())
        }
        Err(e) => {
            record_refresh_result("codex", false);
            Err(e)
        }
    }
}

// --- Inline 401 refresh-and-replay ------------------------------------------

/// Send a request via [`send_with_retry`]; on a pre-stream HTTP 401, run
/// `refresh` and replay the request EXACTLY ONCE with a freshly-built builder.
///
/// CRITICAL SAFETY: the 401 is observed on the status line, before any response
/// body is read, so replaying cannot drop a partially-streamed (already-billed)
/// generation — the same invariant `send_with_retry` relies on. We never retry
/// mid-stream. The replay happens at most once: if it also 401s, that response
/// is surfaced unchanged. `refresh` returns `true` when a usable new token is in
/// place (replay worthwhile) and `false` otherwise (surface the original 401).
///
/// TOCTOU-free token identity: the token is read ONCE here via `read_token` and
/// threaded INTO `build`, so the request provably carries the exact token that
/// `refresh` then force-refreshes against. If `build` instead re-snapshotted the
/// token from global state, a background-loop rotation between the read and the
/// snapshot could make the failing request use a *different* token than the one
/// handed to `refresh` — the coalescing check would then see "already rotated"
/// and skip, leaving the single replay to reuse the same bad token (a silent
/// no-op). Threading the value removes that window entirely. The replay re-reads
/// `read_token` so it picks up the freshly-installed credential.
async fn send_with_401_retry_inner<Read, B, R, Fut>(
    endpoint: &'static str,
    read_token: Read,
    build: B,
    refresh: R,
) -> reqwest::Result<reqwest::Response>
where
    Read: Fn() -> String,
    B: Fn(&str) -> reqwest::RequestBuilder,
    R: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let stale = read_token();
    let response = send_with_retry(build(&stale), endpoint, RetryPolicy::from_env()).await?;
    if !is_unauthorized(response.status().as_u16()) {
        return Ok(response);
    }

    tracing::warn!("upstream 401 ({endpoint}); attempting inline token refresh + single replay");
    // Refresh against the EXACT token the failing request carried, not a fresh
    // re-read of state that may already have rotated under us.
    if !refresh(stale).await {
        // Refresh failed / not possible: surface the original 401 unchanged.
        return Ok(response);
    }

    metrics::counter!("copilot_token_401_replay_total", "endpoint" => endpoint).increment(1);
    // Replay EXACTLY once with the freshly-installed token; surface whatever it
    // returns (including another 401) without further retries.
    let fresh = read_token();
    send_with_retry(build(&fresh), endpoint, RetryPolicy::from_env()).await
}

/// Inline-401 wrapper for the Copilot HTTP call sites: force-refreshes the
/// Copilot token (coalesced) and replays once. `build` receives the exact token
/// the helper read from state and MUST stamp it into the request's auth header
/// (rather than re-snapshotting the token itself) so the refresh decision is made
/// against the token the request actually carried.
pub async fn send_copilot_with_401_retry<B>(
    endpoint: &'static str,
    build: B,
) -> reqwest::Result<reqwest::Response>
where
    B: Fn(&str) -> reqwest::RequestBuilder,
{
    send_with_401_retry_inner(
        endpoint,
        || state::with_state(|s| s.copilot_token.clone()).unwrap_or_default(),
        build,
        |stale| async move { force_refresh_copilot_token(&stale).await.is_ok() },
    )
    .await
}

async fn run_copilot_refresh_loop(refresh_in: i64, aborted: Arc<AtomicBool>) {
    COPILOT_REFRESH_GUARD.deadline_ms.store(
        get_refresh_deadline_ms(refresh_in, now_millis()),
        Ordering::SeqCst,
    );
    let mut retry_delay_ms = RETRY_REFRESH_DELAY_MS;

    while !aborted.load(Ordering::SeqCst) {
        // Re-read the shared deadline every iteration so an inline 401 refresh
        // (which bumps it) resyncs the loop instead of triggering a second one.
        let refresh_at_ms = COPILOT_REFRESH_GUARD.deadline_ms.load(Ordering::SeqCst);
        let next_delay_ms = get_refresh_poll_delay_ms(refresh_at_ms, now_millis());
        if next_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(next_delay_ms as u64)).await;
            continue;
        }

        tracing::debug!("Refreshing Copilot token");
        // Pass the current token as `stale`: when nothing changed under us the
        // guard performs the refresh; if an inline path already rotated it, the
        // call coalesces to a no-op and the bumped deadline ends the loop wait.
        let stale = state::with_state(|s| s.copilot_token.clone()).unwrap_or_default();
        match force_refresh_copilot_token(&stale).await {
            Ok(()) => {
                retry_delay_ms = RETRY_REFRESH_DELAY_MS;
            }
            Err(e) => {
                tracing::error!("Failed to refresh Copilot token: {e}");
                let jitter = (rand::random::<u64>() % RETRY_REFRESH_JITTER_MS as u64) as i64;
                let delay_ms = std::cmp::min(retry_delay_ms + jitter, MAX_RETRY_REFRESH_DELAY_MS);
                let backoff_at_ms = now_millis() + delay_ms;
                COPILOT_REFRESH_GUARD
                    .deadline_ms
                    .store(backoff_at_ms, Ordering::SeqCst);
                retry_delay_ms = std::cmp::min(retry_delay_ms * 2, MAX_RETRY_REFRESH_DELAY_MS);
                record_refresh_deadline("copilot", backoff_at_ms);
                tracing::warn!("Retrying Copilot token refresh in {}s", delay_ms / 1000);
            }
        }
    }
}

async fn run_codex_refresh_loop(aborted: Arc<AtomicBool>) {
    CODEX_REFRESH_GUARD.deadline_ms.store(
        std::cmp::max(
            state::with_state(|s| s.codex_expires_at.unwrap_or_else(now_millis))
                - EARLY_REFRESH_BUFFER_MS,
            now_millis(),
        ),
        Ordering::SeqCst,
    );
    let mut retry_delay_ms = RETRY_REFRESH_DELAY_MS;

    while !aborted.load(Ordering::SeqCst) {
        let (expires_at, refresh_token) =
            state::with_state(|s| (s.codex_expires_at, s.codex_refresh_token.clone()));
        if !matches!((expires_at, &refresh_token), (Some(_), Some(_))) {
            return;
        }

        let refresh_at_ms = CODEX_REFRESH_GUARD.deadline_ms.load(Ordering::SeqCst);
        let next_delay_ms = get_refresh_poll_delay_ms(refresh_at_ms, now_millis());
        if next_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(next_delay_ms as u64)).await;
            continue;
        }

        tracing::debug!("Refreshing Codex credentials");
        let stale = state::with_state(|s| s.codex_access_token.clone()).unwrap_or_default();
        match force_refresh_codex_token(&stale).await {
            Ok(()) => {
                retry_delay_ms = RETRY_REFRESH_DELAY_MS;
            }
            Err(e) => {
                tracing::error!("Failed to refresh Codex credentials: {e}");
                let jitter = (rand::random::<u64>() % RETRY_REFRESH_JITTER_MS as u64) as i64;
                let delay_ms = std::cmp::min(retry_delay_ms + jitter, MAX_RETRY_REFRESH_DELAY_MS);
                let backoff_at_ms = now_millis() + delay_ms;
                CODEX_REFRESH_GUARD
                    .deadline_ms
                    .store(backoff_at_ms, Ordering::SeqCst);
                retry_delay_ms = std::cmp::min(retry_delay_ms * 2, MAX_RETRY_REFRESH_DELAY_MS);
                record_refresh_deadline("codex", backoff_at_ms);
                tracing::warn!("Retrying Codex token refresh in {}s", delay_ms / 1000);
            }
        }
    }
}

// --- GitHub token setup -----------------------------------------------------

pub async fn setup_github_token(force: bool) -> Result<(), anyhow::Error> {
    let result = setup_github_token_inner(force).await;
    if let Err(ref e) = result {
        if let Some(http) = e.downcast_ref::<HttpError>() {
            tracing::error!("Failed to get GitHub token: {}", http.body);
        } else {
            tracing::error!("Failed to get GitHub token: {e}");
        }
    }
    result
}

async fn setup_github_token_inner(force: bool) -> Result<(), anyhow::Error> {
    let github_token = read_github_token().await?;

    if let Some(token) = github_token {
        if !force {
            state::with_state_mut(|s| s.github_token = Some(token.clone()));
            if state::with_state(|s| s.show_token) {
                tracing::info!("GitHub token: {token}");
            }
            log_user().await?;
            return Ok(());
        }
    }

    tracing::info!("Not logged in, getting new access token");
    let response = get_device_code().await?;
    tracing::debug!("Device code response: {:?}", response);

    tracing::info!(
        "Please enter the code \"{}\" in {}",
        response.user_code,
        response.verification_uri
    );
    // Best-effort: open the verification page so the user doesn't have to copy
    // the URL by hand. Failures (no browser, headless/Docker, SSH) are non-fatal
    // — the code + URL were already logged above.
    if webbrowser::open(&response.verification_uri).is_err() {
        tracing::debug!("Could not open a browser automatically; open the URL above manually.");
    }

    let token = poll_access_token(&response).await?;
    write_github_token(&token).await?;
    state::with_state_mut(|s| s.github_token = Some(token.clone()));

    if state::with_state(|s| s.show_token) {
        tracing::info!("GitHub token: {token}");
    }
    log_user().await?;
    Ok(())
}

pub async fn log_user() -> Result<(), anyhow::Error> {
    let snapshot = state::snapshot();
    let user = get_github_user(&snapshot, None).await?;
    state::with_state_mut(|s| s.user_name = Some(user.login.clone()));
    tracing::info!("Logged in as {}", user.login);

    let snapshot = state::snapshot();
    let copilot_user = get_copilot_usage(&snapshot, None).await?;
    state::with_state_mut(|s| {
        s.copilot_api_url = copilot_user.endpoints.as_ref().and_then(|e| e.api.clone());
        s.token_based_billing = copilot_user.token_based_billing;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn is_unauthorized_only_matches_401() {
        assert!(is_unauthorized(401));
        for code in [200u16, 400, 403, 404, 429, 500, 502, 503] {
            assert!(!is_unauthorized(code), "{code} must not be 401");
        }
    }

    #[test]
    fn refresh_already_done_detects_rotation() {
        // Same token still installed: an inline refresh is still required.
        assert!(!refresh_already_done("tok-a", "tok-a"));
        // Token rotated out from under the caller: coalesce to a no-op.
        assert!(refresh_already_done("tok-a", "tok-b"));
        // Current token not loaded (empty): treat as needing a refresh, not done.
        assert!(!refresh_already_done("tok-a", ""));
        // Caller's request had no token but a real one is present now.
        assert!(refresh_already_done("", "tok-b"));
    }

    /// Spawn a throwaway localhost HTTP server whose Nth request gets the status
    /// `status_for(n)` returns. Counts requests; each response closes its
    /// connection so the request count equals the connection count.
    async fn spawn_status_server(status_for: fn(usize) -> u16) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();
        tokio::spawn(async move {
            loop {
                let mut sock = match listener.accept().await {
                    Ok((s, _)) => s,
                    Err(_) => break,
                };
                let nth = count_clone.fetch_add(1, Ordering::SeqCst);
                let status = status_for(nth);
                // Drain the (tiny) request before replying so the client's write
                // side doesn't see a reset.
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let reason = if status == 401 { "Unauthorized" } else { "OK" };
                let body = "{}";
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{addr}/"), count)
    }

    #[tokio::test]
    async fn replays_once_after_successful_refresh() {
        // 401 then 200: a successful refresh drives exactly one replay -> 200.
        let (url, count) = spawn_status_server(|n| if n == 0 { 401 } else { 200 }).await;
        let build = |_token: &str| {
            crate::libs::http::client()
                .post(&url)
                .body(Vec::<u8>::new())
        };
        let refreshed = Arc::new(AtomicBool::new(false));
        let refreshed_clone = refreshed.clone();
        let response =
            send_with_401_retry_inner("chat", String::new, build, move |_stale| async move {
                refreshed_clone.store(true, Ordering::SeqCst);
                true
            })
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert!(refreshed.load(Ordering::SeqCst), "refresh runs on a 401");
        assert_eq!(count.load(Ordering::SeqCst), 2, "exactly one replay");
    }

    #[tokio::test]
    async fn no_replay_when_refresh_fails() {
        // 401 first: a failed refresh surfaces the original 401 with no replay.
        let (url, count) = spawn_status_server(|n| if n == 0 { 401 } else { 200 }).await;
        let build = |_token: &str| {
            crate::libs::http::client()
                .post(&url)
                .body(Vec::<u8>::new())
        };
        let response =
            send_with_401_retry_inner("chat", String::new, build, |_stale| async { false })
                .await
                .unwrap();
        assert_eq!(response.status().as_u16(), 401);
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "no replay when the refresh fails"
        );
    }

    #[tokio::test]
    async fn replay_surfaces_second_401() {
        // Always 401: refresh succeeds but the single replay also 401s, which is
        // surfaced unchanged — the replay never repeats.
        let (url, count) = spawn_status_server(|_| 401).await;
        let build = |_token: &str| {
            crate::libs::http::client()
                .post(&url)
                .body(Vec::<u8>::new())
        };
        let response =
            send_with_401_retry_inner("chat", String::new, build, |_stale| async { true })
                .await
                .unwrap();
        assert_eq!(response.status().as_u16(), 401);
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "the replay happens at most once"
        );
    }

    #[tokio::test]
    async fn no_refresh_on_initial_success() {
        // A 200 first response never triggers a refresh or replay.
        let (url, count) = spawn_status_server(|_| 200).await;
        let build = |_token: &str| {
            crate::libs::http::client()
                .post(&url)
                .body(Vec::<u8>::new())
        };
        let refreshed = Arc::new(AtomicBool::new(false));
        let refreshed_clone = refreshed.clone();
        let response =
            send_with_401_retry_inner("chat", String::new, build, move |_stale| async move {
                refreshed_clone.store(true, Ordering::SeqCst);
                true
            })
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert!(!refreshed.load(Ordering::SeqCst), "no refresh on success");
        assert_eq!(count.load(Ordering::SeqCst), 1, "no replay on success");
    }

    #[tokio::test]
    async fn refresh_decides_against_token_request_carried() {
        // TOCTOU regression guard. Even if the global token rotates between the
        // helper's read and the request build, the value `build` stamps into the
        // first request is the SAME value handed to `refresh` — so the refresh
        // decision can never be made against a token the request didn't carry.
        // The replay then re-reads state and picks up the rotated/refreshed token.
        let (url, count) = spawn_status_server(|n| if n == 0 { 401 } else { 200 }).await;

        // read_token yields "tok-a" first (first request), "tok-b" on the replay
        // re-read — simulating a rotation that landed before the replay.
        let reads = Arc::new(AtomicUsize::new(0));
        let reads_clone = reads.clone();
        let read_token = move || {
            if reads_clone.fetch_add(1, Ordering::SeqCst) == 0 {
                "tok-a".to_string()
            } else {
                "tok-b".to_string()
            }
        };

        // Record every token `build` is asked to stamp into the request.
        let built_with: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let built_with_clone = built_with.clone();
        let build = move |token: &str| {
            built_with_clone.lock().unwrap().push(token.to_string());
            crate::libs::http::client()
                .post(&url)
                .body(Vec::<u8>::new())
        };

        // Record the token the refresh decision was made against.
        let refreshed_with: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let refreshed_with_clone = refreshed_with.clone();
        let refresh = move |stale: String| async move {
            *refreshed_with_clone.lock().unwrap() = Some(stale);
            true
        };

        let response = send_with_401_retry_inner("chat", read_token, build, refresh)
            .await
            .unwrap();

        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(count.load(Ordering::SeqCst), 2, "exactly one replay");
        // The first request carried "tok-a"; the replay carried the re-read "tok-b".
        assert_eq!(
            built_with.lock().unwrap().as_slice(),
            ["tok-a".to_string(), "tok-b".to_string()],
        );
        // Crucially: refresh was decided against the exact token request #1 used.
        assert_eq!(
            refreshed_with.lock().unwrap().as_deref(),
            Some("tok-a"),
            "refresh must target the token the failing request actually carried",
        );
    }
}
