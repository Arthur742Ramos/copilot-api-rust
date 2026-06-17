use std::sync::atomic::{AtomicBool, Ordering};
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

pub fn get_refresh_deadline_ms(refresh_in: i64, now_ms: i64) -> i64 {
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

async fn run_copilot_refresh_loop(refresh_in: i64, aborted: Arc<AtomicBool>) {
    let mut refresh_at_ms = get_refresh_deadline_ms(refresh_in, now_millis());
    let mut retry_delay_ms = RETRY_REFRESH_DELAY_MS;

    while !aborted.load(Ordering::SeqCst) {
        let next_delay_ms = get_refresh_poll_delay_ms(refresh_at_ms, now_millis());
        if next_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(next_delay_ms as u64)).await;
            continue;
        }

        tracing::debug!("Refreshing Copilot token");
        let snapshot = state::snapshot();
        match get_copilot_token(&snapshot).await {
            Ok(resp) => {
                state::with_state_mut(|s| s.copilot_token = Some(resp.token.clone()));
                refresh_at_ms = get_refresh_deadline_ms(resp.refresh_in, now_millis());
                retry_delay_ms = RETRY_REFRESH_DELAY_MS;
                record_refresh_result("copilot", true);
                record_refresh_deadline("copilot", refresh_at_ms);
                tracing::debug!("Copilot token refreshed");
                if state::with_state(|s| s.show_token) {
                    tracing::info!("Refreshed Copilot token: {}", resp.token);
                }
            }
            Err(e) => {
                tracing::error!("Failed to refresh Copilot token: {e}");
                let jitter = (rand::random::<u64>() % RETRY_REFRESH_JITTER_MS as u64) as i64;
                let delay_ms = std::cmp::min(retry_delay_ms + jitter, MAX_RETRY_REFRESH_DELAY_MS);
                refresh_at_ms = now_millis() + delay_ms;
                retry_delay_ms = std::cmp::min(retry_delay_ms * 2, MAX_RETRY_REFRESH_DELAY_MS);
                record_refresh_result("copilot", false);
                record_refresh_deadline("copilot", refresh_at_ms);
                tracing::warn!("Retrying Copilot token refresh in {}s", delay_ms / 1000);
            }
        }
    }
}

async fn run_codex_refresh_loop(aborted: Arc<AtomicBool>) {
    let mut refresh_at_ms = std::cmp::max(
        state::with_state(|s| s.codex_expires_at.unwrap_or_else(now_millis))
            - EARLY_REFRESH_BUFFER_MS,
        now_millis(),
    );

    while !aborted.load(Ordering::SeqCst) {
        let (expires_at, refresh_token) =
            state::with_state(|s| (s.codex_expires_at, s.codex_refresh_token.clone()));
        let (expires_at, refresh_token) = match (expires_at, refresh_token) {
            (Some(e), Some(r)) => (e, r),
            _ => return,
        };

        let next_delay_ms = get_refresh_poll_delay_ms(refresh_at_ms, now_millis());
        if next_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(next_delay_ms as u64)).await;
            continue;
        }

        tracing::debug!("Refreshing Codex credentials");
        let current = state::with_state(|s| CodexCredentials {
            access_token: s.codex_access_token.clone().unwrap_or_default(),
            refresh_token: refresh_token.clone(),
            expires_at,
            account_id: s.codex_account_id.clone().unwrap_or_default(),
        });

        match refresh_codex_credentials(&current).await {
            Ok(credentials) => {
                if let Err(e) = persist_codex_credentials(&credentials, false).await {
                    tracing::error!("Failed to persist refreshed Codex credentials: {e}");
                }
                refresh_at_ms = std::cmp::max(
                    credentials.expires_at - EARLY_REFRESH_BUFFER_MS,
                    now_millis(),
                );
                record_refresh_result("codex", true);
                record_refresh_deadline("codex", refresh_at_ms);
                tracing::debug!("Codex credentials refreshed");
            }
            Err(e) => {
                tracing::error!("Failed to refresh Codex credentials: {e}");
                refresh_at_ms = now_millis() + RETRY_REFRESH_DELAY_MS;
                record_refresh_result("codex", false);
                record_refresh_deadline("codex", refresh_at_ms);
                tracing::warn!(
                    "Retrying Codex token refresh in {}s",
                    RETRY_REFRESH_DELAY_MS / 1000
                );
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
