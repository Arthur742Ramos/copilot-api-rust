//! Ports config-route.test.ts intent: the admin model-mappings route. GET with a
//! valid admin key returns 200 JSON; without it returns 401; POST round-trips a
//! mapping.
//!
//! `set_model_mappings` (POST) writes config.json to disk, so we isolate this
//! test binary by pointing `COPILOT_API_HOME` at a temp dir BEFORE anything
//! touches the `PATHS` lazy static. Auth is satisfied via the cached-config seam.
//! `#[serial]` because both the cached config and the on-disk config are global.

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
        let dir =
            std::env::temp_dir().join(format!("copilot-api-itest-admin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("COPILOT_API_HOME", &dir);
    });
}

fn admin_get() -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri("/admin/config/model-mappings")
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn get_with_admin_key_returns_200_json() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let (status, body) = send(admin_get()).await;
    assert_eq!(status, StatusCode::OK);
    let value = json_body(&body);
    assert!(value.get("configPath").is_some());
    assert!(value.get("modelMappings").is_some());
}

#[tokio::test]
#[serial_test::serial]
async fn get_without_admin_key_returns_401() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let request = Request::builder()
        .method(Method::GET)
        .uri("/admin/config/model-mappings")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[serial_test::serial]
async fn post_roundtrips_a_mapping() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let request = Request::builder()
        .method(Method::POST)
        .uri("/admin/config/model-mappings")
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "modelMappings": { "source-model": "target-model" } }).to_string(),
        ))
        .unwrap();

    let (status, body) = send(request).await;
    assert_eq!(status, StatusCode::OK);
    let value = json_body(&body);
    assert_eq!(value["modelMappings"]["source-model"], "target-model");
}

#[tokio::test]
#[serial_test::serial]
async fn post_with_invalid_shape_returns_400() {
    init_home();
    set_config(&[], Some(ADMIN_KEY));

    let request = Request::builder()
        .method(Method::POST)
        .uri("/admin/config/model-mappings")
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "wrong": "shape" }).to_string()))
        .unwrap();

    let (status, _) = send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
