//! Request guard: Host allowlist, same-origin check and token authentication, and
//! the token scopes that decide what each link may do.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::routes::ApiError;

const TOKEN_HEADER: &str = "x-stream-delay-token";

/// What a token may do. Each scope includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Read the state and the settings the overlay shows (overlay links).
    Read,
    /// Also change the delay (dock links, Stream Deck and other controllers).
    Control,
    /// Everything, including settings, stream keys and OBS (the dashboard).
    Admin,
}

/// The token for `scope`. Dock and overlay tokens are derived from the install's
/// admin token, so their links stay the same across restarts without storing more
/// secrets, and a leaked dock or overlay link does not reveal the admin token.
pub fn scoped_token(admin: &str, scope: Scope) -> String {
    let label = match scope {
        Scope::Admin => return admin.to_string(),
        Scope::Control => "stream-delay control token v1",
        Scope::Read => "stream-delay read token v1",
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, admin.as_bytes());
    let tag = ring::hmac::sign(&key, label.as_bytes());
    tag.as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The tokens this server accepts.
pub(crate) struct Tokens {
    admin: String,
    control: String,
    read: String,
}

impl Tokens {
    pub fn new(admin: &str) -> Self {
        Self {
            admin: admin.to_string(),
            control: scoped_token(admin, Scope::Control),
            read: scoped_token(admin, Scope::Read),
        }
    }

    pub fn get(&self, scope: Scope) -> &str {
        match scope {
            Scope::Admin => &self.admin,
            Scope::Control => &self.control,
            Scope::Read => &self.read,
        }
    }

    /// The scope `token` grants, if any. With no admin token nothing is accepted.
    fn scope_of(&self, token: &str) -> Option<Scope> {
        if self.admin.is_empty() {
            return None;
        }
        let t = token.as_bytes();
        // Compare with every token so the timing does not tell which one matched.
        let matches = [Scope::Admin, Scope::Control, Scope::Read]
            .map(|s| (s, bool::from(t.ct_eq(self.get(s).as_bytes()))));
        matches.into_iter().find(|(_, m)| *m).map(|(s, _)| s)
    }
}

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

/// Guard for every route: origin checks, plus a valid token for `/api`. The token's
/// scope is added to the request for [`require`].
pub(crate) async fn guard(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    if let Err(resp) = check_origin(&state, req.headers()) {
        return resp.into_response();
    }
    if req.uri().path().starts_with("/api/") {
        let scope = token_from(&req).and_then(|t| state.shared.tokens.scope_of(&t));
        let Some(scope) = scope else {
            return (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response();
        };
        req.extensions_mut().insert(scope);
    }
    next.run(req).await
}

/// Route layer: the request's token must grant at least `needed`.
pub(crate) async fn require(needed: Scope, req: Request, next: Next) -> Response {
    match req.extensions().get::<Scope>() {
        Some(s) if *s >= needed => next.run(req).await,
        _ => ApiError(
            StatusCode::FORBIDDEN,
            "this link cannot do that; use the dashboard link".into(),
        )
        .into_response(),
    }
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

    #[test]
    fn scoped_tokens_are_distinct_and_stable() {
        let t = Tokens::new("0123456789abcdef0123456789abcdef");
        assert_eq!(t.get(Scope::Admin), "0123456789abcdef0123456789abcdef");
        assert_eq!(t.get(Scope::Control).len(), 32);
        assert_ne!(t.get(Scope::Control), t.get(Scope::Read));
        assert_ne!(t.get(Scope::Control), t.get(Scope::Admin));
        assert_eq!(
            t.get(Scope::Read),
            scoped_token("0123456789abcdef0123456789abcdef", Scope::Read)
        );
        for s in [Scope::Admin, Scope::Control, Scope::Read] {
            assert_eq!(t.scope_of(t.get(s)), Some(s));
        }
        assert_eq!(t.scope_of("nope"), None);
        // No admin token: nothing is accepted, not even the derived tokens.
        let none = Tokens::new("");
        assert_eq!(none.scope_of(none.get(Scope::Read)), None);
        assert_eq!(none.scope_of(""), None);
    }
}
