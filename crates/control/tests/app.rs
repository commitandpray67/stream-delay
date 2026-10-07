//! `App::start`: settings migration and the ingest key for network-reachable inputs.

use std::sync::Arc;
use std::time::Duration;

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
async fn a_saved_destination_this_version_refuses_does_not_keep_it_from_starting() {
    // Older versions took a port of 0, for one; a hand edit can hold anything.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut c = Config::default();
    c.api.token = "0123456789abcdef".into();
    c.destination.service = "custom".into();
    c.destination.url = "rtmp://ingest.example.net:0/live".into();
    c.save(&path).unwrap();
    let app = App::start(AppOptions {
        config_path: Some(path.clone()),
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .expect("an invalid saved destination kept stream-delay from starting");
    // Nothing is sent to it. The Setup tab shows it, and says what is wrong
    // when it is saved as it is.
    let (_, state) = http(&app, "GET", "/api/v1/state", "").await;
    assert_eq!(state["egress"]["status"], "disabled");
    let (_, cfg) = http(&app, "GET", "/api/v1/config", "").await;
    assert_eq!(
        cfg["config"]["destination"]["url"],
        "rtmp://ingest.example.net:0/live"
    );
    let body = serde_json::json!({ "destination": cfg["config"]["destination"] });
    let (s, body) = http(&app, "PUT", "/api/v1/config", &body.to_string()).await;
    assert_eq!(s, 400, "{body}");
    assert!(body.to_string().contains("port"), "{body}");
    // Other settings can still be changed meanwhile.
    let (s, body) = http(&app, "PUT", "/api/v1/config", r#"{"grace_seconds": 45}"#).await;
    assert_eq!(s, 200, "{body}");
    // A valid one takes effect: with its key, the relay is waiting for a stream.
    let body = serde_json::json!({ "destination": {
        "service": "custom", "url": "rtmp://127.0.0.1:1/live/its-key", "key_mode": "stored" } });
    let (s, body) = http(&app, "PUT", "/api/v1/config", &body.to_string()).await;
    assert_eq!(s, 200, "{body}");
    let (_, state) = http(&app, "GET", "/api/v1/state", "").await;
    assert_eq!(state["egress"]["status"], "idle");
    assert_eq!(state["egress"]["destination"], "rtmp://127.0.0.1:1/live");
    app.shutdown().await;
    // One given on the command line is a mistake to point out at once.
    let started = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: Overrides {
            destination_url: Some("rtmp://ingest.example.net:0/live".into()),
            ..overrides("127.0.0.1:0")
        },
    })
    .await;
    assert!(
        matches!(&started, Err(AppError::Relay(_))),
        "{:?}",
        started.as_ref().err()
    );
}

#[tokio::test]
async fn without_a_stream_key_the_state_says_nothing_can_be_sent() {
    // As installed: Twitch, and no key yet.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut c = Config::default();
    c.api.token = "0123456789abcdef".into();
    c.save(&path).unwrap();
    let app = App::start(AppOptions {
        config_path: Some(path),
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    let (_, state) = http(&app, "GET", "/api/v1/state", "").await;
    assert_eq!(state["egress"]["status"], "disabled", "{state}");
    assert_eq!(
        state["egress"]["destination"], "rtmps://live.twitch.tv/app",
        "{state}"
    );
    let (s, body) = http(
        &app,
        "PUT",
        "/api/v1/destination/key",
        r#"{"key": "live_123"}"#,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    let (_, state) = http(&app, "GET", "/api/v1/state", "").await;
    assert_eq!(state["egress"]["status"], "idle", "{state}");
    app.shutdown().await;
}

#[tokio::test]
async fn a_key_in_a_saved_destination_that_does_not_parse_is_never_shown() {
    // A hand edit: the key after the application stays in the URL, since only
    // a URL that parses can have it moved to the secret store.
    const KEY: &str = "sk_hand_edited_0123456789";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut c = Config::default();
    c.api.token = "0123456789abcdef".into();
    c.destination.service = "custom".into();
    c.destination.url = format!("rtmp://ingest.example.net:0/live/{KEY}");
    c.save(&path).unwrap();
    let app = App::start(AppOptions {
        config_path: Some(path.clone()),
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    let (_, cfg) = http(&app, "GET", "/api/v1/config", "").await;
    assert_eq!(
        cfg["config"]["destination"]["url"],
        "rtmp://ingest.example.net:0/live/…"
    );
    for path in ["/api/v1/config", "/api/v1/state", "/api/v1/diagnostics"] {
        let (_, body) = http(&app, "GET", path, "").await;
        assert!(!body.to_string().contains(KEY), "{path}: {body}");
    }
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

#[tokio::test]
async fn the_relay_starts_with_the_settings_given() {
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: Overrides {
            destination_url: Some("rtmp://127.0.0.1:1/test".into()),
            max_delay_seconds: Some(60),
            // To the millisecond, as a delay set from the dock is: 2.01 × 1000
            // comes out just under 2010.
            start_delay_seconds: Some(2.01),
            ..overrides("127.0.0.1:0")
        },
    })
    .await
    .unwrap();
    let mut state = app.relay().subscribe();
    let s = tokio::time::timeout(
        Duration::from_secs(5),
        state.wait_for(|s| s.delay.target_ms == 2010),
    )
    .await
    .expect("the start delay was not applied")
    .unwrap()
    .clone();
    assert_eq!(s.delay.max_delay_ms, 60_000);
    assert_eq!(
        s.egress.destination.as_deref(),
        Some("rtmp://127.0.0.1:1/test")
    );
    app.shutdown().await;
}

#[tokio::test]
async fn a_command_line_key_only_goes_to_its_server() {
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: Overrides {
            destination_url: Some("rtmp://127.0.0.1:1/test".into()),
            destination_key: Some("cli-key-12345".into()),
            ..overrides("127.0.0.1:0")
        },
    })
    .await
    .unwrap();
    assert!(!app.needs_setup(), "the command-line key is not used");
    let (_, cfg) = http(&app, "GET", "/api/v1/config", "").await;
    assert_eq!(cfg["destination_key_set"], true);
    let dest = |url: &str| {
        serde_json::json!({ "destination": {
            "service": "custom", "url": url, "key_mode": "stored" } })
        .to_string()
    };
    // Another server, set from the dashboard: the key must not go there.
    let (s, body) = http(
        &app,
        "PUT",
        "/api/v1/config",
        &dest("rtmp://127.0.0.1:2/other"),
    )
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["destination_key_set"], false);
    assert!(app.needs_setup());
    // Back to the server it was given for.
    let (s, body) = http(
        &app,
        "PUT",
        "/api/v1/config",
        &dest("rtmp://127.0.0.1:1/test"),
    )
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["destination_key_set"], true);
    app.shutdown().await;
}

#[tokio::test]
async fn a_key_left_in_the_destination_url_is_kept_unless_one_is_saved() {
    for (stored, kept) in [
        // A saved key took precedence over the one in the URL, and still does.
        ("stored-key-5678", "stored-key-5678"),
        // An empty one did not.
        ("", "url-key-1234"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.api.token = "0123456789abcdef".into();
        c.destination.service = "custom".into();
        c.destination.url = "rtmp://a.rtmp.youtube.com/live2/url-key-1234".into();
        c.save(&path).unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        secrets.set(secret::DESTINATION_KEY, stored).unwrap();
        let app = App::start(AppOptions {
            config_path: Some(path.clone()),
            secrets: secrets.clone(),
            overrides: overrides("127.0.0.1:0"),
        })
        .await
        .unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&secrets.get(secret::DESTINATION_KEY).unwrap()).unwrap();
        assert_eq!(saved["value"], kept, "{stored:?}");
        assert_eq!(saved["for"], "rtmp://a.rtmp.youtube.com/live2");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("url-key-1234"), "key still in config.toml");
        app.shutdown().await;
    }
}

#[tokio::test]
async fn presets_from_hotkeys_set_the_delay() {
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    let presets = app.config().delay.presets;
    assert_eq!(presets[0].seconds, 0.0);
    let mut state = app.relay().subscribe();
    let mut target = async |ms: u64| {
        tokio::time::timeout(
            Duration::from_secs(5),
            state.wait_for(|s| s.delay.target_ms == ms),
        )
        .await
        .unwrap_or_else(|_| panic!("the delay did not become {ms} ms"))
        .unwrap();
    };
    app.apply_preset(1).await.unwrap();
    target((presets[1].seconds * 1000.0).round() as u64).await;
    // One that does not exist changes nothing.
    app.apply_preset(presets.len()).await.unwrap();
    app.apply_preset(0).await.unwrap();
    target(0).await;
    app.shutdown().await;
}

#[tokio::test]
async fn the_dashboard_starts_the_desktop_apps_update_check() {
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c = checked.clone();
    app.on_update_check(move || c.store(true, std::sync::atomic::Ordering::SeqCst));
    let (s, body) = http(&app, "POST", "/api/v1/updates/check", "").await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["checking"], true);
    assert!(checked.load(std::sync::atomic::Ordering::SeqCst));
    app.shutdown().await;
}

#[tokio::test]
async fn shutting_down_closes_the_rtmp_input() {
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    let ingest = app.relay().ingest_addr();
    tokio::net::TcpStream::connect(ingest).await.unwrap();
    app.shutdown().await;
    // The input stops accepting soon after, not necessarily at once.
    let closed = async {
        while tokio::net::TcpStream::connect(ingest).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), closed)
        .await
        .expect("still taking encoders");
}

type Events = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// The live updates the dashboard (`role` empty), dock or overlay page gets.
async fn events(app: &App, token: &str, role: &str) -> Events {
    let addr = app.api_addr;
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let url = format!("ws://{addr}/api/v1/events?token={token}{role}");
    tokio_tungstenite::client_async(url, tcp).await.unwrap().0
}

/// The next update `wanted` takes, skipping others.
async fn next(ws: &mut Events, wanted: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let read = async {
        loop {
            if let Message::Text(t) = ws.next().await.unwrap().unwrap() {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if wanted(&v) {
                    return v;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("no such update")
}

fn of_type(t: &'static str) -> impl Fn(&serde_json::Value) -> bool {
    move |v| v["type"] == t
}

#[tokio::test]
async fn live_updates_carry_what_each_link_may_see() {
    use streamdelay_control::{Scope, scoped_token};
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    let admin = app.config().api.token;
    // On connecting: the settings, the state, and how many overlay pages are open.
    let mut dashboard = events(&app, &admin, "").await;
    let first = next(&mut dashboard, |_| true).await;
    assert_eq!(first["type"], "config");
    assert_eq!(first["config"]["scope"], "admin");
    assert!(
        first["config"]["config"]["destination"].is_object(),
        "{first}"
    );
    assert_eq!(next(&mut dashboard, |_| true).await["type"], "state");
    let count = next(&mut dashboard, |_| true).await;
    assert_eq!(
        count,
        serde_json::json!({"type": "overlays", "count": 0, "active": 0})
    );

    // The overlay page sees only what it shows, and is counted while open.
    let mut overlay = events(&app, &scoped_token(&admin, Scope::Read), "&role=overlay").await;
    let config = next(&mut overlay, of_type("config")).await;
    assert_eq!(config["config"]["scope"], "read");
    assert!(
        config["config"]["config"]["destination"].is_null(),
        "{config}"
    );
    assert!(
        config["config"]["config"]["overlay"].is_object(),
        "{config}"
    );
    let count = next(&mut dashboard, of_type("overlays")).await;
    assert_eq!(count["count"], 1);
    drop(overlay);
    let count = next(&mut dashboard, of_type("overlays")).await;
    assert_eq!(count["count"], 0);

    // Changes reach every open link.
    let mut dock = events(&app, &scoped_token(&admin, Scope::Control), "").await;
    next(&mut dock, of_type("overlays")).await;
    let (s, _) = http(&app, "PUT", "/api/v1/config", r#"{"grace_seconds": 45}"#).await;
    assert_eq!(s, 200);
    let config = next(&mut dashboard, of_type("config")).await;
    assert_eq!(config["config"]["config"]["ingest"]["grace_seconds"], 45);
    let (s, _) = http(&app, "PUT", "/api/v1/delay", r#"{"seconds": 4}"#).await;
    assert_eq!(s, 200);
    next(&mut dock, |v| v["state"]["delay"]["target_ms"] == 4000).await;
    app.shutdown().await;
}

async fn say(ws: &mut Events, text: &str) {
    use futures_util::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn only_an_overlay_page_obs_says_is_on_stream_counts_as_covering() {
    use streamdelay_control::{Scope, scoped_token};
    let app = App::start(AppOptions {
        config_path: None,
        secrets: Arc::new(MemorySecrets::default()),
        overrides: overrides("127.0.0.1:0"),
    })
    .await
    .unwrap();
    let admin = app.config().api.token;
    let read = scoped_token(&admin, Scope::Read);
    let mut dashboard = events(&app, &admin, "").await;
    next(&mut dashboard, of_type("overlays")).await;
    let counts = |v: serde_json::Value| (v["count"].clone(), v["active"].clone());

    // Connected, but OBS has not said it is on stream: it does not count yet.
    let mut overlay = events(&app, &read, "&role=overlay").await;
    let v = next(&mut dashboard, of_type("overlays")).await;
    assert_eq!(counts(v), (1.into(), 0.into()));
    say(&mut overlay, r#"{"type":"overlay","active":true}"#).await;
    let v = next(&mut dashboard, of_type("overlays")).await;
    assert_eq!(counts(v), (1.into(), 1.into()));
    say(&mut overlay, r#"{"type":"overlay","active":false}"#).await;
    let v = next(&mut dashboard, of_type("overlays")).await;
    assert_eq!(counts(v), (1.into(), 0.into()));

    // Only overlay pages are listened to; junk and floods are ignored.
    let dock_token = scoped_token(&admin, Scope::Control);
    let mut dock = events(&app, &dock_token, "").await;
    say(&mut dock, r#"{"type":"overlay","active":true}"#).await;
    say(&mut dashboard, r#"{"type":"overlay","active":true}"#).await;
    say(&mut overlay, r#"{"type":"overlay","active":"yes"}"#).await;
    say(&mut overlay, "not json").await;
    for _ in 0..10 {
        say(&mut overlay, r#"{"type":"slate-shown","change":0}"#).await;
    }
    // Its 13th message in a second: ignored.
    say(&mut overlay, r#"{"type":"overlay","active":true}"#).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut probe = events(&app, &dock_token, "").await;
    let v = next(&mut probe, of_type("overlays")).await;
    assert_eq!(counts(v), (1.into(), 0.into()));
    tokio::time::sleep(Duration::from_secs(1)).await;
    say(&mut overlay, r#"{"type":"overlay","active":true}"#).await;
    let v = next(&mut probe, of_type("overlays")).await;
    assert_eq!(counts(v), (1.into(), 1.into()));

    // Leaving takes it off.
    drop(overlay);
    let v = next(&mut probe, of_type("overlays")).await;
    assert_eq!(counts(v), (0.into(), 0.into()));
    app.shutdown().await;
}
