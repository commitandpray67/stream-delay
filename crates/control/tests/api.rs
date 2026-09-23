//! API tests: authentication, Host/Origin checks and delay commands.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;

use streamdelay_config::{Config, Secrets};
use streamdelay_relay::RelayConfig;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef";
const PORT: u16 = 7788;

async fn app() -> axum::Router {
    app_with_secrets("").await
}

async fn app_with_secrets(suffix: &str) -> axum::Router {
    let relay = streamdelay_relay::start(RelayConfig {
        ingest_bind: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    })
    .await
    .unwrap();
    let mut config = Config::default();
    config.api.token = TOKEN.into();
    config.destination.url = String::new();
    // Secrets go to a throwaway directory that is never cleaned up mid-test.
    let dir = std::env::temp_dir().join(format!("sd-api-test-{}{suffix}", std::process::id()));
    let secrets = Arc::new(Secrets::new(&dir, false));
    streamdelay_control::router(relay, config, secrets, PORT)
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
    let (s, body) = send(&app, put(r#"{"seconds":10,"mode":"mask"}"#)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["target_ms"], 10_000);
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

fn authed(method: &str, path: &str) -> axum::http::request::Builder {
    req(method, path).header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
}

#[tokio::test]
async fn config_hides_secrets_and_validates_updates() {
    let app = app().await;
    let (s, body) = send(
        &app,
        authed("GET", "/api/v1/config").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body["config"]["api"]["token"], "",
        "token must never be returned"
    );
    assert_eq!(body["destination_key_set"], false);
    assert!(
        body["urls"]["dock"]
            .as_str()
            .unwrap()
            .contains("/dock?token=")
    );
    assert_eq!(
        body["urls"]["obs_server"]
            .as_str()
            .unwrap()
            .split(':')
            .next(),
        Some("rtmp")
    );

    let json = |b: &str| {
        authed("PUT", "/api/v1/config")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap()
    };
    let (s, body) = send(&app, json(r##"{"overlay":{"badge":false,"badge_position":"top-left","popup":true,"mask_title":"BRB","mask_subtitle":"","accent_color":"#ff0000","background_color":"#000","text_color":"#ffffff","mask_image":""}}"##)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["overlay"]["mask_title"], "BRB");
    let (s, _) = send(&app, json(r##"{"overlay":{"badge":false,"badge_position":"middle","popup":true,"mask_title":"","mask_subtitle":"","accent_color":"red","background_color":"#000","text_color":"#fff","mask_image":""}}"##)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, body) = send(
        &app,
        json(r#"{"destination":{"service":"custom","url":"http://nope","key_mode":"stored"}}"#),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("rtmp"));

    let (s, body) = send(
        &app,
        authed("PUT", "/api/v1/destination/key")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"key":"live_123"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["destination_key_set"], true);
    assert!(
        !body.to_string().contains("live_123"),
        "key must never be returned"
    );
}

#[tokio::test]
async fn ui_is_served_without_token() {
    let app = app().await;
    let (s, _) = send(&app, req("GET", "/dock").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn diagnostics_bundle_has_no_secrets() {
    use tracing_subscriber::prelude::*;
    const KEY: &str = "sk-not-a-twitch-key-42";
    let app = app_with_secrets("-diag").await;
    let (s, _) = send(
        &app,
        authed("PUT", "/api/v1/destination/key")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"key":"{KEY}"}}"#)))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // Log lines that leak every kind of secret.
    let subscriber = tracing_subscriber::registry().with(streamdelay_control::diagnostics::layer());
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!("egress to rtmp://example/app/{KEY} failed");
        tracing::warn!("dock opened: /dock?token={TOKEN}");
        tracing::warn!("obs profile uses live_987654_AbCdEfGh");
    });

    let r = req("GET", "/api/v1/diagnostics")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.0, StatusCode::UNAUTHORIZED);
    let resp = app
        .clone()
        .oneshot(
            authed("GET", "/api/v1/diagnostics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let disposition = resp.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap();
    assert!(disposition.starts_with("attachment; filename=\"stream-delay-diagnostics-"));
    let text = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["state"]["delay"]["phase"], "offline");
    let logs = body["logs"].as_array().unwrap();
    assert!(logs.iter().any(|l| {
        l.as_str()
            .unwrap()
            .contains("egress to rtmp://example/app/<redacted> failed")
    }));
    for secret in [KEY, TOKEN, "987654_AbCdEfGh"] {
        assert!(!text.contains(secret), "diagnostics leak {secret}");
    }
}
