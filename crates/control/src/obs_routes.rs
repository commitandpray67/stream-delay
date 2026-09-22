//! OBS integration endpoints (filled in by the OBS setup wizard milestone).

use axum::Router;

use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
}
