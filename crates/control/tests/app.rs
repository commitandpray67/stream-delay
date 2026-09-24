//! `App::start`: settings migration and the ingest key for network-reachable inputs.

use std::sync::Arc;

use streamdelay_config::{Config, MemorySecrets, ObsBackup, SecretStore, secret};
use streamdelay_control::{App, AppError, AppOptions, Overrides};

fn overrides(ingest: &str) -> Overrides {
    Overrides {
        ingest: Some(ingest.parse().unwrap()),
        api: Some("127.0.0.1:0".parse().unwrap()),
        ..Default::default()
    }
}

#[tokio::test]
async fn network_ingest_gets_a_saved_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let app = App::start(AppOptions {
        config_path: Some(path.clone()),
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("0.0.0.0:0"),
    })
    .await
    .unwrap();
    let key = app.config().ingest.key.expect("no ingest key generated");
    assert_eq!(key.len(), 32);
    assert_eq!(app.urls().obs_key, key);
    // Kept for the next run, without this run's command-line overrides.
    let saved = Config::load_or_create(&path).unwrap();
    assert_eq!(saved.ingest.key.as_deref(), Some(key.as_str()));
    assert_eq!(saved.ingest.bind, Config::default().ingest.bind);
    app.shutdown().await;
}

#[tokio::test]
async fn ipv6_addresses_give_working_links() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("no IPv6 loopback here; skipping");
        return;
    }
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: Overrides {
            ingest: Some("[::1]:0".parse().unwrap()),
            api: Some("[::1]:0".parse().unwrap()),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    let urls = app.urls();
    let api = format!("http://[::1]:{}/", app.api_addr.port());
    assert!(urls.dashboard.starts_with(&api), "{}", urls.dashboard);
    assert!(
        urls.dock.starts_with(&format!("{api}dock?")),
        "{}",
        urls.dock
    );
    assert!(
        urls.obs_server.starts_with("rtmp://[::1]:"),
        "{}",
        urls.obs_server
    );
    app.shutdown().await;
}

#[tokio::test]
async fn local_ingest_needs_no_key() {
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    assert_eq!(app.config().ingest.key, None);
    assert_eq!(app.urls().obs_key, "streamdelay");
    app.shutdown().await;
}

#[tokio::test]
async fn key_in_saved_destination_url_moves_to_the_secret_store() {
    const KEY: &str = "abcd-efgh-ijkl-mnop-qrst";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut c = Config::default();
    c.api.token = "0123456789abcdef".into();
    c.destination.service = "custom".into();
    c.destination.url = format!("rtmp://a.rtmp.youtube.com/live2/{KEY}");
    c.save(&path).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let app = App::start(AppOptions {
        config_path: Some(path.clone()),
        secrets: secrets.clone(),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    // Saved with the server it belongs to.
    let saved: serde_json::Value =
        serde_json::from_str(&secrets.get(secret::DESTINATION_KEY).unwrap()).unwrap();
    assert_eq!(saved["value"], KEY);
    assert_eq!(saved["for"], "rtmp://a.rtmp.youtube.com/live2");
    assert_eq!(
        app.config().destination.url,
        "rtmp://a.rtmp.youtube.com/live2"
    );
    assert!(!app.needs_setup());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains(KEY), "key still in config.toml");
    app.shutdown().await;
}

#[tokio::test]
async fn saved_obs_settings_move_out_of_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut c = Config::default();
    c.api.token = "0123456789abcdef".into();
    c.obs.backup = Some(ObsBackup {
        service_type: "rtmp_custom".into(),
        obs: None,
        settings_json: Some(
            r#"{"server":"rtmp://ingest.example.net/live","use_auth":true,"password":"hunter2-secret"}"#
                .into(),
        ),
    });
    c.save(&path).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    secrets
        .set(secret::OBS_BACKUP_KEY, "custom-key-5150")
        .unwrap();
    let app = App::start(AppOptions {
        config_path: Some(path.clone()),
        secrets: secrets.clone(),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("hunter2-secret"),
        "password still in config.toml"
    );
    assert!(text.contains("rtmp_custom"), "the backup must be kept");
    let saved = secrets.get(secret::OBS_BACKUP).unwrap();
    assert!(saved.contains("hunter2-secret") && saved.contains("custom-key-5150"));
    assert_eq!(secrets.get(secret::OBS_BACKUP_KEY), None);
    app.shutdown().await;
}

#[tokio::test]
async fn short_tokens_and_out_of_range_settings_are_refused_at_startup() {
    let start = |overrides: Overrides| {
        App::start(AppOptions {
            config_path: None,
            secrets: Arc::new(MemorySecrets::default()),
            overrides,
        })
    };
    let short = start(Overrides {
        token: Some("hunter2".into()),
        ..overrides("127.0.0.1:0")
    })
    .await;
    assert!(
        matches!(short, Err(AppError::WeakToken)),
        "short token accepted"
    );
    let empty = start(Overrides {
        token: Some(String::new()),
        ..overrides("127.0.0.1:0")
    })
    .await;
    assert!(
        matches!(empty, Err(AppError::WeakToken)),
        "empty token accepted"
    );
    // Characters that a query string would change or cut off.
    for token in [
        "0123456789abcdef+X",
        "0123456789abcdef&x=1",
        "0123456789abcdef#x",
        "0123456789abcdef%41",
        " 0123456789abcdef",
        "0123456789abcdéfgh",
    ] {
        let r = start(Overrides {
            token: Some(token.into()),
            ..overrides("127.0.0.1:0")
        })
        .await;
        assert!(
            matches!(r, Err(AppError::TokenCharacters)),
            "{token:?} accepted"
        );
    }
    let fine = start(Overrides {
        token: Some("Ab0.123_456~789-xyz".into()),
        ..overrides("127.0.0.1:0")
    })
    .await
    .unwrap();
    fine.shutdown().await;
    // Would overflow the buffer size computation.
    let huge = start(Overrides {
        max_delay_seconds: Some(u64::MAX / 10),
        ..overrides("127.0.0.1:0")
    })
    .await;
    assert!(
        matches!(huge, Err(AppError::Settings(_))),
        "huge maximum delay accepted"
    );
    let grace = start(Overrides {
        grace_seconds: Some(1_000_000),
        ..overrides("127.0.0.1:0")
    })
    .await;
    assert!(
        matches!(grace, Err(AppError::Settings(_))),
        "huge grace period accepted"
    );
}

/// A minimal HTTP/1.1 request to a running instance.
async fn http(app: &App, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = app.api_addr;
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        app.config().api.token,
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).await.unwrap();
    let status = resp[9..12].parse().unwrap();
    let json = resp
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str(b).ok())
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn a_command_line_destination_never_gets_the_stored_key_of_another_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut c = Config::default(); // Twitch
    c.api.token = "0123456789abcdef".into();
    c.save(&path).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    secrets
        .set(secret::DESTINATION_KEY, "live_123_twitchkey")
        .unwrap();
    let start = |dest: &str| {
        App::start(AppOptions {
            config_path: Some(path.clone()),
            secrets: secrets.clone(),
            overrides: Overrides {
                destination_url: Some(dest.into()),
                ..overrides("127.0.0.1:0")
            },
        })
    };
    // Another server: the Twitch key must not go there.
    let app = start("rtmp://127.0.0.1:1/test").await.unwrap();
    assert!(
        app.needs_setup(),
        "the stored key would be sent to another server"
    );
    let (_, cfg) = http(&app, "GET", "/api/v1/config", "").await;
    assert_eq!(cfg["destination_key_set"], false);
    app.shutdown().await;
    // Twitch over RTMPS is the same service: the key applies.
    let app = start("rtmps://live.twitch.tv:443/app").await.unwrap();
    assert!(!app.needs_setup());
    app.shutdown().await;
}

#[tokio::test]
async fn command_line_settings_are_not_saved_with_dashboard_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut c = Config::default();
    c.api.token = "0123456789abcdef".into();
    c.save(&path).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let app = App::start(AppOptions {
        config_path: Some(path.clone()),
        secrets: secrets.clone(),
        overrides: Overrides {
            destination_url: Some("rtmp://127.0.0.1:1/test".into()),
            max_delay_seconds: Some(60),
            token: Some("command-line-token-0123".into()),
            ..overrides("127.0.0.1:0")
        },
    })
    .await
    .unwrap();
    // A change from the dashboard, unrelated to the overridden settings.
    let (s, body) = http(&app, "PUT", "/api/v1/config", r#"{"grace_seconds": 45}"#).await;
    assert_eq!(s, 200, "{body}");
    let saved = Config::load_or_create(&path).unwrap();
    assert_eq!(saved.ingest.grace_seconds, 45, "the change was not saved");
    assert_eq!(saved.destination, Config::default().destination);
    assert_eq!(saved.delay.max_seconds, 120);
    assert_eq!(saved.api.token, "0123456789abcdef");
    // Still in effect for this run.
    let running = app.config();
    assert_eq!(running.destination.url, "rtmp://127.0.0.1:1/test");
    assert_eq!(running.delay.max_seconds, 60);
    assert_eq!(running.ingest.grace_seconds, 45);
    // The dashboard saves whole sections, overridden values included; only what
    // changed is saved.
    let mut delay = serde_json::to_value(&running.delay).unwrap();
    delay["presets"][1]["seconds"] = 7.into();
    let body = serde_json::json!({ "delay": delay, "grace_seconds": 45, "allow_lan": false });
    let (s, body) = http(&app, "PUT", "/api/v1/config", &body.to_string()).await;
    assert_eq!(s, 200, "{body}");
    let saved = Config::load_or_create(&path).unwrap();
    assert_eq!(
        saved.delay.presets[1].seconds, 7.0,
        "the change was not saved"
    );
    assert_eq!(
        saved.delay.max_seconds, 120,
        "a command-line value was saved"
    );
    assert_eq!(app.config().delay.max_seconds, 60);
    // Setting an overridden value from the dashboard saves it.
    let body = serde_json::json!({ "destination": {
        "service": "custom", "url": "rtmp://127.0.0.1:2/other", "key_mode": "stored" } });
    let (s, _) = http(&app, "PUT", "/api/v1/config", &body.to_string()).await;
    assert_eq!(s, 200);
    let saved = Config::load_or_create(&path).unwrap();
    assert_eq!(saved.destination.url, "rtmp://127.0.0.1:2/other");
    app.shutdown().await;
}
