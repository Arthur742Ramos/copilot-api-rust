use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Mirrors src/lib/oauth/codex.ts. OpenAI Codex OAuth (PKCE) login + refresh.

pub const CODEX_API_BASE_URL: &str = "https://chatgpt.com/backend-api";

const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const REFRESH_BUFFER_MS: i64 = 60_000;
const CALLBACK_TIMEOUT_MS: u64 = 180_000;

fn redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexCredentials {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    #[serde(rename = "accountId")]
    pub account_id: String,
}

pub struct CodexAuthInfo {
    pub url: String,
    pub instructions: Option<String>,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct TokenSuccessResult {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
}

#[derive(Deserialize)]
struct TokenResponsePayload {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<f64>,
}

fn generate_pkce() -> (String, String) {
    let mut verifier_bytes = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut verifier_bytes[..]);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());
    (verifier, challenge)
}

fn create_state() -> String {
    let mut bytes = [0u8; 16];
    rand::Rng::fill(&mut rand::thread_rng(), &mut bytes[..]);
    hex::encode(bytes)
}

fn decode_jwt(access_token: &str) -> Option<serde_json::Value> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn get_account_id(access_token: &str) -> Option<String> {
    let payload = decode_jwt(access_token)?;
    let auth_payload = payload.get(JWT_CLAIM_PATH)?;
    let account_id = auth_payload.get("chatgpt_account_id")?.as_str()?;
    if account_id.is_empty() {
        None
    } else {
        Some(account_id.to_string())
    }
}

async fn post_token_form(params: &[(&str, &str)]) -> Result<TokenSuccessResult, anyhow::Error> {
    let response = crate::libs::http::client()
        .post(TOKEN_URL)
        .form(params)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let details = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Codex token request failed ({status}): {details}"
        ));
    }

    let payload: TokenResponsePayload = response.json().await?;
    match (
        payload.access_token,
        payload.refresh_token,
        payload.expires_in,
    ) {
        (Some(access_token), Some(refresh_token), Some(expires_in)) => Ok(TokenSuccessResult {
            access_token,
            refresh_token,
            expires_at: now_millis() + (expires_in as i64) * 1000,
        }),
        _ => Err(anyhow::anyhow!("Codex token response missing fields")),
    }
}

async fn exchange_authorization_code(
    code: &str,
    verifier: &str,
) -> Result<TokenSuccessResult, anyhow::Error> {
    let redirect = redirect_uri();
    post_token_form(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", &redirect),
    ])
    .await
}

async fn refresh_access_token(refresh_token: &str) -> Result<TokenSuccessResult, anyhow::Error> {
    post_token_form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ])
    .await
}

fn create_authorization_flow() -> (String, String, String) {
    let (verifier, challenge) = generate_pkce();
    let state = create_state();
    let redirect = redirect_uri();
    let mut url = url::Url::parse(AUTHORIZE_URL).expect("valid authorize url");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "copilot-api");
    (verifier, state, url.to_string())
}

const SUCCESS_BODY: &str = "OpenAI Codex authentication completed. You can close this window.";

fn http_response(status_line: &str, body_message: &str) -> String {
    let body = render_oauth_page(body_message);
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn render_oauth_page(message: &str) -> String {
    format!(
        "<!doctype html><html><body><p>{}</p></body></html>",
        escape_html(message)
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Run a one-shot local HTTP server on the callback port, returning the captured
/// authorization `code` once the browser redirects (or None on timeout/error).
///
/// Binds the callback on BOTH `127.0.0.1` and `[::1]`: the redirect URL OpenAI
/// sends the browser to uses `localhost`, which on many systems (notably
/// Windows) resolves to IPv6 `::1` first. Binding only IPv4 there would leave the
/// browser knocking on a dead `[::1]:1455` and the user stuck on a "connection
/// refused" page, so we listen on both stacks and take whichever fires.
async fn wait_for_authorization_code(state: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Try both loopback families; succeed if at least one binds.
    let v4 = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .ok();
    let v6 = tokio::net::TcpListener::bind(("::1", CALLBACK_PORT))
        .await
        .ok();
    if v4.is_none() && v6.is_none() {
        return None;
    }

    // Handle one accepted connection: parse the callback, write the response,
    // and return Some(code) once the matching-state code arrives.
    async fn handle_conn(socket: &mut tokio::net::TcpStream, state: &str) -> Option<String> {
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.ok()?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let request_line = request.lines().next().unwrap_or("");
        // "GET /auth/callback?code=...&state=... HTTP/1.1"
        let target = request_line.split_whitespace().nth(1).unwrap_or("");

        let full = format!("http://localhost{target}");
        let parsed = url::Url::parse(&full).ok();

        let (status_line, message, code) = match parsed {
            Some(u) if u.path() == CALLBACK_PATH => {
                let q_state = u
                    .query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.to_string());
                let q_code = u
                    .query_pairs()
                    .find(|(k, _)| k == "code")
                    .map(|(_, v)| v.to_string());
                if q_state.as_deref() != Some(state) {
                    ("400 Bad Request", "State mismatch.".to_string(), None)
                } else if let Some(code) = q_code {
                    ("200 OK", SUCCESS_BODY.to_string(), Some(code))
                } else {
                    (
                        "400 Bad Request",
                        "Missing authorization code.".to_string(),
                        None,
                    )
                }
            }
            _ => (
                "404 Not Found",
                "Callback route not found.".to_string(),
                None,
            ),
        };

        let _ = socket
            .write_all(http_response(status_line, &message).as_bytes())
            .await;
        let _ = socket.flush().await;
        code
    }

    // Accept on a single listener until a valid code arrives (skipping
    // favicon/preflight/non-matching hits).
    async fn accept_on(listener: tokio::net::TcpListener, state: &str) -> Option<String> {
        loop {
            // A transient accept() error (ECONNABORTED, EMFILE, ...) must not end
            // this branch: under the select! below, returning None would cancel
            // the healthy peer listener and drop the whole callback to manual
            // paste. Skip the error and keep listening instead.
            let mut socket = match listener.accept().await {
                Ok((socket, _)) => socket,
                Err(e) => {
                    tracing::debug!("OAuth callback accept error (continuing): {e}");
                    continue;
                }
            };
            if let Some(code) = handle_conn(&mut socket, state).await {
                return Some(code);
            }
        }
    }

    let accept_loop = async {
        match (v4, v6) {
            (Some(l4), Some(l6)) => {
                tokio::select! {
                    r = accept_on(l4, state) => r,
                    r = accept_on(l6, state) => r,
                }
            }
            (Some(l4), None) => accept_on(l4, state).await,
            (None, Some(l6)) => accept_on(l6, state).await,
            (None, None) => None,
        }
    };

    tokio::time::timeout(Duration::from_millis(CALLBACK_TIMEOUT_MS), accept_loop)
        .await
        .unwrap_or_default()
}

fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Ok(u) = url::Url::parse(value) {
        let code = u
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string());
        let state = u
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.to_string());
        return (code, state);
    }
    if value.contains('#') {
        let mut parts = value.splitn(2, '#');
        let code = parts.next().map(|s| s.to_string());
        let state = parts.next().map(|s| s.to_string());
        return (code, state);
    }
    if value.contains("code=") {
        let params = url::form_urlencoded::parse(value.as_bytes());
        let mut code = None;
        let mut state = None;
        for (k, v) in params {
            if k == "code" {
                code = Some(v.to_string());
            } else if k == "state" {
                state = Some(v.to_string());
            }
        }
        return (code, state);
    }
    (Some(value.to_string()), None)
}

/// Interactive Codex login. `on_auth` surfaces the URL to the user; `prompt`
/// reads a pasted code/URL if the local callback server doesn't capture it.
pub async fn login_codex<P, Fut>(
    on_auth: impl FnOnce(CodexAuthInfo),
    prompt: P,
) -> Result<CodexCredentials, anyhow::Error>
where
    P: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = String>,
{
    let (verifier, state, url) = create_authorization_flow();
    on_auth(CodexAuthInfo {
        url,
        instructions: Some(
            "Please complete the login in the browser. If the browser does not automatically redirect, please paste the callback URL or code back to the terminal."
                .to_string(),
        ),
    });
    tracing::debug!("Waiting for Codex OAuth callback");

    let mut code = wait_for_authorization_code(&state).await;
    if code.is_none() {
        let input = prompt("Paste the authorization code or full redirect URL:".to_string()).await;
        let (parsed_code, parsed_state) = parse_authorization_input(&input);
        if let Some(s) = parsed_state {
            if s != state {
                return Err(anyhow::anyhow!("Codex OAuth state mismatch"));
            }
        }
        code = parsed_code;
    }

    let code = code.ok_or_else(|| anyhow::anyhow!("Missing Codex authorization code"))?;
    let token_result = exchange_authorization_code(&code, &verifier).await?;
    let account_id = get_account_id(&token_result.access_token)
        .ok_or_else(|| anyhow::anyhow!("Failed to extract Codex account id from access token"))?;

    Ok(CodexCredentials {
        access_token: token_result.access_token,
        refresh_token: token_result.refresh_token,
        expires_at: token_result.expires_at,
        account_id,
    })
}

pub async fn refresh_codex_credentials(
    credentials: &CodexCredentials,
) -> Result<CodexCredentials, anyhow::Error> {
    let token_result = refresh_access_token(&credentials.refresh_token).await?;
    let account_id = get_account_id(&token_result.access_token)
        .ok_or_else(|| anyhow::anyhow!("Failed to extract Codex account id from access token"))?;

    Ok(CodexCredentials {
        access_token: token_result.access_token,
        refresh_token: token_result.refresh_token,
        expires_at: token_result.expires_at,
        account_id,
    })
}

pub fn is_codex_credentials_expired(expires_at: i64, now: Option<i64>) -> bool {
    let now = now.unwrap_or_else(now_millis);
    expires_at <= now + REFRESH_BUFFER_MS
}
