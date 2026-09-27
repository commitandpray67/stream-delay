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

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

pub fn get(base: &str, token: &str, path: &str) -> Result<Value> {
    finish(
        agent()
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .call(),
    )
}

pub fn put(base: &str, token: &str, path: &str, body: Value) -> Result<Value> {
    finish(
        agent()
            .put(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send_json(body),
    )
}

pub fn post(base: &str, token: &str, path: &str, body: Value) -> Result<Value> {
    finish(
        agent()
            .post(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send_json(body),
    )
}

/// The version of the stream-delay answering at `base`. Needs no token. Gives
/// up after a few seconds, so a health check never hangs on a stuck instance.
pub fn health(base: &str) -> Result<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
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
