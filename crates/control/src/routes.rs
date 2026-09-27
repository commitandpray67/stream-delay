//! HTTP and WebSocket routes for delay control.

use std::sync::PoisonError;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use streamdelay_config::SecretError;
use streamdelay_relay::{Ack, DelayMode, GoLiveWhen, RelayError, RelayState};

use crate::auth::{self, Scope};
use crate::overlays::{Counts, Page};
use crate::{AppState, diagnostics, obs_routes, settings, ui};

/// Error body: `{"error": "..."}`.
#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub(crate) fn bad_request(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<SecretError> for ApiError {
    fn from(e: SecretError) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl From<RelayError> for ApiError {
    fn from(e: RelayError) -> Self {
        let status = match e {
            RelayError::Engine(_) | RelayError::Url(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        ApiError(status, e.to_string())
    }
}

pub(crate) fn router(state: AppState) -> Router {
    // Every link may read the state (overlay links can do no more).
    let read = Router::new()
        .route("/api/v1/state", get(get_state))
        .route("/api/v1/events", get(events))
        .route_layer(middleware::from_fn(|r: Request, n: Next| {
            auth::require(Scope::Read, r, n)
        }));
    // Dock links can also change the delay.
    let control = Router::new()
        .route("/api/v1/delay", put(set_delay))
        .route("/api/v1/live", post(go_live))
        .route("/api/v1/cancel", post(cancel))
        .route("/api/v1/stream/end", post(end_stream))
        .route("/api/v1/stream/dump", post(dump))
        .route("/api/v1/stream/resume", post(resume))
        .route("/api/v1/presets/{index}", post(preset))
        .route_layer(middleware::from_fn(|r: Request, n: Next| {
            auth::require(Scope::Control, r, n)
        }));
    // Settings, stream keys, diagnostics and OBS need the dashboard link.
    let admin = settings::routes()
        .merge(diagnostics::routes())
        .merge(obs_routes::routes())
        .route("/api/v1/updates/check", post(check_updates))
        .route_layer(middleware::from_fn(|r: Request, n: Next| {
            auth::require(Scope::Admin, r, n)
        }));
    Router::new()
        // Names the app, so a second copy can tell that the port is taken by
        // stream-delay rather than by another program.
        .route(
            "/healthz",
            get(|| async {
                Json(json!({
                    "status": "ok",
                    "app": "stream-delay",
                    "version": env!("CARGO_PKG_VERSION"),
                }))
            }),
        )
        .merge(read)
        .merge(control)
        .merge(admin)
        .merge(diagnostics::download_routes())
        .merge(ui::routes())
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        // Outermost, so refusals from the guard carry the headers too.
        .layer(middleware::map_response(ui::security_headers))
        .with_state(state)
}

async fn get_state(
    State(st): State<AppState>,
    Extension(scope): Extension<Scope>,
) -> impl IntoResponse {
    Json(visible_state(st.relay().state(), scope))
}

/// The state as a link with `scope` may see it: the encoder's network address,
/// where the stream goes, and what the destination replied (keys are removed
/// from it, but it is the destination's text) are for the dashboard only.
fn visible_state(mut state: RelayState, scope: Scope) -> RelayState {
    if scope < Scope::Admin {
        state.ingest.peer = None;
        state.ingest.bad_keys_recent = 0;
        state.ingest.bad_key_from = None;
        state.ingest.key_warning = None;
        state.egress.destination = None;
        state.egress.last_error = None;
    }
    state
}

#[derive(Debug, Deserialize)]
struct DelayBody {
    /// Delay in seconds (fractions allowed).
    seconds: f64,
    /// Defaults to the configured default mode.
    mode: Option<DelayMode>,
}

async fn set_delay(
    State(st): State<AppState>,
    Json(body): Json<DelayBody>,
) -> Result<Json<Ack>, ApiError> {
    if !body.seconds.is_finite() || body.seconds < 0.0 {
        return Err(ApiError::bad_request("seconds must be a positive number"));
    }
    let mode = body.mode.unwrap_or(st.config().delay.default_mode);
    let ms = (body.seconds * 1000.0).round() as u64;
    Ok(Json(st.relay().set_delay(ms, mode).await?))
}

#[derive(Debug, Deserialize)]
struct LiveBody {
    #[serde(default = "default_when")]
    when: GoLiveWhen,
}

fn default_when() -> GoLiveWhen {
    GoLiveWhen::Now
}

/// `{"when": ...}` from a request body; no body means now. A body is read
/// whatever Content-Type it is sent with: asking for the buffer to air first must
/// never act at once instead.
fn when(body: &Bytes) -> Result<GoLiveWhen, ApiError> {
    if body.trim_ascii().is_empty() {
        return Ok(GoLiveWhen::Now);
    }
    Ok(serde_json::from_slice::<LiveBody>(body)
        .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?
        .when)
}

async fn go_live(State(st): State<AppState>, body: Bytes) -> Result<Json<Ack>, ApiError> {
    Ok(Json(st.relay().go_live(when(&body)?).await?))
}

/// Ends the broadcast: now, without airing what is buffered, or with
/// `{"when": "after-air"}` once what stream-delay has received so far has aired.
async fn end_stream(
    State(st): State<AppState>,
    Extension(scope): Extension<Scope>,
    body: Bytes,
) -> Result<Json<RelayState>, ApiError> {
    match when(&body)? {
        GoLiveWhen::Now => st.relay().end_stream().await?,
        GoLiveWhen::AfterAir => st.relay().end_stream_after_air().await?,
    }
    Ok(Json(visible_state(st.relay().state(), scope)))
}

#[derive(Debug, Default, Deserialize)]
struct DumpBody {
    /// Defaults to the configured default mode.
    mode: Option<DelayMode>,
    /// The slate covers the stream even with no overlay page connected (the
    /// caller shows it some other way).
    #[serde(default)]
    allow_uncovered: bool,
}

/// Throws away what has not aired yet and keeps broadcasting with the same delay.
/// The answer's `dump` says what viewers see: the last stretch again
/// (`replay`, Rewind only), the slate while the delay builds back up (`cover`,
/// only with an overlay connected), or the last frame, still (`hold`).
async fn dump(State(st): State<AppState>, body: Bytes) -> Result<Json<Ack>, ApiError> {
    let body = if body.trim_ascii().is_empty() {
        DumpBody::default()
    } else {
        serde_json::from_slice::<DumpBody>(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?
    };
    Ok(Json(st.dump(body.mode, body.allow_uncovered).await?))
}

/// Where releases are published.
const RELEASES: &str = "https://github.com/commitandpray67/stream-delay/releases/latest";

/// Runs the desktop app's updater, which asks before installing anything. Other
/// builds answer with the releases page instead.
async fn check_updates(State(st): State<AppState>) -> Json<serde_json::Value> {
    let check = st
        .shared
        .update_check
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match check {
        Some(check) => {
            check();
            Json(json!({ "checking": true }))
        }
        None => Json(json!({ "checking": false, "releases": RELEASES })),
    }
}

async fn resume(
    State(st): State<AppState>,
    Extension(scope): Extension<Scope>,
) -> Result<Json<RelayState>, ApiError> {
    st.relay().resume().await?;
    Ok(Json(visible_state(st.relay().state(), scope)))
}

async fn cancel(State(st): State<AppState>) -> Result<Json<Ack>, ApiError> {
    Ok(Json(
        st.relay()
            .command(streamdelay_relay::Command::Cancel)
            .await?,
    ))
}

async fn preset(
    State(st): State<AppState>,
    Path(index): Path<usize>,
) -> Result<Json<Ack>, ApiError> {
    let presets = st.config().delay.presets;
    let Some(p) = presets.get(index) else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no preset {index}"),
        ));
    };
    let ack = if p.seconds <= 0.0 {
        st.relay().go_live(GoLiveWhen::Now).await?
    } else {
        st.relay()
            .set_delay((p.seconds * 1000.0).round() as u64, p.mode)
            .await?
    };
    Ok(Json(ack))
}

#[derive(Debug, Default, Deserialize)]
struct EventsQuery {
    /// `overlay` for the overlay page in OBS, which is then counted.
    role: Option<String>,
}

/// Pushes `{"type":"state","state":{...}}` whenever the state changes,
/// `{"type":"config","config":{...}}` on connect and whenever settings change, and
/// `{"type":"overlays","count":n,"active":m}` on connect and whenever an overlay
/// page connects, leaves, or goes on or off stream. The config is the full
/// settings for dashboard links, and only what the dock and overlay display for
/// other links.
///
/// The overlay page in OBS (`role=overlay`) says what OBS tells it (see
/// [`FromOverlay`]); other connections only send pings and close frames.
async fn events(
    State(st): State<AppState>,
    Extension(scope): Extension<Scope>,
    Query(query): Query<EventsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let overlay = query.role.as_deref() == Some("overlay");
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| stream_events(st, scope, overlay, socket))
}

/// What the overlay page in OBS says. Anything else, or more than
/// [`OVERLAY_MESSAGES_PER_SECOND`], is ignored.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum FromOverlay {
    /// OBS put the page on stream (`true`) or took it off.
    Overlay { active: bool },
    /// The page painted the slate for this change.
    SlateShown { change: u64 },
}

const OVERLAY_MESSAGES_PER_SECOND: u32 = 10;
/// Longer than any message an overlay page sends.
const MAX_OVERLAY_MESSAGE: usize = 256;

/// Lets [`OVERLAY_MESSAGES_PER_SECOND`] through each second.
struct RateLimit {
    since: tokio::time::Instant,
    taken: u32,
}

impl RateLimit {
    fn new() -> Self {
        Self {
            since: tokio::time::Instant::now(),
            taken: 0,
        }
    }

    fn allow(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        if now.duration_since(self.since) >= Duration::from_secs(1) {
            self.since = now;
            self.taken = 0;
        }
        self.taken += 1;
        self.taken <= OVERLAY_MESSAGES_PER_SECOND
    }
}

async fn from_overlay(st: &AppState, page: &Page, text: &str) {
    if text.len() > MAX_OVERLAY_MESSAGE {
        return;
    }
    match serde_json::from_str::<FromOverlay>(text) {
        Ok(FromOverlay::Overlay { active }) => page.set_active(active),
        Ok(FromOverlay::SlateShown { change }) => {
            let _ = st.relay().slate_shown(change).await;
        }
        Err(_) => {}
    }
}

/// Largest WebSocket message accepted from a client.
const MAX_WS_MESSAGE: usize = 64 * 1024;

async fn stream_events(st: AppState, scope: Scope, overlay: bool, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let page = overlay.then(|| st.shared.overlays.join());
    let mut limit = RateLimit::new();
    let mut state = st.relay().subscribe();
    let mut config = st.shared.config_tx.subscribe();
    let mut overlays = st.shared.overlays.subscribe();
    let overlays_msg = |c: Counts| {
        let msg = json!({ "type": "overlays", "count": c.count, "active": c.active });
        Message::Text(msg.to_string().into())
    };
    let state_msg = |s: &RelayState| {
        let s = visible_state(s.clone(), scope);
        Message::Text(json!({ "type": "state", "state": s }).to_string().into())
    };
    let config_msg = |st: &AppState| {
        let c = match scope {
            Scope::Admin => json!(settings::public_config(st)),
            _ => json!(settings::limited_config(st, scope)),
        };
        Message::Text(json!({ "type": "config", "config": c }).to_string().into())
    };
    config.mark_unchanged();
    if tx.send(config_msg(&st)).await.is_err() {
        return;
    }
    let initial = state_msg(&state.borrow_and_update());
    if tx.send(initial).await.is_err() {
        return;
    }
    let count = *overlays.borrow_and_update();
    if tx.send(overlays_msg(count)).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    loop {
        let msg = tokio::select! {
            changed = state.changed() => {
                if changed.is_err() {
                    return;
                }
                state_msg(&state.borrow_and_update())
            }
            changed = config.changed() => {
                if changed.is_err() {
                    return;
                }
                config.mark_unchanged();
                config_msg(&st)
            }
            changed = overlays.changed() => {
                if changed.is_err() {
                    return;
                }
                let count = *overlays.borrow_and_update();
                overlays_msg(count)
            }
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(Message::Text(text))) => {
                    if let Some(page) = &page
                        && limit.allow()
                    {
                        from_overlay(&st, page, &text).await;
                    }
                    continue;
                }
                _ => continue,
            },
            _ = ping.tick() => Message::Ping(Vec::new().into()),
        };
        if tx.send(msg).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_overlay_page_says_at_most_ten_things_a_second() {
        let mut limit = RateLimit::new();
        assert!((0..10).all(|_| limit.allow()));
        assert!(!limit.allow());
        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(!limit.allow());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!((0..10).all(|_| limit.allow()));
        assert!(!limit.allow());
    }

    #[test]
    fn only_the_dashboard_sees_the_encoder_address_and_destination_replies() {
        let mut state = RelayState::default();
        state.ingest.peer = Some("192.168.1.20:50123".into());
        state.ingest.bad_keys_recent = 12;
        state.ingest.bad_key_from = Some("203.0.113.9".into());
        state.ingest.key_warning = Some("weak".into());
        state.egress.last_error = Some("destination refused the stream".into());
        state.egress.destination = Some("rtmps://restream.example.net:4443/live".into());
        let admin = visible_state(state.clone(), Scope::Admin);
        assert!(admin.ingest.peer.is_some());
        assert!(admin.egress.last_error.is_some());
        assert!(admin.egress.destination.is_some());
        assert_eq!(admin.ingest.bad_keys_recent, 12);
        assert!(admin.ingest.bad_key_from.is_some() && admin.ingest.key_warning.is_some());
        for scope in [Scope::Control, Scope::Read] {
            let s = visible_state(state.clone(), scope);
            assert_eq!(s.ingest.peer, None);
            assert_eq!(s.egress.last_error, None);
            // The settings they get leave the destination out; so does the state.
            assert_eq!(s.egress.destination, None);
            assert_eq!(s.ingest.bad_keys_recent, 0);
            assert_eq!((s.ingest.bad_key_from, s.ingest.key_warning), (None, None));
        }
    }
}
