//! Request guard: Host allowlist, same-origin check and token authentication.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::AppState;

const TOKEN_HEADER: &str = "x-stream-delay-token";

fn is_loopback_host(host: &str, port: u16) -> bool {
    let p = port.to_string();
    let (name, host_port) = match host.rsplit_once(':') {
        Some((n, hp)) if !n.is_empty() && !hp.contains(']') => (n, Some(hp)),
        _ => (host, None),
    };
    if host_port.is_some_and(|hp| hp != p) {
        return false;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]"
    )
}

/// Rejects requests whose Host header does not name this server (DNS rebinding) and
/// browser requests from other origins.
pub(crate) fn check_origin(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let allow_lan = state
        .shared
        .config
        .read()
        .map(|c| c.api.allow_lan)
        .unwrap_or(false);
    if !allow_lan && !is_loopback_host(host, state.shared.port) {
        return Err((StatusCode::MISDIRECTED_REQUEST, "unexpected Host header"));
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        let same = origin.eq_ignore_ascii_case(&format!("http://{host}"));
        if !same {
            return Err((
                StatusCode::FORBIDDEN,
                "cross-origin requests are not allowed",
            ));
        }
    }
    Ok(())
}

fn token_from(req: &Request) -> Option<String> {
    let h = req.headers();
    if let Some(v) = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        && let Some(t) = v.strip_prefix("Bearer ")
    {
        return Some(t.trim().to_string());
    }
    if let Some(v) = h.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
        return Some(v.trim().to_string());
    }
    // Browser sources and docks cannot set headers on WebSocket connections, so the
    // token may also be passed as a query parameter.
    req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token=").map(str::to_string))
    })
}

pub(crate) fn token_ok(state: &AppState, req: &Request) -> bool {
    let Ok(config) = state.shared.config.read() else {
        return false;
    };
    let expected = config.api.token.as_bytes();
    !expected.is_empty() && token_from(req).is_some_and(|t| t.as_bytes().ct_eq(expected).into())
}

/// Guard for every route: origin checks, plus the token for `/api`.
pub(crate) async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Err(resp) = check_origin(&state, req.headers()) {
        return resp.into_response();
    }
    if req.uri().path().starts_with("/api/") && !token_ok(&state, &req) {
        return (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1:7788", 7788));
        assert!(is_loopback_host("localhost:7788", 7788));
        assert!(is_loopback_host("LOCALHOST:7788", 7788));
        assert!(is_loopback_host("[::1]:7788", 7788));
        assert!(!is_loopback_host("127.0.0.1:80", 7788));
        assert!(!is_loopback_host("evil.example:7788", 7788));
        assert!(!is_loopback_host("", 7788));
    }
}
