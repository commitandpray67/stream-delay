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
async fn every_response_carries_security_headers() {
    let app = app().await;
    for (r, want) in [
        (req("GET", "/").body(Body::empty()).unwrap(), StatusCode::OK),
        (
            authed("GET", "/api/v1/state").body(Body::empty()).unwrap(),
            StatusCode::OK,
        ),
        // Refusals too.
        (
            req("GET", "/api/v1/state").body(Body::empty()).unwrap(),
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        let path = r.uri().to_string();
        let resp = app.clone().oneshot(r).await.unwrap();
        assert_eq!(resp.status(), want, "{path}");
        let h = resp.headers();
        assert_eq!(
            h["content-security-policy"], "frame-ancestors 'self'",
            "{path}"
        );
        assert_eq!(h["x-frame-options"], "SAMEORIGIN", "{path}");
        assert_eq!(h["x-content-type-options"], "nosniff", "{path}");
        assert_eq!(h["referrer-policy"], "no-referrer", "{path}");
    }
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

fn with_token(method: &str, path: &str, token: &str) -> axum::http::request::Builder {
    req(method, path).header(header::AUTHORIZATION, format!("Bearer {token}"))
}

#[tokio::test]
async fn dock_and_overlay_tokens_are_limited() {
    use streamdelay_control::{Scope, scoped_token};
    let app = app_with_secrets("-scopes").await;
    let control = scoped_token(TOKEN, Scope::Control);
    let read = scoped_token(TOKEN, Scope::Read);

    // The links handed out carry the limited tokens.
    let (_, body) = send(
        &app,
        authed("GET", "/api/v1/config").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(body["urls"]["dashboard"].as_str().unwrap().ends_with(TOKEN));
    assert!(body["urls"]["dock"].as_str().unwrap().ends_with(&control));
    assert!(body["urls"]["overlay"].as_str().unwrap().ends_with(&read));
    assert_eq!(body["scope"], "admin");

    let delay = |token: &str| {
        with_token("PUT", "/api/v1/delay", token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"seconds":5}"#))
            .unwrap()
    };
    let get = |path: &str, token: &str| with_token("GET", path, token).body(Body::empty()).unwrap();

    // Overlay links can read the state, nothing else.
    assert_eq!(
        send(&app, get("/api/v1/state", &read)).await.0,
        StatusCode::OK
    );
    let (s, body) = send(&app, delay(&read)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("dashboard"));
    // Query-string tokens (browser sources, WebSockets) get the same scope.
    let r = req("GET", &format!("/api/v1/state?token={read}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.0, StatusCode::OK);
    // The events socket is open to them (this plain GET fails the upgrade instead).
    let (s, _) = send(&app, get("/api/v1/events", &read)).await;
    assert!(
        s != StatusCode::FORBIDDEN && s != StatusCode::UNAUTHORIZED,
        "{s}"
    );

    // Dock links can also change the delay.
    assert_eq!(send(&app, delay(&control)).await.0, StatusCode::OK);
    let r = with_token("POST", "/api/v1/presets/0", &control)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.0, StatusCode::OK);

    // Neither can reach settings, keys, diagnostics or OBS.
    for token in [&control, &read] {
        for path in [
            "/api/v1/config",
            "/api/v1/diagnostics",
            "/api/v1/obs/status",
        ] {
            assert_eq!(
                send(&app, get(path, token)).await.0,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
        let r = with_token("PUT", "/api/v1/destination/key", token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"key":"live_1"}"#))
            .unwrap();
        assert_eq!(send(&app, r).await.0, StatusCode::FORBIDDEN);
        let r = with_token("PUT", "/api/v1/config", token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"destination":{"service":"custom","url":"rtmp://evil.example/app","key_mode":"stored"}}"#,
            ))
            .unwrap();
        assert_eq!(send(&app, r).await.0, StatusCode::FORBIDDEN);
    }
    // Something that merely looks like a token is still rejected.
    let (s, _) = send(
        &app,
        get("/api/v1/state", "0123456789abcdef0123456789abcdef"),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

async fn put_config(app: &axum::Router, body: &str) -> (StatusCode, Value) {
    send(
        app,
        authed("PUT", "/api/v1/config")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn key_in_destination_url_is_stored_as_a_secret() {
    const KEY: &str = "abcd-efgh-ijkl-mnop-qrst";
    let app = app_with_secrets("-urlkey").await;
    let (s, body) = put_config(
        &app,
        &format!(
            r#"{{"destination":{{"service":"custom","url":"rtmp://a.rtmp.youtube.com/live2/{KEY}","key_mode":"stored"}}}}"#
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(
        body["config"]["destination"]["url"],
        "rtmp://a.rtmp.youtube.com/live2"
    );
    assert_eq!(body["destination_key_set"], true);
    assert!(!body.to_string().contains(KEY), "key returned: {body}");
    let r = authed("GET", "/api/v1/diagnostics")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(r).await.unwrap();
    let text = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        !String::from_utf8_lossy(&text).contains(KEY),
        "diagnostics leak the key"
    );
}

#[tokio::test]
async fn changing_to_another_server_forgets_the_stored_key() {
    let app = app_with_secrets("-forget").await;
    let dest = |url: &str| {
        format!(r#"{{"destination":{{"service":"custom","url":"{url}","key_mode":"stored"}}}}"#)
    };
    let (s, _) = put_config(&app, &dest("rtmp://live.twitch.tv/app")).await;
    assert_eq!(s, StatusCode::OK);
    let r = authed("PUT", "/api/v1/destination/key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"key":"live_123"}"#))
        .unwrap();
    assert_eq!(send(&app, r).await.1["destination_key_set"], true);
    // Same service over RTMPS, or one of its regional servers: the key stays.
    for url in [
        "rtmps://live.twitch.tv:443/app",
        "rtmp://sea02.contribute.live-video.net/app",
    ] {
        let (_, body) = put_config(&app, &dest(url)).await;
        assert_eq!(body["destination_key_set"], true, "{url}");
    }
    // Another server: the key must not follow.
    let (s, body) = put_config(&app, &dest("rtmp://ingest.example.net/live")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["destination_key_set"], false);

    // Nor via an empty destination in between.
    put_config(&app, &dest("rtmp://live.twitch.tv/app")).await;
    let r = authed("PUT", "/api/v1/destination/key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"key":"live_123"}"#))
        .unwrap();
    assert_eq!(send(&app, r).await.1["destination_key_set"], true);
    let (_, body) = put_config(&app, &dest("")).await;
    assert_eq!(body["destination_key_set"], true, "clearing keeps the key");
    let (_, body) = put_config(&app, &dest("rtmp://ingest.example.net/live")).await;
    assert_eq!(
        body["destination_key_set"], false,
        "key carried over via an empty URL"
    );
}

#[tokio::test]
async fn end_stream_resume_and_buffer_setting() {
    let app = app_with_secrets("-end").await;
    let (s, body) = send(
        &app,
        authed("POST", "/api/v1/stream/end")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["ended"], true);
    let (s, body) = send(
        &app,
        authed("POST", "/api/v1/stream/resume")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["ended"], false);

    // The rolling buffer can be switched off without a restart.
    let (_, current) = send(
        &app,
        authed("GET", "/api/v1/config").body(Body::empty()).unwrap(),
    )
    .await;
    let mut delay = current["config"]["delay"].clone();
    assert_eq!(delay["keep_buffer"], true);
    delay["keep_buffer"] = false.into();
    let (s, body) = send(
        &app,
        authed("PUT", "/api/v1/config")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "delay": delay }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["delay"]["keep_buffer"], false);
    assert_eq!(body["restart_required"], false);

    // The health check names the app, so a second copy can recognize it.
    let (s, body) = send(&app, req("GET", "/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["app"], "stream-delay");
}

#[tokio::test]
async fn go_live_reads_its_body_whatever_the_content_type() {
    let app = app().await;
    let live = |body: &'static str| {
        authed("POST", "/api/v1/live")
            .body(Body::from(body))
            .unwrap()
    };
    assert_eq!(send(&app, live("")).await.0, StatusCode::OK);
    // Without a Content-Type header, as some integrations send it.
    assert_eq!(
        send(&app, live(r#"{"when":"after-air"}"#)).await.0,
        StatusCode::OK
    );
    // Must never be taken for an empty body, which means "now".
    let (s, body) = send(&app, live(r#"{"when":"later"}"#)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("invalid"));
}

#[tokio::test]
async fn diagnostics_download_links_work_once() {
    use streamdelay_control::{Scope, scoped_token};
    let app = app_with_secrets("-diaglink").await;
    let (s, body) = send(
        &app,
        authed("POST", "/api/v1/diagnostics/link")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let url = body["url"].as_str().unwrap().to_string();
    assert!(!url.contains(TOKEN));
    // Opened as a plain link: no token.
    let resp = app
        .clone()
        .oneshot(req("GET", &url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    let again = req("GET", &url).body(Body::empty()).unwrap();
    assert_eq!(send(&app, again).await.0, StatusCode::NOT_FOUND);
    let guess = req("GET", "/diagnostics/0123456789abcdef0123456789abcdef")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, guess).await.0, StatusCode::NOT_FOUND);
    // Dock links cannot make one.
    let control = scoped_token(TOKEN, Scope::Control);
    let r = with_token("POST", "/api/v1/diagnostics/link", &control)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn allowing_lan_access_waits_for_a_restart() {
    let app = app_with_secrets("-lan").await;
    let (s, body) = put_config(&app, r#"{"allow_lan":true}"#).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["restart_required"], true);
    let r = Request::builder()
        .uri("/api/v1/state")
        .header(header::HOST, format!("attacker.example:{PORT}"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, r).await.0, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn grace_period_is_bounded() {
    let app = app_with_secrets("-grace").await;
    let (s, body) = put_config(&app, r#"{"grace_seconds": 100000}"#).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("grace"));
    let (s, body) = put_config(&app, r#"{"grace_seconds": 60}"#).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}
