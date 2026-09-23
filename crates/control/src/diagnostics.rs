//! Diagnostics bundle for bug reports: version, platform, settings, live state and
//! recent log lines, with every secret removed.

use std::collections::VecDeque;
use std::io;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use streamdelay_config::secret;

use crate::AppState;
use crate::auth::Scope;
use crate::settings::public_config;

/// Recent log lines kept in memory for the bundle.
const LOG_LINES: usize = 2000;

fn ring() -> &'static Mutex<VecDeque<String>> {
    static RING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(LOG_LINES)))
}

fn push_line(line: &str) {
    if let Ok(mut r) = ring().lock() {
        if r.len() == LOG_LINES {
            r.pop_front();
        }
        r.push_back(line.to_string());
    }
}

/// Recent log lines, oldest first.
pub fn recent_logs() -> Vec<String> {
    ring()
        .lock()
        .map(|r| r.iter().cloned().collect())
        .unwrap_or_default()
}

/// A `tracing_subscriber` writer that keeps recent lines for the diagnostics bundle.
pub fn log_writer() -> LogRing {
    LogRing
}

/// A formatting layer that feeds the diagnostics bundle, to add next to the normal
/// log output:
/// `registry().with(filter).with(fmt::layer()).with(diagnostics::layer()).init()`.
pub fn layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(LogRing)
}

#[derive(Clone, Copy)]
pub struct LogRing;

pub struct LogRingWriter {
    buf: Vec<u8>,
}

impl io::Write for LogRingWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            push_line(String::from_utf8_lossy(&line[..line.len() - 1]).trim_end());
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogRingWriter {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            push_line(String::from_utf8_lossy(&self.buf).trim_end());
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogRing {
    type Writer = LogRingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogRingWriter { buf: Vec::new() }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/diagnostics", get(diagnostics))
}

/// Replaces every known secret, Twitch-style key and URL token in `text`.
pub(crate) fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets.iter().filter(|s| s.len() >= 4) {
        out = out.replace(s.as_str(), "<redacted>");
    }
    // Twitch keys look like live_<digits>_<alphanumerics>; token query parameters
    // may appear in logged URLs. Words that merely contain "live_", such as the
    // setting go_live_after_air, are left alone.
    type Rule = (&'static str, fn(&str) -> bool);
    let rules: [Rule; 2] = [
        ("live_", is_twitch_key_tail),
        ("token=", |value| !value.is_empty()),
    ];
    for (prefix, looks_secret) in rules {
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(prefix) {
            result.push_str(&rest[..i + prefix.len()]);
            let tail = &rest[i + prefix.len()..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(tail.len());
            if looks_secret(&tail[..end]) {
                result.push_str("<redacted>");
                rest = &tail[end..];
            } else {
                rest = tail;
            }
        }
        result.push_str(rest);
        out = result;
    }
    // Paths under the home directory reveal the user name.
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        && let Some(home) = home.to_str()
        && home.len() > 1
    {
        out = out.replace(home, "~");
    }
    out
}

/// `<digits>_<something>`: what follows `live_` in a Twitch stream key.
fn is_twitch_key_tail(tail: &str) -> bool {
    let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && tail.len() > digits + 1 && tail.as_bytes()[digits] == b'_'
}

fn known_secrets(st: &AppState) -> Vec<String> {
    let s = &st.shared.secrets;
    let mut v: Vec<String> = [
        secret::DESTINATION_KEY,
        secret::OBS_PASSWORD,
        secret::OBS_BACKUP_KEY,
    ]
    .iter()
    .filter_map(|name| s.get(name))
    .collect();
    v.extend(st.shared.key_override.clone());
    let c = st.config();
    v.extend(c.ingest.key);
    v.extend(crate::app::split_url_key(&c.destination.url).1);
    v.extend(
        [Scope::Admin, Scope::Control, Scope::Read].map(|s| st.shared.tokens.get(s).to_string()),
    );
    v
}

pub(crate) fn bundle(st: &AppState) -> Value {
    let mut config = serde_json::to_value(public_config(st)).unwrap_or(Value::Null);
    // The links embed the access token.
    if let Some(obj) = config.as_object_mut() {
        obj.remove("urls");
    }
    let generated = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let raw = json!({
        "generated_unix": generated,
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "settings": config,
        "state": st.relay().state(),
        "logs": recent_logs(),
    });
    let text = redact(&raw.to_string(), &known_secrets(st));
    serde_json::from_str(&text).unwrap_or(raw)
}

async fn diagnostics(State(st): State<AppState>) -> impl IntoResponse {
    let body = bundle(&st);
    let name = format!("stream-delay-diagnostics-{}.json", body["generated_unix"]);
    (
        [(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        )],
        Json(body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_keys_and_tokens() {
        let secrets = vec!["hunter2-secret".to_string(), "abc".to_string()];
        let text =
            "key live_12345_AbCdEf used, pw hunter2-secret, url /dock?token=0123abcd&x=1, abc";
        let r = redact(text, &secrets);
        assert!(!r.contains("12345_AbCdEf"));
        assert!(!r.contains("hunter2-secret"));
        assert!(!r.contains("0123abcd"));
        assert!(r.contains("live_<redacted>"));
        assert!(r.contains("token=<redacted>&x=1"));
        // Too-short values are not blindly replaced everywhere.
        assert!(r.ends_with("abc"));
    }

    #[test]
    fn words_containing_live_are_not_keys() {
        let text = r#"{"go_live_after_air":"x","live_now":1,"key":"live_987_xYz"}"#;
        let r = redact(text, &[]);
        assert!(r.contains(r#""go_live_after_air":"x""#), "{r}");
        assert!(r.contains(r#""live_now":1"#), "{r}");
        assert!(r.contains(r#""key":"live_<redacted>""#), "{r}");
    }

    #[test]
    fn log_ring_keeps_lines() {
        use std::io::Write;
        let mut w = log_writer().make_writer_for_test();
        w.write_all(b"first line\nsecond ").unwrap();
        w.write_all(b"line\n").unwrap();
        drop(w);
        let logs = recent_logs();
        assert!(logs.iter().any(|l| l == "first line"));
        assert!(logs.iter().any(|l| l == "second line"));
    }

    impl LogRing {
        fn make_writer_for_test(&self) -> LogRingWriter {
            tracing_subscriber::fmt::MakeWriter::make_writer(self)
        }
    }
}
