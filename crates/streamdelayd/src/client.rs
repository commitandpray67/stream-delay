//! Minimal client for the local control API.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

fn finish(resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value> {
    let mut resp = resp?;
    let status = resp.status();
    let body: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("request failed");
        bail!("{status}: {msg}");
    }
    Ok(body)
}

/// An agent for requests to `base`, giving up after `timeout` if given. Like
/// browsers, it goes to this computer directly: a proxy set for its other
/// traffic cannot reach its loopback, and would see the token.
fn agent(base: &str, timeout: Option<Duration>) -> ureq::Agent {
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(timeout);
    if is_this_computer(base) {
        config = config.proxy(None);
    }
    config.build().into()
}

/// True when `base` names this computer: `localhost` or a loopback address.
fn is_this_computer(base: &str) -> bool {
    let Some(host) = base.parse::<ureq::http::Uri>().ok().and_then(|u| {
        u.host()
            .map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string())
    }) else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// How long a command waits for the running instance: far longer than any
/// request takes (a dump waits at most 2 s for the destination), but a script
/// or a Stream Deck button must not hang on an instance that is stuck.
const REQUEST_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(30)
};

pub fn get(base: &str, token: &str, path: &str) -> Result<Value> {
    finish(
        agent(base, Some(REQUEST_TIMEOUT))
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .call(),
    )
}

pub fn put(base: &str, token: &str, path: &str, body: Value) -> Result<Value> {
    finish(
        agent(base, Some(REQUEST_TIMEOUT))
            .put(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send_json(body),
    )
}

pub fn post(base: &str, token: &str, path: &str, body: Value) -> Result<Value> {
    finish(
        agent(base, Some(REQUEST_TIMEOUT))
            .post(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send_json(body),
    )
}

/// The version of the stream-delay answering at `base`. Needs no token. Gives
/// up after a few seconds, so a health check never hangs on a stuck instance.
pub fn health(base: &str) -> Result<String> {
    let agent = agent(base, Some(Duration::from_secs(5)));
    let body = finish(agent.get(format!("{base}/healthz")).call())?;
    if body["app"] != "stream-delay" || body["status"] != "ok" {
        bail!("{base} answers, but not as stream-delay");
    }
    Ok(body["version"].as_str().unwrap_or("unknown").to_string())
}

pub fn print(v: Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{get, is_this_computer};

    #[test]
    fn a_command_gives_up_on_an_instance_that_never_answers() {
        // Takes the connection (the OS does, for the listener) but never answers.
        let stuck = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", stuck.local_addr().unwrap());
        let started = Instant::now();
        assert!(get(&base, "token", "/api/v1/state").is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn this_computer_is_localhost_or_a_loopback_address() {
        for base in [
            "http://127.0.0.1:7788",
            "http://127.0.0.2:7788",
            "http://[::1]:7788",
            "http://[::ffff:127.0.0.1]:7788",
            "http://LOCALHOST:7788",
        ] {
            assert!(is_this_computer(base), "{base}");
        }
        for base in [
            "http://192.168.1.5:7788",
            "http://nas:7788",
            "https://stream.example.com",
            "http://localhost.example.com",
            "not a URL",
        ] {
            assert!(!is_this_computer(base), "{base}");
        }
    }
}
