//! API tests: authentication, Host/Origin checks and delay commands.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use streamdelay_control::{ControlConfig, Preset};
use streamdelay_relay::RelayConfig;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef";
const PORT: u16 = 7788;

async fn app() -> axum::Router {
    let relay = streamdelay_relay::start(RelayConfig {
        ingest_bind: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    })
    .await
    .unwrap();
    let config = ControlConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        token: TOKEN.into(),
        allow_lan: false,
        presets: Preset::defaults(),
    };
    streamdelay_control::router(relay, &config, PORT)
}

fn req(method: &str, path: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, format!("127.0.0.1:{PORT}"))
}

async fn send(app: &axum::Router, r: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(r).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn token_is_required() {
    let app = app().await;
    let (s, _) = send(
        &app,
        req("GET", "/api/v1/state").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = send(
        &app,
        req("GET", "/api/v1/state")
            .header("Authorization", "Bearer wrong")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, body) = send(
        &app,
        req("GET", "/api/v1/state")
            .header("Authorization", format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["delay"]["phase"], "offline");
    // Query-string token (used by browser sources and WebSockets).
    let (s, _) = send(
        &app,
        req("GET", &format!("/api/v1/state?token={TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // Health check needs no token.
    let (s, _) = send(&app, req("GET", "/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn foreign_host_and_cross_origin_are_rejected() {
    let app = app().await;
    // DNS rebinding: attacker.example resolving to 127.0.0.1.
    let r = Request::builder()
        .uri(format!("/api/v1/state?token={TOKEN}"))
        .header(header::HOST, format!("attacker.example:{PORT}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.0, StatusCode::MISDIRECTED_REQUEST);
    // A page on another origin calling the API.
    let r = req("PUT", "/api/v1/delay")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"seconds":0}"#))
        .unwrap();
    assert_eq!(send(&app, r).await.0, StatusCode::FORBIDDEN);
    // Same origin (the dock) is fine.
    let r = req("GET", "/api/v1/state")
        .header(header::ORIGIN, format!("http://127.0.0.1:{PORT}"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.0, StatusCode::OK);
}

#[tokio::test]
async fn delay_commands() {
    let app = app().await;
    let put = |body: &str| {
        req("PUT", "/api/v1/delay")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let (s, body) = send(&app, put(r#"{"seconds":30}"#)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["target_ms"], 30_000);
    let (s, body) = send(&app, put(r#"{"seconds":9999}"#)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("maximum"));
    let (s, _) = send(&app, put(r#"{"seconds":-1}"#)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, body) = send(
        &app,
        req("POST", "/api/v1/presets/0")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["target_ms"], 0);
    let (s, _) = send(
        &app,
        req("POST", "/api/v1/presets/99")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
