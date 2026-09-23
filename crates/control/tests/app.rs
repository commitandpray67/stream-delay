//! `App::start`: settings migration and the ingest key for network-reachable inputs.

use std::sync::Arc;

use streamdelay_config::{Config, MemorySecrets, ObsBackup, SecretStore, secret};
use streamdelay_control::{App, AppOptions, Overrides};

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
    assert_eq!(secrets.get(secret::DESTINATION_KEY).as_deref(), Some(KEY));
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
