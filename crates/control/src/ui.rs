//! Serves the web UI (dashboard, OBS dock, overlay) embedded at build time from
//! `ui/dist`. Build it with `pnpm -C ui build` before `cargo build`.

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use rust_embed::RustEmbed;

use crate::AppState;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Assets;

const MISSING: &str = r#"<!doctype html><meta charset="utf-8"><title>stream-delay</title>
<body style="font-family:system-ui;background:#0e0e10;color:#efeff1;padding:2rem">
<h1>stream-delay is running</h1>
<p>The web interface was not included in this build. Run <code>pnpm -C ui install</code>, then
<code>pnpm -C ui build</code>, then build stream-delay again; or use the HTTP API directly.</p></body>"#;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/dock", get(index))
        .route("/overlay", get(index))
        .route("/setup", get(index))
        .route("/assets/{*path}", get(asset))
        .route("/favicon.svg", get(|| asset_named("favicon.svg")))
}

/// Headers for every response: only this server's own pages may frame ours (the
/// dashboard previews the overlay; OBS loads docks and sources directly), no MIME
/// sniffing, and no Referer, since page URLs can carry the access token.
pub(crate) async fn security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'self'"),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    r
}

async fn index() -> Response {
    match Assets::get("index.html") {
        Some(f) => {
            let mut r = Html(f.data.into_owned()).into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            r
        }
        None => Html(MISSING).into_response(),
    }
}

async fn asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    asset_named(&format!("assets/{path}")).await
}

async fn asset_named(path: &str) -> Response {
    match Assets::get(path) {
        Some(f) => {
            let mime = f.metadata.mimetype().to_string();
            let mut r = (StatusCode::OK, f.data.into_owned()).into_response();
            let h = r.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&mime) {
                h.insert(header::CONTENT_TYPE, v);
            }
            // Vite fingerprints asset names, so they can be cached forever.
            h.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            r
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
