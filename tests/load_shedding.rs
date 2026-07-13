//! Concurrency-admission regression tests. These use tiny limits and synthetic
//! response bodies so no test contacts GitHub or any configured provider.

mod common;

use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, Response, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use copilot_api::libs::admission::{admission_middleware, AdmissionController};
use futures_util::future::join_all;
use http_body_util::BodyExt;
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;

fn limited(limit: usize) -> AdmissionController {
    AdmissionController::limited(NonZeroUsize::new(limit).expect("test limit is non-zero"))
}

fn request(method: Method, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("valid request")
}

async fn assert_overloaded(response: Response<Body>, expect_body: bool, path: &str) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    let body = response
        .into_body()
        .collect()
        .await
        .expect("overload body")
        .to_bytes();
    if !expect_body {
        assert!(body.is_empty(), "HEAD responses must not include a body");
        return;
    }
    let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON overload body");
    if copilot_api::libs::error::is_openai_native_path(path) {
        assert!(value.get("type").is_none());
        assert_eq!(value["error"]["type"], "server_error");
        assert_eq!(value["error"]["code"], "server_overloaded");
    } else {
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], "overloaded_error");
    }
}

fn guarded_test_router(controller: AdmissionController, router: Router) -> Router {
    router.route_layer(from_fn_with_state(controller, admission_middleware))
}

#[tokio::test]
#[serial_test::serial]
async fn unlimited_remains_the_default() {
    let controller = AdmissionController::default();
    assert_eq!(controller.limit(), None);

    let permits: Vec<_> = (0..126)
        .map(|_| controller.try_acquire().expect("unlimited admission"))
        .collect();
    assert_eq!(controller.current(), 126);
    drop(permits);
    assert_eq!(controller.current(), 0);
}

#[tokio::test]
#[serial_test::serial]
async fn configured_overload_covers_upstream_routes_but_not_control_plane() {
    common::set_config(&[], None);
    let controller = limited(1);
    let held = controller.try_acquire().expect("occupy only slot");
    let app = copilot_api::server::build_router_with_admission(controller.clone());

    let guarded_routes = [
        (Method::POST, "/chat/completions"),
        (Method::POST, "/v1/chat/completions"),
        (Method::GET, "/models"),
        (Method::GET, "/v1/models"),
        (Method::GET, "/models/test"),
        (Method::GET, "/v1/models/test"),
        (Method::HEAD, "/models"),
        (Method::HEAD, "/v1/models"),
        (Method::HEAD, "/models/test"),
        (Method::HEAD, "/v1/models/test"),
        (Method::POST, "/embeddings"),
        (Method::POST, "/v1/embeddings"),
        (Method::POST, "/images/generations"),
        (Method::POST, "/v1/images/generations"),
        (Method::POST, "/responses"),
        (Method::POST, "/v1/responses"),
        (Method::POST, "/responses/compact"),
        (Method::POST, "/v1/responses/compact"),
        (Method::POST, "/v1/messages"),
        (Method::POST, "/v1/messages/count_tokens"),
        (Method::POST, "/test-provider/v1/messages"),
        (Method::POST, "/test-provider/v1/messages/count_tokens"),
        (Method::GET, "/test-provider/v1/models"),
        (Method::HEAD, "/test-provider/v1/models"),
    ];
    for (method, path) in guarded_routes {
        let expect_body = method != Method::HEAD;
        let response = app
            .clone()
            .oneshot(request(method, path))
            .await
            .expect("router response");
        assert_overloaded(response, expect_body, path).await;
    }

    for path in ["/", "/version", "/readyz", "/admin/config"] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path))
            .await
            .expect("control response");
        assert_ne!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("1"),
            "{path} must bypass upstream admission"
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("control body")
            .to_bytes();
        assert!(
            !String::from_utf8_lossy(&body).contains("overloaded_error"),
            "{path} returned an overload body"
        );
    }

    let metrics = app
        .clone()
        .oneshot(request(Method::GET, "/metrics"))
        .await
        .expect("metrics response");
    assert_eq!(metrics.status(), StatusCode::OK);
    let metrics = metrics
        .into_body()
        .collect()
        .await
        .expect("metrics body")
        .to_bytes();
    let metrics = String::from_utf8_lossy(&metrics);
    assert!(metrics.contains("proxy_upstream_concurrency_limit 1"));
    assert!(metrics.contains("proxy_upstream_requests_active 1"));
    assert!(metrics.contains("proxy_upstream_overload_rejections_total"));

    drop(held);
    assert_eq!(controller.current(), 0);
}

async fn controlled_stream(State(release): State<Arc<tokio::sync::Semaphore>>) -> Response<Body> {
    let stream = async_stream::stream! {
        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"first"));
        let _release = release.acquire().await.expect("release semaphore open");
        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"last"));
    };
    Response::new(Body::from_stream(stream))
}

#[tokio::test]
#[serial_test::serial]
async fn streaming_permit_is_held_until_body_completion() {
    let controller = limited(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let app = guarded_test_router(
        controller.clone(),
        Router::new()
            .route("/work", get(controlled_stream))
            .with_state(Arc::clone(&release)),
    );

    let first = app
        .clone()
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("first response");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(controller.current(), 1);

    let mut body = first.into_body();
    let first_frame = body
        .frame()
        .await
        .expect("first frame")
        .expect("first frame is successful");
    assert_eq!(
        first_frame.into_data().expect("data frame"),
        Bytes::from_static(b"first")
    );
    assert_eq!(controller.current(), 1);

    let excess = app
        .clone()
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("overload response");
    assert_overloaded(excess, true, "/work").await;

    release.add_permits(1);
    let last = body
        .frame()
        .await
        .expect("last frame")
        .expect("last frame is successful");
    assert_eq!(
        last.into_data().expect("data frame"),
        Bytes::from_static(b"last")
    );
    assert!(body.frame().await.is_none(), "stream should reach EOF");
    drop(body);
    assert_eq!(controller.current(), 0);

    let next = app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("slot is reusable");
    assert_eq!(next.status(), StatusCode::OK);
    drop(next);
    assert_eq!(controller.current(), 0);
}

async fn error_stream() -> Response<Body> {
    let stream = futures_util::stream::once(async {
        Err::<Bytes, std::io::Error>(std::io::Error::other("synthetic body failure"))
    });
    Response::new(Body::from_stream(stream))
}

#[tokio::test]
#[serial_test::serial]
async fn cancellation_and_body_errors_release_permits() {
    let controller = limited(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let cancel_app = guarded_test_router(
        controller.clone(),
        Router::new()
            .route("/work", get(controlled_stream))
            .with_state(release),
    );

    let response = cancel_app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("stream response");
    assert_eq!(controller.current(), 1);
    drop(response);
    assert_eq!(controller.current(), 0);

    let error_app = guarded_test_router(
        controller.clone(),
        Router::new().route("/work", get(error_stream)),
    );
    let response = error_app
        .clone()
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("error stream response");
    assert_eq!(controller.current(), 1);
    assert!(
        response.into_body().collect().await.is_err(),
        "synthetic body error should propagate"
    );
    assert_eq!(controller.current(), 0);

    let next = error_app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("slot is reusable after error");
    assert_eq!(next.status(), StatusCode::OK);
    drop(next);
    assert_eq!(controller.current(), 0);
}

async fn panic_handler() -> Response<Body> {
    panic!("synthetic handler panic");
}

#[tokio::test]
#[serial_test::serial]
async fn handler_panics_release_permits() {
    let controller = limited(1);
    let app = guarded_test_router(
        controller.clone(),
        Router::new().route("/work", get(panic_handler)),
    )
    .layer(CatchPanicLayer::new());

    let response = app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("panic is converted to a response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(controller.current(), 0);
}

async fn pending_stream() -> impl IntoResponse {
    let stream = futures_util::stream::pending::<Result<Bytes, Infallible>>();
    Body::from_stream(stream)
}

#[tokio::test]
#[serial_test::serial]
async fn burst_of_126_requests_is_bounded_without_an_upstream() {
    const BURST: usize = 126;
    const LIMIT: usize = 4;

    let controller = limited(LIMIT);
    let app = guarded_test_router(
        controller.clone(),
        Router::new().route("/work", get(pending_stream)),
    );

    let responses =
        join_all((0..BURST).map(|_| app.clone().oneshot(request(Method::GET, "/work")))).await;
    let mut responses: Vec<Response<Body>> = responses
        .into_iter()
        .map(|result| result.expect("router response"))
        .collect();

    let accepted = responses
        .iter()
        .filter(|response| response.status() == StatusCode::OK)
        .count();
    let rejected = responses
        .iter()
        .filter(|response| response.status() == StatusCode::SERVICE_UNAVAILABLE)
        .count();
    assert_eq!(accepted, LIMIT);
    assert_eq!(rejected, BURST - LIMIT);
    assert_eq!(controller.current(), LIMIT);

    responses.clear();
    assert_eq!(controller.current(), 0);

    let next = app
        .oneshot(request(Method::GET, "/work"))
        .await
        .expect("slot is reusable after burst drains");
    assert_eq!(next.status(), StatusCode::OK);
    drop(next);
    assert_eq!(controller.current(), 0);
}
