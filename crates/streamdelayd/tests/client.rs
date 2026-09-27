//! The command-line client: what it sends and prints, against a stand-in control
//! API, and a round trip against a running instance.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use serde_json::{Value, json};
use streamdelay_config::{Config, MemorySecrets};
use streamdelay_control::{App, AppOptions, Overrides};

/// A request the stand-in API received.
#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    path: String,
    auth: Option<String>,
    body: Value,
}

/// A control API that answers every request with the same status and body, and
/// keeps what it was sent.
struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    _rt: tokio::runtime::Runtime,
}

impl Fake {
    fn new(status: StatusCode, reply: &'static str) -> Fake {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let log = log.clone();
                async move {
                    log.lock().unwrap().push(Seen {
                        method,
                        path: uri.path().to_string(),
                        auth: headers
                            .get(header::AUTHORIZATION)
                            .map(|v| v.to_str().unwrap().to_string()),
                        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                    });
                    (status, [(header::CONTENT_TYPE, "application/json")], reply)
                }
            },
        );
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
        Fake { url, seen, _rt: rt }
    }

    fn ok(reply: &'static str) -> Fake {
        Fake::new(StatusCode::OK, reply)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn only(&self) -> Seen {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        seen[0].clone()
    }
}

/// `streamdelayd` with no token or settings file from the machine running the
/// tests.
fn cli(no_config: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_streamdelayd"));
    c.env_remove("STREAMDELAY_TOKEN")
        .env("STREAMDELAY_CONFIG", no_config.join("none.toml"));
    c
}

fn run(c: &mut Command) -> Output {
    c.output().expect("could not run streamdelayd")
}

fn stdout(o: &Output) -> String {
    assert!(
        o.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn stderr(o: &Output) -> String {
    assert!(
        !o.status.success(),
        "succeeded: {}",
        String::from_utf8_lossy(&o.stdout)
    );
    String::from_utf8(o.stderr.clone()).unwrap()
}

#[test]
fn a_dump_says_what_viewers_see() {
    let dir = tempfile::tempdir().unwrap();
    let cases: [(&str, &str); 5] = [
        (
            r#"{"target_ms":30000,"effective_ms":30000,"pending":false,"history_short":false,"dump":"replay"}"#,
            "Viewers see the last 30 s again",
        ),
        (
            r#"{"target_ms":30000,"effective_ms":0,"pending":true,"history_short":false,"dump":"cover"}"#,
            "The overlay slate covers the stream",
        ),
        (
            r#"{"target_ms":29500,"effective_ms":0,"pending":true,"history_short":false,"dump":"hold"}"#,
            "No overlay is connected, so viewers see the last frame, still, for about 30 s",
        ),
        // Before or at the end of a broadcast nothing is held, nothing more airs.
        (
            r#"{"target_ms":30000,"effective_ms":0,"pending":false,"history_short":false,"dump":"hold"}"#,
            "Nothing that was waiting will air",
        ),
        // An older instance that does not say.
        (
            r#"{"target_ms":30000,"effective_ms":0,"pending":false,"history_short":false}"#,
            "Nothing that was waiting will air",
        ),
    ];
    for (reply, says) in cases {
        let api = Fake::ok(reply);
        let out = stdout(&run(cli(dir.path())
            .args(["dump", "--url", &api.url])
            .env("STREAMDELAY_TOKEN", "env-token")));
        assert!(out.contains(says), "{reply}: {out}");
        let req = api.only();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.path, "/api/v1/stream/dump");
        assert_eq!(req.auth.as_deref(), Some("Bearer env-token"));
        // The settings decide how, and never to air it uncovered.
        assert_eq!(req.body, json!({}));
    }
    let api = Fake::ok(
        r#"{"target_ms":30000,"effective_ms":0,"pending":true,"history_short":false,"dump":"cover"}"#,
    );
    stdout(&run(
        cli(dir.path()).args(["dump", "--mask", "--url", &api.url, "--token", "t"])
    ));
    assert_eq!(api.only().body, json!({ "mode": "mask" }));
}

#[test]
fn the_token_given_wins_over_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let api = Fake::ok("{}");
    stdout(&run(cli(dir.path())
        .args(["state", "--url", &api.url, "--token", "flag-token"])
        .env("STREAMDELAY_TOKEN", "env-token")));
    assert_eq!(api.only().auth.as_deref(), Some("Bearer flag-token"));
}

#[test]
fn without_a_token_nothing_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let api = Fake::ok("{}");
    let err = stderr(&run(cli(dir.path()).args(["state", "--url", &api.url])));
    assert!(err.contains("no API token"), "{err}");
    assert!(api.seen().is_empty());
}

#[test]
fn the_settings_file_says_where_and_with_what_token() {
    let dir = tempfile::tempdir().unwrap();
    let api = Fake::ok(r#"{"ended":false}"#);
    let mut c = Config::default();
    c.api.bind = api.url.trim_start_matches("http://").parse().unwrap();
    c.api.token = "token-from-the-file".into();
    let path = dir.path().join("config.toml");
    c.save(&path).unwrap();
    let out = stdout(&run(cli(dir.path()).args(["state", "--config"]).arg(&path)));
    let req = api.only();
    assert_eq!(
        (req.method, req.path.as_str()),
        (Method::GET, "/api/v1/state")
    );
    assert_eq!(req.auth.as_deref(), Some("Bearer token-from-the-file"));
    let printed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(printed, json!({ "ended": false }));
}

#[test]
fn stream_commands_send_what_the_api_expects() {
    let dir = tempfile::tempdir().unwrap();
    let cases: [(&[&str], Method, &str, Value); 7] = [
        (
            &["delay", "12.5", "--mask"],
            Method::PUT,
            "/api/v1/delay",
            json!({ "seconds": 12.5, "mode": "mask" }),
        ),
        (
            &["delay", "0"],
            Method::PUT,
            "/api/v1/delay",
            json!({ "seconds": 0.0 }),
        ),
        (
            &["live"],
            Method::POST,
            "/api/v1/live",
            json!({ "when": "now" }),
        ),
        (
            &["live", "--after-air"],
            Method::POST,
            "/api/v1/live",
            json!({ "when": "after-air" }),
        ),
        (
            &["end"],
            Method::POST,
            "/api/v1/stream/end",
            json!({ "when": "now" }),
        ),
        (
            &["end", "--after-air"],
            Method::POST,
            "/api/v1/stream/end",
            json!({ "when": "after-air" }),
        ),
        (
            &["resume"],
            Method::POST,
            "/api/v1/stream/resume",
            json!({}),
        ),
    ];
    for (args, method, path, body) in cases {
        let api = Fake::ok(r#"{"ending":false}"#);
        stdout(&run(cli(dir.path())
            .args(args)
            .args(["--url", &api.url, "--token", "t"])));
        let req = api.only();
        assert_eq!(
            (&req.method, req.path.as_str()),
            (&method, path),
            "{args:?}"
        );
        assert_eq!(req.body, body, "{args:?}");
    }
    // What `end` prints depends on whether the broadcast ends now.
    for (reply, says) in [
        (r#"{"ending":true}"#, "ends once what is buffered has aired"),
        (r#"{"ending":false}"#, "Nothing buffered will air"),
    ] {
        let api = Fake::ok(reply);
        let out = stdout(&run(
            cli(dir.path()).args(["end", "--url", &api.url, "--token", "t"])
        ));
        assert!(out.contains(says), "{reply}: {out}");
    }
}

#[test]
fn diagnostics_can_be_saved_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let api = Fake::ok(r#"{"logs":["started"]}"#);
    let file = dir.path().join("diag.json");
    let out = stdout(&run(cli(dir.path())
        .args(["diagnostics", "--url", &api.url, "--token", "t", "-o"])
        .arg(&file)));
    assert!(out.contains("Saved diagnostics"), "{out}");
    assert_eq!(api.only().path, "/api/v1/diagnostics");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved, json!({ "logs": ["started"] }));
}

#[test]
fn api_errors_show_their_status_and_message() {
    let dir = tempfile::tempdir().unwrap();
    let api = Fake::new(
        StatusCode::BAD_REQUEST,
        r#"{"error":"there is no delay, so nothing is waiting to air"}"#,
    );
    let err = stderr(&run(
        cli(dir.path()).args(["dump", "--url", &api.url, "--token", "t"])
    ));
    assert!(
        err.contains("400 Bad Request: there is no delay, so nothing is waiting to air"),
        "{err}"
    );
    // A body that is not the API's JSON.
    let api = Fake::new(StatusCode::BAD_GATEWAY, "<html>proxy error</html>");
    let err = stderr(&run(
        cli(dir.path()).args(["state", "--url", &api.url, "--token", "t"])
    ));
    assert!(err.contains("502 Bad Gateway: request failed"), "{err}");
    // Nothing listening.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let err = stderr(&run(
        cli(dir.path()).args(["state", "--url", &url, "--token", "t"])
    ));
    assert!(!err.trim().is_empty());
}

/// The client and a real instance agree on paths, bodies and answers.
#[test]
fn round_trip_with_a_running_instance() {
    const TOKEN: &str = "cli-test-token-0123456789";
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let app = rt
        .block_on(App::start(AppOptions {
            config_path: None,
            secrets: Arc::new(MemorySecrets::default()),
            overrides: Overrides {
                ingest: Some("127.0.0.1:0".parse().unwrap()),
                api: Some("127.0.0.1:0".parse().unwrap()),
                token: Some(TOKEN.into()),
                ..Default::default()
            },
        }))
        .unwrap();
    let url = format!("http://{}", app.api_addr);
    let sd = |args: &[&str]| {
        run(cli(dir.path())
            .args(args)
            .args(["--url", &url])
            .env("STREAMDELAY_TOKEN", TOKEN))
    };

    let state: Value = serde_json::from_str(&stdout(&sd(&["state"]))).unwrap();
    assert_eq!(state["ended"], false);
    assert_eq!(state["delay"]["target_ms"], 0);

    let err = stderr(&sd(&["delay", "100000"]));
    assert!(err.contains("400 Bad Request"), "{err}");
    assert!(err.contains("exceeds the maximum"), "{err}");

    let ack: Value = serde_json::from_str(&stdout(&sd(&["delay", "5"]))).unwrap();
    assert_eq!(ack["target_ms"], 5000);
    let state: Value = serde_json::from_str(&stdout(&sd(&["state"]))).unwrap();
    assert_eq!(state["delay"]["target_ms"], 5000);

    // Before a broadcast starts, a dump drops what is buffered and holds
    // nothing. The instance's answer reads as that outcome.
    let out = stdout(&sd(&["dump"]));
    assert_eq!(out.trim(), "Dumped. Nothing that was waiting will air.");

    let err = stderr(&run(cli(dir.path()).args([
        "state",
        "--url",
        &url,
        "--token",
        "not-the-token-at-all",
    ])));
    assert!(err.contains("401"), "{err}");

    rt.block_on(app.shutdown());
}
