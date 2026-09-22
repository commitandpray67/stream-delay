//! HTTP and WebSocket routes.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router, middleware};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use streamdelay_relay::{Ack, DelayMode, GoLiveWhen, RelayError};

use crate::{AppState, auth};

/// Error body: `{"error": "..."}`.
#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

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
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .with_state(state)
}

async fn get_state(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.relay.state())
}

#[derive(Debug, Deserialize)]
struct DelayBody {
    /// Delay in seconds (fractions allowed).
    seconds: f64,
    #[serde(default)]
    mode: DelayMode,
}

#[derive(Debug, Serialize)]
struct AckBody {
    #[serde(flatten)]
    ack: Ack,
}

async fn set_delay(
    State(st): State<AppState>,
    Json(body): Json<DelayBody>,
) -> Result<Json<AckBody>, ApiError> {
    if !body.seconds.is_finite() || body.seconds < 0.0 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "seconds must be a positive number".into(),
        ));
    }
    let ms = (body.seconds * 1000.0).round() as u64;
    let ack = st.relay.set_delay(ms, body.mode).await?;
    Ok(Json(AckBody { ack }))
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
) -> Result<Json<AckBody>, ApiError> {
    let when = body.map_or(GoLiveWhen::Now, |b| b.when);
    let ack = st.relay.go_live(when).await?;
    Ok(Json(AckBody { ack }))
}

async fn cancel(State(st): State<AppState>) -> Result<Json<AckBody>, ApiError> {
    let ack = st.relay.command(streamdelay_relay::Command::Cancel).await?;
    Ok(Json(AckBody { ack }))
}

async fn preset(
    State(st): State<AppState>,
    Path(index): Path<usize>,
) -> Result<Json<AckBody>, ApiError> {
    let presets = st.presets();
    let Some(p) = presets.get(index) else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no preset {index}"),
        ));
    };
    let ack = if p.seconds <= 0.0 {
        st.relay.go_live(GoLiveWhen::Now).await?
    } else {
        st.relay
            .set_delay((p.seconds * 1000.0).round() as u64, p.mode)
            .await?
    };
    Ok(Json(AckBody { ack }))
}

/// Streams state updates as JSON messages: `{"type":"state","state":{...}}`.
async fn events(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| stream_state(st, socket))
}

async fn stream_state(st: AppState, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut state = st.relay.subscribe();
    let send = |s: &streamdelay_relay::RelayState| {
        Message::Text(json!({ "type": "state", "state": s }).to_string().into())
    };
    let initial = send(&state.borrow_and_update());
    if tx.send(initial).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    loop {
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err() {
                    return;
                }
                let msg = send(&state.borrow_and_update());
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
            _ = ping.tick() => {
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return;
                }
            }
        }
    }
}
