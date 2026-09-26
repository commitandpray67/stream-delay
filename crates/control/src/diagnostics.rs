//! Diagnostics bundle for bug reports: version, platform, settings, live state and
//! recent log lines, with every secret removed.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use streamdelay_config::secret;
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::auth::Scope;
use crate::routes::ApiError;
use crate::settings::public_config;

/// How long a download link from `POST /api/v1/diagnostics/link` works.
const CODE_TTL: Duration = Duration::from_secs(60);
/// Codes outstanding at once; older ones stop working.
const MAX_CODES: usize = 8;

/// Recent log lines kept in memory for the bundle.
const LOG_LINES: usize = 2000;
/// Longest line kept, in bytes: lines can quote what others sent.
const MAX_LINE: usize = 2048;

fn ring() -> &'static Mutex<VecDeque<String>> {
    static RING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(LOG_LINES)))
}

fn push_line(line: &str) {
    let mut line = line.to_string();
    if line.len() > MAX_LINE {
        let mut end = MAX_LINE;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push('…');
    }
    if let Ok(mut r) = ring().lock() {
        if r.len() == LOG_LINES {
            r.pop_front();
        }
        r.push_back(line);
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

/// Unused download codes and when they expire.
#[derive(Default)]
pub(crate) struct Codes(Mutex<Vec<(String, Instant)>>);

impl Codes {
    fn issue(&self) -> String {
        let code = streamdelay_config::new_token();
        let mut codes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        codes.retain(|(_, expires)| *expires > now);
        if codes.len() >= MAX_CODES {
            codes.remove(0);
        }
        codes.push((code.clone(), now + CODE_TTL));
        code
    }

    /// Uses up `code`; true if it was valid.
    fn redeem(&self, code: &str) -> bool {
        let mut codes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        codes.retain(|(_, expires)| *expires > now);
        let found = codes
            .iter()
            .position(|(c, _)| bool::from(c.as_bytes().ct_eq(code.as_bytes())));
        found.map(|i| codes.remove(i)).is_some()
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/diagnostics", get(diagnostics))
        .route("/api/v1/diagnostics/link", post(link))
}

/// Served without a token: the single-use code in the path stands in for it, so
/// the dashboard can offer a plain download link without putting its token in a
/// URL (downloads remember where they came from).
pub(crate) fn download_routes() -> Router<AppState> {
    Router::new().route("/diagnostics/{code}", get(download))
}

async fn link(State(st): State<AppState>) -> Json<Value> {
    let code = st.shared.download_codes.issue();
    Json(json!({ "url": format!("/diagnostics/{code}") }))
}

async fn download(
    State(st): State<AppState>,
    Path(code): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if !st.shared.download_codes.redeem(&code) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "this download link has expired; download again from the dashboard".into(),
        ));
    }
    Ok(diagnostics(State(st)).await)
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
    mask_public_ips(&out)
}

/// Replaces public IP addresses in `text` (the streamer's home address on a
/// server, anyone's who connected) with `<public IP xxxx>`: a tag that is the
/// same for the same address within this run, so connections can still be told
/// apart, but does not give the address away. Local and private ones stay.
fn mask_public_ips(text: &str) -> String {
    let ip_char = |c: char| c.is_ascii_hexdigit() || c == ':' || c == '.';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(ip_char) {
        out.push_str(&rest[..start]);
        let run = &rest[start..];
        let end = run.find(|c| !ip_char(c)).unwrap_or(run.len());
        let (candidate, tail) = run[..end].split_at(ip_end(&run[..end]));
        match candidate.parse::<IpAddr>() {
            Ok(ip) if is_public(ip) => out.push_str(&ip_tag(ip)),
            _ => out.push_str(candidate),
        }
        out.push_str(tail);
        rest = &run[end..];
    }
    out.push_str(rest);
    out
}

/// Where the address in `run` ends: before a trailing `:port` of an IPv4 address,
/// or a full stop that ends a sentence.
fn ip_end(run: &str) -> usize {
    let run = run.trim_end_matches('.');
    match run.rsplit_once(':') {
        Some((v4, port))
            if v4.parse::<Ipv4Addr>().is_ok() && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            v4.len()
        }
        _ => run.len(),
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                // Carrier-grade NAT (100.64.0.0/10).
                || (a == 100 && (64..128).contains(&b)))
        }
        // Global unicast (2000::/3), which unique-local and link-local are not.
        IpAddr::V6(v6) => v6.segments()[0] & 0xe000 == 0x2000,
    }
}

fn ip_tag(ip: IpAddr) -> String {
    use std::hash::BuildHasher;
    // Random for each run: without it the tag could be matched to an address by
    // trying them all.
    static SALT: OnceLock<std::collections::hash_map::RandomState> = OnceLock::new();
    let hash = SALT.get_or_init(Default::default).hash_one(ip);
    format!("<public IP {:04x}>", hash & 0xffff)
}

/// `<digits>_<something>`: what follows `live_` in a Twitch stream key.
fn is_twitch_key_tail(tail: &str) -> bool {
    let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && tail.len() > digits + 1 && tail.as_bytes()[digits] == b'_'
}

fn known_secrets(st: &AppState) -> Vec<String> {
    let s = &st.shared.secrets;
    let mut v: Vec<String> = s.get(secret::OBS_BACKUP_KEY).into_iter().collect();
    v.extend(crate::obs_routes::saved_password(s.as_ref()));
    v.extend(crate::dest_key::any(s.as_ref()));
    v.extend(crate::obs_routes::backup_secrets(s.as_ref()));
    v.extend(st.shared.key_override.clone());
    let c = st.config();
    v.extend(c.ingest.key);
    v.extend(crate::app::split_url_key(&c.destination.url).1);
    if let Ok(u) = streamdelay_relay::RtmpUrl::parse(&c.destination.url) {
        v.extend(u.query_secrets());
    }
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
    let mut bundle = raw;
    redact_value(&mut bundle, &known_secrets(st));
    bundle
}

/// Redacts every string in `v`, and every object key. Working on the values, not
/// on the JSON text, finds secrets whatever characters they contain (JSON would
/// escape quotes and backslashes in them) and can never produce something that
/// does not parse. A string that is nothing but a secret is redacted however
/// short the secret is.
fn redact_value(v: &mut Value, secrets: &[String]) {
    match v {
        Value::String(s) => {
            *s = if secrets
                .iter()
                .any(|secret| !secret.is_empty() && secret == s)
            {
                "<redacted>".into()
            } else {
                redact(s, secrets)
            };
        }
        Value::Array(items) => items.iter_mut().for_each(|x| redact_value(x, secrets)),
        Value::Object(map) => {
            for (k, mut x) in std::mem::take(map) {
                redact_value(&mut x, secrets);
                map.insert(redact(&k, secrets), x);
            }
        }
        _ => {}
    }
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
    fn public_ip_addresses_are_masked() {
        let text = "encoder connected peer=203.0.113.9:51234; from 198.51.100.7. \
                    [2a01:4f8::7]:1935 and 2a01:4f8::7 again, 8.8.8.8, ::ffff:8.8.4.4";
        let r = redact(text, &[]);
        for ip in ["51234", "1935"] {
            assert!(r.contains(ip), "port {ip} kept: {r}");
        }
        for ip in ["2a01", "8.8.8.8", "8.8.4.4"] {
            assert!(!r.contains(ip), "{ip} in {r}");
        }
        // Documentation addresses are not real ones, so they stay.
        assert!(r.contains("203.0.113.9") && r.contains("198.51.100.7"));
        // The same address gets the same tag.
        let tag = ip_tag("2a01:4f8::7".parse().unwrap());
        assert_eq!(r.matches(&tag).count(), 2, "{r}");
        assert!(r.contains(&format!("[{tag}]:1935")), "{r}");
        // Local, private and link-local ones, and things that only look similar,
        // stay.
        let kept = "127.0.0.1:1935 192.168.1.20 10.0.0.5 172.16.3.4 100.64.1.1 \
                    169.254.1.1 ::1 fe80::1 fd12:3456::1 0.0.0.0 version 0.3.1 \
                    at 12:34:56 hash deadbeef build 10.0.19045.1";
        assert_eq!(redact(kept, &[]), kept);
    }

    #[test]
    fn secrets_are_found_whatever_characters_they_contain() {
        let secrets = vec![
            r#"pa"ss\wo"rd"#.to_string(),
            "}, {".to_string(),
            "abc".to_string(),
        ];
        let mut v = json!({
            "logs": [r#"login failed with pa"ss\wo"rd for obs"#],
            "state": {"egress": {"last_error": "refused: }, {"}},
            "settings": {"obs": {"password": "abc"}},
        });
        redact_value(&mut v, &secrets);
        let text = v.to_string();
        assert!(!text.contains(r#"ss\\wo"#), "{text}");
        assert!(!text.contains("}, {"), "{text}");
        // Too short to look for inside text, but a whole value is still found.
        assert_eq!(v["settings"]["obs"]["password"], "<redacted>");
        assert_eq!(v["logs"][0], "login failed with <redacted> for obs");
        assert_eq!(v["state"]["egress"]["last_error"], "refused: <redacted>");
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

    #[test]
    fn log_lines_are_kept_short() {
        use std::io::Write;
        let mut w = log_writer().make_writer_for_test();
        let long = format!("start {}\n", "é".repeat(100_000));
        w.write_all(long.as_bytes()).unwrap();
        drop(w);
        let kept = recent_logs()
            .into_iter()
            .find(|l| l.starts_with("start "))
            .unwrap();
        assert!(kept.len() <= MAX_LINE + '…'.len_utf8(), "{}", kept.len());
        assert!(kept.ends_with('…'));
    }

    impl LogRing {
        fn make_writer_for_test(&self) -> LogRingWriter {
            tracing_subscriber::fmt::MakeWriter::make_writer(self)
        }
    }
}
