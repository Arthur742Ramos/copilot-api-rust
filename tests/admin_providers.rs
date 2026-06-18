//! Covers the admin provider + reload routes added alongside model-mappings:
//! auth is enforced, a provider upsert round-trips with its apiKey redacted to
//! `apiKeySet`, the reserved `copilot` name is rejected, and reload returns a
//! redacted summary.
//!
//! These routes write config.json to disk, so we isolate this test binary by
//! pointing `COPILOT_API_HOME` at a temp dir BEFORE anything touches the `PATHS`
//! lazy static. Auth is satisfied via the cached-config seam. `#[serial]`
//! because both the cached config and the on-disk config are global.

mod common;

use std::sync::Once;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{json_body, send, set_config};
use serde_json::json;

const ADMIN_KEY: &str = "admin-key";

static INIT_HOME: Once = Once::new();

fn init_home() {
    INIT_HOME.call_once(|| {
        let dir = std::env::temp_dir().join(format!(
            "copilot-api-itest-providers-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Seed config.json on disk with the admin key so reload_config() (which
        // the provider/reload handlers call) preserves it — unlike the in-memory
        // test seam, which a reload would discard.
        std::fs::write(
            dir.join("config.json"),
            json!({ "auth": { "adminApiKey": ADMIN_KEY } }).to_string(),
        )
        .unwrap();
        std::env::set_var("COPILOT_API_HOME", &dir);
    });
}

fn admin_post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn providers_get_requires_admin_key() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let request = Request::builder()
        .method(Method::GET)
        .uri("/admin/config/providers")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[serial_test::serial]
async fn provider_upsert_roundtrips_with_redacted_key() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let request = admin_post(
        "/admin/config/providers",
        json!({
            "name": "acme",
            "config": {
                "type": "openai-compatible",
                "baseUrl": "https://acme.example.com",
                "apiKey": "super-secret-key",
            }
        }),
    );
    let (status, body) = send(request).await;
    assert_eq!(status, StatusCode::OK);
    let value = json_body(&body);
    assert_eq!(value["provider"]["name"], "acme");
    assert_eq!(value["provider"]["apiKeySet"], true);
    // The raw secret must never be echoed back.
    assert!(!body_contains(&body, "super-secret-key"));

    // It must then appear in the redacted GET listing.
    let get = Request::builder()
        .method(Method::GET)
        .uri("/admin/config/providers")
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(get).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body_contains(&body, "super-secret-key"));
    let value = json_body(&body);
    let providers = value["providers"].as_array().unwrap();
    assert!(providers.iter().any(|p| p["name"] == "acme"));
}

#[tokio::test]
#[serial_test::serial]
async fn reserved_provider_name_is_rejected() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let request = admin_post(
        "/admin/config/providers",
        json!({
            "name": "copilot",
            "config": { "type": "openai-compatible", "baseUrl": "https://x.example.com" }
        }),
    );
    let (status, _) = send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial_test::serial]
async fn reload_returns_summary() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let (status, body) = send(admin_post("/admin/config/reload", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let value = json_body(&body);
    assert_eq!(value["reloaded"], true);
    assert!(value.get("configPath").is_some());
    assert!(value.get("providers").is_some());
}

#[tokio::test]
#[serial_test::serial]
async fn effective_config_get_requires_admin_key() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let request = Request::builder()
        .method(Method::GET)
        .uri("/admin/config")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[serial_test::serial]
async fn effective_config_redacts_secrets() {
    init_home();
    // Install a cached config carrying secrets via the test seam; the GET reads
    // get_config() and must never echo the raw secret values.
    use copilot_api::libs::config::{set_cached_config_for_test, AppConfig, AuthConfig};
    set_cached_config_for_test(AppConfig {
        auth: Some(AuthConfig {
            api_keys: Some(vec![json!("sk-secret-1"), json!("sk-secret-2")]),
            admin_api_key: Some(json!(ADMIN_KEY)),
        }),
        anthropic_api_key: Some("sk-ant-secret".to_string()),
        daily_token_budget: Some(1234),
        ..Default::default()
    });

    let request = Request::builder()
        .method(Method::GET)
        .uri("/admin/config")
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(request).await;
    assert_eq!(status, StatusCode::OK);

    // No raw secret may appear anywhere in the response.
    assert!(!body_contains(&body, ADMIN_KEY));
    assert!(!body_contains(&body, "sk-secret-1"));
    assert!(!body_contains(&body, "sk-ant-secret"));

    let value = json_body(&body);
    let cfg = &value["config"];
    // Non-secret knobs are visible in the config object.
    assert_eq!(cfg["dailyTokenBudget"], 1234);
    // Raw secret keys are stripped from the config object entirely.
    assert!(cfg["auth"].get("adminApiKey").is_none());
    assert!(cfg["auth"].get("apiKeys").is_none());
    assert!(cfg.get("anthropicApiKey").is_none());
    // Presence indicators live under a separate `secrets` object (not inline in
    // config, to avoid colliding with serde-flatten extra keys).
    let secrets = &value["secrets"];
    assert_eq!(secrets["adminApiKeySet"], true);
    assert_eq!(secrets["apiKeysCount"], 2);
    assert_eq!(secrets["anthropicApiKeySet"], true);
}

fn body_contains(body: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(body).contains(needle)
}
