//! HTTP and WebSocket routes for delay control.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router, middleware};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use streamdelay_relay::{Ack, DelayMode, GoLiveWhen, RelayError};

use crate::{AppState, auth, obs_routes, settings, ui};

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
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/v1/state", get(get_state))
        .route("/api/v1/delay", put(set_delay))
        .route("/api/v1/live", post(go_live))
        .route("/api/v1/cancel", post(cancel))
        .route("/api/v1/presets/{index}", post(preset))
        .route("/api/v1/events", get(events))
        .merge(settings::routes())
        .merge(obs_routes::routes())
        .merge(ui::routes())
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .with_state(state)
}

async fn get_state(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.relay().state())
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

async fn go_live(
    State(st): State<AppState>,
    body: Option<Json<LiveBody>>,
) -> Result<Json<Ack>, ApiError> {
    let when = body.map_or(GoLiveWhen::Now, |b| b.when);
    Ok(Json(st.relay().go_live(when).await?))
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

/// Pushes `{"type":"state","state":{...}}` whenever the state changes and
/// `{"type":"config","config":{...}}` on connect and whenever settings change.
async fn events(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| stream_events(st, socket))
}

async fn stream_events(st: AppState, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut state = st.relay().subscribe();
    let mut config = st.shared.config_tx.subscribe();
    let state_msg = |s: &streamdelay_relay::RelayState| {
        Message::Text(json!({ "type": "state", "state": s }).to_string().into())
    };
    let config_msg = |st: &AppState| {
        let c = settings::public_config(st);
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
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => continue,
            },
            _ = ping.tick() => Message::Ping(Vec::new().into()),
        };
        if tx.send(msg).await.is_err() {
            return;
        }
    }
}
