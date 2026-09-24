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
    /// Asks clients for a password (any is accepted).
    auth: bool,
    /// The authentication each client sent.
    auth_seen: Vec<Option<String>>,
}

type Shared = Arc<Mutex<FakeObs>>;

async fn ws(State(obs): State<Shared>, up: WebSocketUpgrade) -> Response {
    up.on_upgrade(move |socket| serve(obs, socket))
}

async fn serve(obs: Shared, mut socket: WebSocket) {
    let mut hello = json!({"op": 0, "d": {"obsWebSocketVersion": "5.5.2", "rpcVersion": 1}});
    if obs.lock().unwrap().auth {
        hello["d"]["authentication"] = json!({"challenge": "challenge", "salt": "salt"});
    }
    socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    while let Some(Ok(msg)) = socket.recv().await {
        let Message::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(&text).unwrap();
        let reply = match v["op"].as_u64() {
            Some(1) => {
                let auth = v["d"]["authentication"].as_str().map(String::from);
                obs.lock().unwrap().auth_seen.push(auth);
                json!({"op": 2, "d": {"negotiatedRpcVersion": 1}})
            }
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

/// Starts a fake OBS with these stream settings; returns it and its port.
async fn spawn_obs(service_type: &str, settings: Value) -> (Shared, u16) {
    let obs: Shared = Arc::new(Mutex::new(FakeObs {
        service_type: service_type.into(),
        settings,
        streaming: false,
        inputs: vec![],
        auth: false,
        auth_seen: vec![],
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route("/", get(ws)).with_state(obs.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (obs, port)
}

async fn setup() -> (axum::Router, Shared, Arc<Secrets>, tempfile::TempDir) {
    setup_with(
        "rtmp_common",
        json!({"service": "Twitch", "server": "auto", "key": "live_987_secret"}),
    )
    .await
}

async fn setup_with(
    service_type: &str,
    settings: Value,
) -> (axum::Router, Shared, Arc<Secrets>, tempfile::TempDir) {
    setup_with_config(service_type, settings, |_| {}).await
}

async fn setup_with_config(
    service_type: &str,
    settings: Value,
    edit: impl FnOnce(&mut Config),
) -> (axum::Router, Shared, Arc<Secrets>, tempfile::TempDir) {
    let (obs, obs_port) = spawn_obs(service_type, settings).await;

    let relay = streamdelay_relay::start(RelayConfig {
        ingest_bind: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    })
    .await
    .unwrap();
    let mut config = Config::default();
    config.api.token = TOKEN.into();
    config.obs.port = obs_port;
    edit(&mut config);
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
    // For Twitch only.
    let saved: serde_json::Value =
        serde_json::from_str(&secrets.get(secret::DESTINATION_KEY).unwrap()).unwrap();
    assert_eq!(saved["key"], "live_987_secret");
    assert_eq!(saved["server"], "rtmp://live.twitch.tv/app");
    assert!(!r.to_string().contains("live_987_secret"));
    let (_, cfg) = call(&app, "GET", "/api/v1/config", None).await;
    assert_eq!(cfg["destination_key_set"], true);
    // The backup, key included, is a secret.
    assert_eq!(
        cfg["config"]["obs"]["backup"]["service_type"],
        "rtmp_common"
    );
    assert!(cfg["config"]["obs"]["backup"]["settings_json"].is_null());
    assert!(!cfg.to_string().contains("live_987_secret"));
    assert!(
        secrets
            .get(secret::OBS_BACKUP)
            .unwrap()
            .contains("live_987_secret")
    );

    // Running the wizard again is harmless.
    let (s, r) = call(&app, "POST", "/api/v1/obs/configure", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["overlay_added"], false);

    let (s, r) = call(&app, "POST", "/api/v1/obs/restore", None).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    {
        let o = obs.lock().unwrap();
        assert_eq!(o.service_type, "rtmp_common");
        assert_eq!(o.settings["service"], "Twitch");
        assert_eq!(o.settings["key"], "live_987_secret");
    }
    assert_eq!(secrets.get(secret::OBS_BACKUP), None);
}

#[tokio::test]
async fn custom_server_credentials_stay_out_of_settings_and_diagnostics() {
    let custom = json!({
        "server": "rtmp://ingest.example.net/live", "key": "custom-key-5150",
        "use_auth": true, "username": "streamer", "password": "hunter2-secret",
    });
    let (app, obs, _secrets, _dir) = setup_with("rtmp_custom", custom.clone()).await;
    let (s, r) = call(&app, "POST", "/api/v1/obs/configure", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["imported_key"], false, "not a Twitch key");

    let (_, cfg) = call(&app, "GET", "/api/v1/config", None).await;
    let (s, diag) = call(&app, "GET", "/api/v1/diagnostics", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        cfg["config"]["obs"]["backup"]["service_type"],
        "rtmp_custom"
    );
    assert_eq!(
        diag["settings"]["config"]["obs"]["backup"]["service_type"],
        "rtmp_custom"
    );
    for text in [cfg.to_string(), diag.to_string()] {
        assert!(!text.contains("hunter2-secret"), "{text}");
        assert!(!text.contains("custom-key-5150"), "{text}");
    }

    let (s, r) = call(&app, "POST", "/api/v1/obs/restore", None).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let o = obs.lock().unwrap();
    assert_eq!(o.service_type, "rtmp_custom");
    assert_eq!(o.settings, custom);
}

#[tokio::test]
async fn saved_password_and_backup_stay_with_their_obs() {
    let (app, home, secrets, _dir) = setup().await;
    home.lock().unwrap().auth = true;
    let home_port = {
        let (_, cfg) = call(&app, "GET", "/api/v1/config", None).await;
        cfg["config"]["obs"]["port"].as_u64().unwrap()
    };
    let connect = |port: u64, password: &str| json!({"host": "127.0.0.1", "port": port, "password": password});
    let (s, r) = call(
        &app,
        "POST",
        "/api/v1/obs/connect",
        Some(connect(home_port, "obs-pass-1234")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = call(&app, "POST", "/api/v1/obs/configure", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let sent = home.lock().unwrap().auth_seen.clone();
    assert!(
        sent.iter().all(|a| a.is_some() && *a == sent[0]),
        "{sent:?}"
    );

    // Another OBS (or whatever listens there) gets neither the saved password...
    let (other, other_port) = spawn_obs(
        "rtmp_common",
        json!({"service": "YouTube - RTMPS", "server": "x", "key": "yt"}),
    )
    .await;
    other.lock().unwrap().auth = true;
    let (s, r) = call(
        &app,
        "POST",
        "/api/v1/obs/connect",
        Some(connect(u64::from(other_port), "")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(other.lock().unwrap().auth_seen, vec![None]);
    // ...which belonged to the previous OBS and is forgotten...
    assert_eq!(secrets.get(secret::OBS_PASSWORD), None);
    assert_eq!(r["password_saved"], false);
    // ...nor the backed-up settings of the first one.
    let (s, r) = call(&app, "POST", "/api/v1/obs/restore", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{r}");
    assert!(r["error"].as_str().unwrap().contains("connect to that OBS"));
    assert_eq!(other.lock().unwrap().settings["key"], "yt");

    // Back at the first OBS, the settings go back where they came from.
    let (s, r) = call(
        &app,
        "POST",
        "/api/v1/obs/connect",
        Some(connect(home_port, "obs-pass-1234")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = call(&app, "POST", "/api/v1/obs/restore", None).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(home.lock().unwrap().settings["key"], "live_987_secret");
}

#[tokio::test]
async fn an_earlier_stream_delay_address_does_not_replace_the_backup() {
    let (app, obs, secrets, _dir) = setup().await;
    let (s, r) = call(&app, "POST", "/api/v1/obs/configure", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    // stream-delay's port changed since (the one it wanted was taken): OBS still
    // streams to the old address.
    let current = obs.lock().unwrap().settings["server"].clone();
    obs.lock().unwrap().settings = json!({
        "server": "rtmp://127.0.0.1:1/live", "key": "streamdelay", "use_auth": false,
    });
    let (_, status) = call(&app, "GET", "/api/v1/obs/status", None).await;
    assert_eq!(status["configured"], false);
    let (s, r) = call(&app, "POST", "/api/v1/obs/configure", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(obs.lock().unwrap().settings["server"], current);
    assert!(
        secrets
            .get(secret::OBS_BACKUP)
            .unwrap()
            .contains("live_987_secret"),
        "the backup of OBS's own settings was replaced"
    );
    let (s, r) = call(&app, "POST", "/api/v1/obs/restore", None).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(obs.lock().unwrap().settings["key"], "live_987_secret");
}

#[tokio::test]
async fn an_obs_on_another_computer_is_not_pointed_at_itself() {
    // stream-delay's RTMP input only listens on this computer (the default).
    let (app, obs, _secrets, _dir) = setup_with_config(
        "rtmp_common",
        json!({"service": "Twitch", "server": "auto", "key": "live_987_secret"}),
        |c| c.obs.host = "192.0.2.10".into(),
    )
    .await;
    let (s, r) = call(&app, "POST", "/api/v1/obs/configure", Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("--ingest 0.0.0.0:1935"),
        "{r}"
    );
    assert_eq!(obs.lock().unwrap().service_type, "rtmp_common");
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
