//! The command-line client: what it sends and prints, against a stand-in control
//! API, and a round trip against a running instance.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use serde_json::{Value, json};
use streamdelay_config::{Config, MemorySecrets};
use streamdelay_control::{App, AppOptions, Overrides, Scope, scoped_token};

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

/// A `streamdelayd run` process on a free port, stopped when dropped.
struct Daemon {
    child: std::process::Child,
    url: String,
}

impl Daemon {
    /// Starts it with `setup`'s arguments and environment, and waits until
    /// `streamdelayd health` says it answers.
    fn start(no_config: &Path, setup: impl FnOnce(&mut Command)) -> Daemon {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = free.local_addr().unwrap();
        drop(free);
        let mut c = cli(no_config);
        c.args(["run", "--ingest", "127.0.0.1:0", "--api"])
            .arg(api.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        setup(&mut c);
        let mut daemon = Daemon {
            child: c.spawn().unwrap(),
            url: format!("http://{api}"),
        };
        let started = Instant::now();
        loop {
            let health = run(cli(no_config).args(["health", "--url", &daemon.url]));
            if health.status.success() {
                return daemon;
            }
            if let Some(status) = daemon.child.try_wait().unwrap() {
                panic!("streamdelayd run exited: {status}");
            }
            if started.elapsed() > Duration::from_secs(30) {
                daemon.stop();
                panic!("streamdelayd run never answered");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `streamdelayd` with no token or settings file from the machine running the
/// tests.
fn cli(no_config: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_streamdelayd"));
    c.env_remove("STREAMDELAY_TOKEN")
        .env_remove("STREAMDELAY_TOKEN_FILE")
        .env_remove("STREAMDELAY_INGEST_KEY")
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
            "No overlay is on stream, so viewers see the last frame, still, for about 30 s",
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
fn a_token_file_wins_over_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("token");
    std::fs::write(&file, "file-token\n").unwrap();
    let api = Fake::ok("{}");
    stdout(&run(cli(dir.path())
        .args(["state", "--url", &api.url, "--token-file"])
        .arg(&file)
        .env("STREAMDELAY_TOKEN", "env-token")));
    assert_eq!(api.only().auth.as_deref(), Some("Bearer file-token"));
    // A file that is not there is an error, not a fallback to another token.
    let err = stderr(&run(cli(dir.path())
        .args(["state", "--url", &api.url, "--token-file"])
        .arg(dir.path().join("missing"))
        .env("STREAMDELAY_TOKEN", "env-token")));
    assert!(err.contains("reading the token file"), "{err}");
    assert_eq!(api.seen().len(), 1);
}

#[test]
fn requests_to_this_computer_never_go_through_a_proxy() {
    // A proxy set for this computer's other traffic (as in a company network,
    // or passed into containers by Docker) cannot reach its loopback, and would
    // see the token: like browsers, the client goes to this computer directly.
    let dir = tempfile::tempdir().unwrap();
    let api = Fake::ok(r#"{"status":"ok","app":"stream-delay","version":"9.9.9"}"#);
    let proxy = Fake::ok("{}");
    let proxied = |c: &mut Command| {
        for v in ["ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY"] {
            c.env(v, &proxy.url).env(v.to_lowercase(), &proxy.url);
        }
        c.env_remove("NO_PROXY").env_remove("no_proxy");
    };
    let mut state = cli(dir.path());
    state.args(["state", "--url", &api.url, "--token", "t"]);
    proxied(&mut state);
    stdout(&run(&mut state));
    let mut health = cli(dir.path());
    health.args(["health", "--url", &api.url]);
    proxied(&mut health);
    stdout(&run(&mut health));
    assert!(proxy.seen().is_empty(), "{:?}", proxy.seen());
    assert_eq!(api.seen().len(), 2);
}

#[test]
fn health_says_whether_stream_delay_answers() {
    let dir = tempfile::tempdir().unwrap();
    let api = Fake::ok(r#"{"status":"ok","app":"stream-delay","version":"9.9.9"}"#);
    let out = stdout(&run(cli(dir.path()).args(["health", "--url", &api.url])));
    assert_eq!(
        out.trim(),
        format!("stream-delay 9.9.9 is running at {}", api.url)
    );
    let req = api.only();
    assert_eq!((req.method, req.path.as_str()), (Method::GET, "/healthz"));
    // No token is sent, even one at hand.
    assert_eq!(req.auth, None);

    // Something else on the port.
    let other = Fake::ok(r#"{"status":"ok"}"#);
    let err = stderr(&run(cli(dir.path()).args(["health", "--url", &other.url])));
    assert!(err.contains("not as stream-delay"), "{err}");
    let failing = Fake::new(StatusCode::SERVICE_UNAVAILABLE, "{}");
    let err = stderr(&run(cli(dir.path()).args([
        "health",
        "--url",
        &failing.url,
    ])));
    assert!(err.contains("503"), "{err}");
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let out = run(cli(dir.path()).args(["health", "--url", &url]));
    assert_eq!(out.status.code(), Some(1));
}

/// `streamdelayd run` with its token in a file, checked the way the Docker
/// image's health check does, then used by the client.
#[test]
fn run_reads_its_token_from_a_file() {
    const TOKEN: &str = "run-token-from-a-file-0123";
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("token");
    std::fs::write(&file, format!("{TOKEN}\n")).unwrap();
    let mut daemon = Daemon::start(dir.path(), |c| {
        c.arg("--ephemeral")
            .arg("--token-file")
            .arg(&file)
            // Not the token: the file wins.
            .env("STREAMDELAY_TOKEN", "not-the-token-0123456789");
    });
    let url = daemon.url.clone();
    let state = run(cli(dir.path()).args(["state", "--url", &url, "--token", TOKEN]));
    let refused = run(cli(dir.path()).args([
        "state",
        "--url",
        &url,
        "--token",
        "not-the-token-0123456789",
    ]));
    daemon.stop();
    let state: Value = serde_json::from_str(&stdout(&state)).unwrap();
    assert_eq!(state["ended"], false);
    assert!(stderr(&refused).contains("401"));
}

/// As in the Docker image: `run` gets its token file and ingest key from the
/// environment, which `docker exec streamdelayd urls` inherits. The links it
/// prints must work, and the OBS key must be the one encoders need.
#[test]
fn urls_show_what_run_was_given() {
    const TOKEN: &str = "urls-token-from-a-file-0123";
    const INGEST_KEY: &str = "ingest-key-from-the-env-4567";
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let file = dir.path().join("token");
    std::fs::write(&file, format!("{TOKEN}\n")).unwrap();
    let env = |c: &mut Command| {
        c.env("STREAMDELAY_TOKEN_FILE", &file)
            .env("STREAMDELAY_INGEST_KEY", INGEST_KEY)
            .env("STREAMDELAY_CONFIG", &config);
    };
    let mut daemon = Daemon::start(dir.path(), |c| {
        c.arg("--no-keychain");
        env(c);
    });
    let mut urls = cli(dir.path());
    urls.arg("urls");
    env(&mut urls);
    let shown = run(&mut urls);
    let links = stdout(&shown);
    let token = links
        .lines()
        .find_map(|l| l.strip_prefix("Dashboard:"))
        .and_then(|l| l.split_once("token="))
        .map(|(_, t)| t.trim().to_string())
        .unwrap_or_else(|| panic!("no dashboard link: {links}"));
    let state = run(cli(dir.path()).args(["state", "--url", &daemon.url, "--token", &token]));
    daemon.stop();
    assert_eq!(token, TOKEN);
    assert!(
        links.contains(&format!("OBS key:     {INGEST_KEY}")),
        "{links}"
    );
    for (page, scope) in [("dock", Scope::Control), ("overlay", Scope::Read)] {
        let link = format!("/{page}?token={}", scoped_token(TOKEN, scope));
        assert!(links.contains(&link), "{link} not in {links}");
    }
    stdout(&state);

    // Without them, what the settings file says: its own token, and no key for
    // an input only this computer can reach.
    let saved = Config::load_or_create(&config).unwrap();
    let plain = stdout(&run(cli(dir.path())
        .args(["urls", "--config"])
        .arg(&config)));
    assert!(
        plain.contains(&format!("/?token={}", saved.api.token)),
        "{plain}"
    );
    assert!(plain.contains("OBS key:     any"), "{plain}");
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

    let out = stdout(&sd(&["health"]));
    assert!(
        out.contains(&format!("stream-delay {}", env!("CARGO_PKG_VERSION"))),
        "{out}"
    );

    rt.block_on(app.shutdown());
}

/// What `run` printed as it started: the lines up to the one with `until`,
/// and any that follow at once.
fn printed(daemon: &mut Daemon, until: &str) -> String {
    use std::io::BufRead;
    let out = daemon.child.stdout.take().expect("stdout is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut text = String::new();
    let mut wait = Duration::from_secs(10);
    while let Ok(line) = rx.recv_timeout(wait) {
        if line.contains(until) {
            wait = Duration::from_millis(500);
        }
        text.push_str(&line);
        text.push('\n');
    }
    text
}

#[test]
fn an_ephemeral_run_prints_its_links_with_their_tokens() {
    // It has no settings file for `streamdelayd urls` to read: what it prints
    // is the only way to its token, whether to a terminal or not.
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(dir.path(), |c| {
        c.arg("--ephemeral").stdout(Stdio::piped());
    });
    let out = printed(&mut daemon, "Overlay:");
    daemon.stop();
    assert!(
        out.lines()
            .any(|l| l.starts_with("Dashboard:") && l.contains("/?token=")),
        "{out}"
    );
    assert!(!out.contains("not written to logs"), "{out}");
}

#[test]
fn run_keeps_the_tokens_and_the_obs_key_out_of_logs() {
    // Printed to a pipe (a log, `docker logs`) rather than a terminal, with a
    // settings file that `streamdelayd urls` reads instead.
    const INGEST_KEY: &str = "ingest-key-kept-out-of-logs-0123";
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(dir.path(), |c| {
        c.arg("--no-keychain")
            .env("STREAMDELAY_INGEST_KEY", INGEST_KEY)
            .stdout(Stdio::piped());
    });
    let out = printed(&mut daemon, "not written to logs");
    daemon.stop();
    assert!(!out.contains("token="), "{out}");
    assert!(!out.contains(INGEST_KEY), "{out}");
    assert!(out.contains("OBS key:     (see below)"), "{out}");
    assert!(out.contains("`streamdelayd urls` shows them"), "{out}");
}

#[test]
fn run_takes_the_destination_key_from_the_environment() {
    const TOKEN: &str = "run-token-for-the-key-0123";
    for (key, saved) in [("live_1_from_the_env", true), ("", false)] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, TOKEN).unwrap();
        let mut daemon = Daemon::start(dir.path(), |c| {
            c.args([
                "--ephemeral",
                "--dest",
                "rtmp://127.0.0.1:9/app",
                "--token-file",
            ])
            .arg(&file)
            .env("STREAMDELAY_KEY", key);
        });
        let diagnostics =
            run(cli(dir.path()).args(["diagnostics", "--url", &daemon.url, "--token", TOKEN]));
        daemon.stop();
        let bundle: Value = serde_json::from_str(&stdout(&diagnostics)).unwrap();
        assert_eq!(
            bundle["settings"]["destination_key_set"], saved,
            "STREAMDELAY_KEY={key:?}"
        );
    }
}
