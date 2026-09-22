//! OBS setup wizard against a mock obs-websocket v5 server.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use streamdelay_config::{Config, SecretStore, Secrets, secret};
use streamdelay_relay::RelayConfig;
use tower::ServiceExt;

const TOKEN: &str = "wizard-token";
const PORT: u16 = 7788;

/// State of the fake OBS.
struct FakeObs {
    service_type: String,
    settings: Value,
    streaming: bool,
    inputs: Vec<String>,
}

type Shared = Arc<Mutex<FakeObs>>;

async fn ws(State(obs): State<Shared>, up: WebSocketUpgrade) -> Response {
    up.on_upgrade(move |socket| serve(obs, socket))
}

async fn serve(obs: Shared, mut socket: WebSocket) {
    let hello = json!({"op": 0, "d": {"obsWebSocketVersion": "5.5.2", "rpcVersion": 1}});
    socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    while let Some(Ok(msg)) = socket.recv().await {
        let Message::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(&text).unwrap();
        let reply = match v["op"].as_u64() {
            Some(1) => json!({"op": 2, "d": {"negotiatedRpcVersion": 1}}),
            Some(6) => {
                let d = &v["d"];
                let ty = d["requestType"].as_str().unwrap().to_string();
                let data = respond(&obs, &ty, &d["requestData"]);
                json!({"op": 7, "d": {
                    "requestType": ty,
                    "requestId": d["requestId"],
                    "requestStatus": {"result": true, "code": 100},
                    "responseData": data,
                }})
            }
            _ => continue,
        };
        if socket
            .send(Message::Text(reply.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
}

fn respond(obs: &Shared, ty: &str, data: &Value) -> Value {
    let mut o = obs.lock().unwrap();
    match ty {
        "GetVersion" => json!({
            "obsVersion": "29.1.3", "obsWebSocketVersion": "5.5.2", "rpcVersion": 1,
            "availableRequests": [], "supportedImageFormats": [], "platform": "linux",
            "platformDescription": "test",
        }),
        "GetStreamStatus" => json!({
            "outputActive": o.streaming, "outputReconnecting": false,
            "outputTimecode": "00:00:00.000", "outputDuration": 0, "outputCongestion": 0.0,
            "outputBytes": 0, "outputSkippedFrames": 0, "outputTotalFrames": 0,
        }),
        "GetStreamServiceSettings" => json!({
            "streamServiceType": o.service_type, "streamServiceSettings": o.settings,
        }),
        "SetStreamServiceSettings" => {
            o.service_type = data["streamServiceType"].as_str().unwrap().into();
            o.settings = data["streamServiceSettings"].clone();
            Value::Null
        }
        "GetInputList" => json!({"inputs": o.inputs.iter().map(|n| json!({
            "inputName": n, "inputUuid": "00000000-0000-0000-0000-000000000001",
            "inputKind": "browser_source", "unversionedInputKind": "browser_source",
        })).collect::<Vec<_>>()}),
        "GetVideoSettings" => json!({
            "fpsNumerator": 60, "fpsDenominator": 1, "baseWidth": 1920, "baseHeight": 1080,
            "outputWidth": 1920, "outputHeight": 1080,
        }),
        "GetCurrentProgramScene" => json!({
            "sceneName": "Game", "sceneUuid": "00000000-0000-0000-0000-000000000002",
            "currentProgramSceneName": "Game",
            "currentProgramSceneUuid": "00000000-0000-0000-0000-000000000002",
        }),
        "CreateInput" => {
            assert_eq!(data["inputKind"], "browser_source");
            assert_eq!(data["sceneName"], "Game");
            assert!(
                data["inputSettings"]["url"]
                    .as_str()
                    .unwrap()
                    .contains("/overlay?token=")
            );
            assert_eq!(data["inputSettings"]["width"], 1920);
            o.inputs.push(data["inputName"].as_str().unwrap().into());
            json!({"inputUuid": "00000000-0000-0000-0000-000000000003", "sceneItemId": 7})
        }
        other => panic!("unexpected request {other}"),
    }
}

async fn setup() -> (axum::Router, Shared, Arc<Secrets>, tempfile::TempDir) {
    let obs: Shared = Arc::new(Mutex::new(FakeObs {
        service_type: "rtmp_common".into(),
        settings: json!({"service": "Twitch", "server": "auto", "key": "live_987_secret"}),
        streaming: false,
        inputs: vec![],
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let obs_port = listener.local_addr().unwrap().port();
    let app = Router::new().route("/", get(ws)).with_state(obs.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let relay = streamdelay_relay::start(RelayConfig {
        ingest_bind: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    })
    .await
    .unwrap();
    let mut config = Config::default();
    config.api.token = TOKEN.into();
    config.obs.port = obs_port;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(Secrets::new(dir.path(), false));
    let router = streamdelay_control::router(relay, config, secrets.clone(), PORT);
    (router, obs, secrets, dir)
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, format!("127.0.0.1:{PORT}"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn configure_imports_key_adds_overlay_and_restores() {
    let (app, obs, secrets, _dir) = setup().await;

    let (s, status) = call(&app, "GET", "/api/v1/obs/status", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(status["reachable"], true, "{status}");
    assert_eq!(status["version"], "29.1.3");
    assert_eq!(status["configured"], false);
    assert_eq!(status["current_server"], "Twitch (auto)");

    let (s, r) = call(
        &app,
        "POST",
        "/api/v1/obs/configure",
        Some(json!({"import_key": true, "add_overlay": true})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["imported_key"], true);
    assert_eq!(r["overlay_added"], true);
    assert_eq!(r["status"]["configured"], true);
    {
        let o = obs.lock().unwrap();
        assert_eq!(o.service_type, "rtmp_custom");
        assert!(
            o.settings["server"]
                .as_str()
                .unwrap()
                .starts_with("rtmp://127.0.0.1:")
        );
        assert_eq!(o.inputs, vec!["Stream Delay Overlay".to_string()]);
    }
    // The key moved into stream-delay's secret store, and is never echoed back.
    assert_eq!(
        secrets.get(secret::DESTINATION_KEY).as_deref(),
        Some("live_987_secret")
    );
    assert!(!r.to_string().contains("live_987_secret"));
    let (_, cfg) = call(&app, "GET", "/api/v1/config", None).await;
    assert_eq!(cfg["destination_key_set"], true);
    assert!(
        cfg["config"]["obs"]["backup"]["settings_json"]
            .as_str()
            .unwrap()
            .contains("Twitch")
    );
    assert!(
        !cfg.to_string().contains("live_987_secret"),
        "backup must not contain the key"
    );

    // Running the wizard again is harmless.
    let (s, r) = call(&app, "POST", "/api/v1/obs/configure", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["overlay_added"], false);

    let (s, r) = call(&app, "POST", "/api/v1/obs/restore", None).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let o = obs.lock().unwrap();
    assert_eq!(o.service_type, "rtmp_common");
    assert_eq!(o.settings["service"], "Twitch");
    assert_eq!(o.settings["key"], "live_987_secret");
}

#[tokio::test]
async fn refuses_while_obs_is_streaming() {
    let (app, obs, _secrets, _dir) = setup().await;
    obs.lock().unwrap().streaming = true;
    let (s, r) = call(&app, "POST", "/api/v1/obs/configure", Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(r["error"].as_str().unwrap().contains("Stop the stream"));
    assert_eq!(obs.lock().unwrap().service_type, "rtmp_common");
}

#[tokio::test]
async fn unreachable_obs_is_reported_not_fatal() {
    let (app, _obs, _secrets, _dir) = setup().await;
    let (s, r) = call(
        &app,
        "POST",
        "/api/v1/obs/connect",
        Some(json!({"host": "127.0.0.1", "port": 1, "password": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert!(r["error"].as_str().unwrap().contains("WebSocket"));
}
