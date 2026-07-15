//! Codex setup failures must remain internal/setup failures, never masquerade as
//! provider-not-found responses.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{json_body, send};
use copilot_api::libs::config::{reload_config, reset_cached_config_for_test};
use copilot_api::libs::state;
use serde_json::json;

#[tokio::test]
#[serial_test::serial]
async fn malformed_codex_credentials_surface_as_openai_500_on_alpha_routes() {
    let home = std::env::temp_dir().join(format!(
        "copilot-api-codex-resolver-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("COPILOT_API_HOME", &home);
    std::fs::write(
        home.join("config.json"),
        json!({
            "auth":{"apiKeys":[]},
            "providers":{
                "codex":{
                    "type":"openai-responses",
                    "enabled":true,
                    "baseUrl":"https://chatgpt.com/backend-api",
                    "authType":"oauth2"
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(home.join("codex_credentials.json"), "{malformed").unwrap();
    copilot_api::libs::paths::ensure_paths().await.unwrap();
    reload_config().unwrap();
    state::with_state_mut(|state| {
        state.codex_access_token = None;
        state.codex_refresh_token = None;
        state.codex_account_id = None;
        state.codex_expires_at = None;
    });

    for path in [
        "/alpha/search",
        "/v1/alpha/search",
        "/codex/v1/alpha/search",
    ] {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"credential error"}"#))
            .unwrap();
        let (status, body) = send(request).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}");
        let body = json_body(&body);
        assert_eq!(body["error"]["type"], "server_error");
        assert_ne!(body["error"]["code"], "provider_not_found");
        assert!(!body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not found or disabled"));
        assert!(!body.to_string().contains("malformed"));
    }

    reset_cached_config_for_test();
    std::env::remove_var("COPILOT_API_HOME");
    let _ = std::fs::remove_dir_all(home);
}
