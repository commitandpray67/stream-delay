//! OBS integration through obs-websocket v5 (built into OBS 28 and newer).
//!
//! Used by the setup wizard to point OBS at stream-delay, import the existing
//! Twitch stream key, add the overlay browser source, and restore the original
//! settings later.

use std::time::Duration;

use obws::client::{ConnectConfig, DangerousConnectConfig};
use obws::requests::inputs::{Create, InputId, SetSettings};
use obws::requests::scenes::SceneId;
use serde_json::{Value, json};
use thiserror::Error;
use tracing::debug;

/// Where OBS's WebSocket server is.
#[derive(Clone)]
pub struct ObsTarget {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
}

impl std::fmt::Debug for ObsTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObsTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ObsError {
    #[error(
        "could not reach OBS at {0}. Is OBS running with its WebSocket server enabled (Tools → WebSocket Server Settings)?"
    )]
    Unreachable(String),
    #[error("OBS rejected the WebSocket password")]
    Auth,
    #[error("OBS is streaming. Stop the stream in OBS before changing its stream settings.")]
    Streaming,
    #[error("OBS reported an error: {0}")]
    Other(String),
}

impl ObsError {
    fn from_obws(e: obws::error::Error, target: &ObsTarget) -> Self {
        use obws::error::Error as E;
        match e {
            E::Connect(_) | E::Timeout | E::InvalidUri(_) => {
                ObsError::Unreachable(format!("{}:{}", target.host, target.port))
            }
            E::Handshake(h) => {
                let msg = h.to_string();
                if msg.to_lowercase().contains("auth") || msg.contains("password") {
                    ObsError::Auth
                } else {
                    ObsError::Other(msg)
                }
            }
            other => ObsError::Other(other.to_string()),
        }
    }
}

/// OBS's current stream settings, including the stream key.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSettings {
    /// `rtmp_common` (a listed service like Twitch) or `rtmp_custom`.
    pub service_type: String,
    pub settings: Value,
}

impl StreamSettings {
    /// The server OBS streams to, for display: without a login, query or stream
    /// key someone may have put in the URL (see [`shown_server`]).
    pub fn server(&self) -> Option<String> {
        let server = shown_server(self.settings.get("server").and_then(Value::as_str)?);
        let service = self.settings.get("service").and_then(Value::as_str);
        Some(match service {
            Some(s) if self.service_type == "rtmp_common" => format!("{s} ({server})"),
            _ => server,
        })
    }

    pub fn key(&self) -> Option<&str> {
        self.settings
            .get("key")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
    }

    /// The Twitch stream key, if OBS is set up for Twitch.
    pub fn twitch_key(&self) -> Option<&str> {
        let service = self
            .settings
            .get("service")
            .and_then(Value::as_str)
            .unwrap_or("");
        let server = self
            .settings
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or("");
        let listed = self.service_type == "rtmp_common" && service.eq_ignore_ascii_case("twitch");
        if listed || twitch_host(server) {
            self.key()
        } else {
            None
        }
    }

    /// True if OBS already streams to `server` (our ingest address).
    pub fn points_to(&self, server: &str) -> bool {
        self.service_type == "rtmp_custom"
            && self
                .settings
                .get("server")
                .and_then(Value::as_str)
                .is_some_and(|s| s.trim_end_matches('/') == server.trim_end_matches('/'))
    }
}

/// Splits a server URL into its scheme (with `://`), the host (and port), and
/// whether a login, more path after the application, or a query follow.
fn server_parts(server: &str) -> (&str, &str, &str, bool, bool) {
    let (scheme, rest) = match server.find("://") {
        Some(i) => server.split_at(i + 3),
        None => ("", server),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    let (login, host) = match authority.rsplit_once('@') {
        Some((_, host)) => (true, host),
        None => (false, authority),
    };
    let (path, query) = match path.find(['?', '#']) {
        Some(i) => (&path[..i], true),
        None => (path, false),
    };
    (scheme, host, path, login, query)
}

/// A server URL for display: a login (`user:password@`), a query and anything
/// after the application (where a stream key may have been pasted) are replaced
/// by `…`.
pub fn shown_server(server: &str) -> String {
    let (scheme, host, path, login, query) = server_parts(server.trim());
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    let mut shown = format!("{scheme}{}{host}", if login { "…@" } else { "" });
    if let Some(app) = segments.next() {
        shown.push('/');
        shown.push_str(app);
    }
    if segments.next().is_some() {
        shown.push_str("/…");
    }
    if query {
        shown.push_str("?…");
    }
    shown
}

/// True if `server` is one of Twitch's ingest servers, by its host.
fn twitch_host(server: &str) -> bool {
    let (_, host, ..) = server_parts(server.trim());
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ["twitch.tv", "live-video.net"]
        .iter()
        .any(|d| host == *d || host.strip_suffix(d).is_some_and(|p| p.ends_with('.')))
}

/// Snapshot of OBS for the wizard.
#[derive(Debug, Clone)]
pub struct ObsInfo {
    pub version: String,
    pub streaming: bool,
    pub stream: StreamSettings,
}

pub struct Obs {
    client: obws::Client,
    target: ObsTarget,
}

impl Obs {
    pub async fn connect(target: &ObsTarget) -> Result<Obs, ObsError> {
        debug!(?target, "connecting to obs-websocket");
        let client = obws::Client::connect_with_config(ConnectConfig {
            host: target.host.as_str(),
            port: target.port,
            // obws requires OBS 30.2+ by default, but every request used here exists
            // since obs-websocket 5.0 (OBS 28), so accept older versions too.
            dangerous: Some(DangerousConnectConfig {
                skip_studio_version_check: true,
                skip_websocket_version_check: true,
            }),
            password: target.password.as_deref(),
            event_subscriptions: None,
            broadcast_capacity: obws::client::DEFAULT_BROADCAST_CAPACITY,
            connect_timeout: Duration::from_secs(4),
        })
        .await
        .map_err(|e| ObsError::from_obws(e, target))?;
        Ok(Obs {
            client,
            target: target.clone(),
        })
    }

    fn err(&self, e: obws::error::Error) -> ObsError {
        ObsError::from_obws(e, &self.target)
    }

    pub async fn info(&self) -> Result<ObsInfo, ObsError> {
        let version = self
            .client
            .general()
            .version()
            .await
            .map_err(|e| self.err(e))?;
        let status = self
            .client
            .streaming()
            .status()
            .await
            .map_err(|e| self.err(e))?;
        Ok(ObsInfo {
            version: version.obs_studio_version.to_string(),
            streaming: status.active || status.reconnecting,
            stream: self.stream_settings().await?,
        })
    }

    pub async fn stream_settings(&self) -> Result<StreamSettings, ObsError> {
        let s = self
            .client
            .config()
            .stream_service_settings::<Value>()
            .await
            .map_err(|e| self.err(e))?;
        Ok(StreamSettings {
            service_type: s.r#type,
            settings: s.settings,
        })
    }

    async fn ensure_not_streaming(&self) -> Result<(), ObsError> {
        let status = self
            .client
            .streaming()
            .status()
            .await
            .map_err(|e| self.err(e))?;
        if status.active || status.reconnecting {
            return Err(ObsError::Streaming);
        }
        Ok(())
    }

    /// Points OBS at stream-delay (custom RTMP server).
    pub async fn stream_to(&self, server: &str, key: &str) -> Result<(), ObsError> {
        self.ensure_not_streaming().await?;
        let settings = json!({ "server": server, "key": key, "use_auth": false });
        self.client
            .config()
            .set_stream_service_settings("rtmp_custom", &settings)
            .await
            .map_err(|e| self.err(e))
    }

    /// Puts back settings read earlier with [`Obs::stream_settings`].
    pub async fn restore(&self, settings: &StreamSettings) -> Result<(), ObsError> {
        self.ensure_not_streaming().await?;
        self.client
            .config()
            .set_stream_service_settings(&settings.service_type, &settings.settings)
            .await
            .map_err(|e| self.err(e))
    }

    /// Adds a browser source to the current program scene, sized to the canvas.
    /// If an input with this name exists, points it at `url` instead.
    pub async fn add_browser_source(
        &self,
        name: &str,
        url: &str,
    ) -> Result<SourceChange, ObsError> {
        let inputs = self
            .client
            .inputs()
            .list(None)
            .await
            .map_err(|e| self.err(e))?;
        if inputs.iter().any(|i| i.id.name == name) {
            // Made earlier, perhaps for another address or token: point it here.
            let current = self
                .client
                .inputs()
                .settings::<Value>(InputId::Name(name))
                .await
                .map_err(|e| self.err(e))?;
            if current.settings.get("url").and_then(Value::as_str) == Some(url) {
                return Ok(SourceChange::Unchanged);
            }
            self.client
                .inputs()
                .set_settings(SetSettings {
                    input: InputId::Name(name),
                    settings: &json!({ "url": url }),
                    overlay: Some(true),
                })
                .await
                .map_err(|e| self.err(e))?;
            return Ok(SourceChange::Updated);
        }
        let video = self
            .client
            .config()
            .video_settings()
            .await
            .map_err(|e| self.err(e))?;
        let scene = self
            .client
            .scenes()
            .current_program_scene()
            .await
            .map_err(|e| self.err(e))?;
        let settings = json!({
            "url": url,
            "width": video.base_width,
            "height": video.base_height,
            "reroute_audio": false,
            "shutdown": false,
        });
        self.client
            .inputs()
            .create(Create {
                scene: SceneId::Name(&scene.id.name),
                input: name,
                kind: "browser_source",
                settings: Some(settings),
                enabled: Some(true),
            })
            .await
            .map_err(|e| self.err(e))?;
        Ok(SourceChange::Added)
    }
}

/// What [`Obs::add_browser_source`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceChange {
    Added,
    /// It existed with another URL.
    Updated,
    Unchanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_twitch_key_and_our_server() {
        let twitch = StreamSettings {
            service_type: "rtmp_common".into(),
            settings: json!({"service": "Twitch", "server": "auto", "key": "live_123"}),
        };
        assert_eq!(twitch.twitch_key(), Some("live_123"));
        assert_eq!(twitch.server().as_deref(), Some("Twitch (auto)"));

        let ours = StreamSettings {
            service_type: "rtmp_custom".into(),
            settings: json!({"server": "rtmp://127.0.0.1:1935/live/", "key": "streamdelay"}),
        };
        assert!(ours.points_to("rtmp://127.0.0.1:1935/live"));
        assert_eq!(ours.twitch_key(), None);

        let youtube = StreamSettings {
            service_type: "rtmp_common".into(),
            settings: json!({"service": "YouTube - RTMPS", "server": "x", "key": "yt"}),
        };
        assert_eq!(youtube.twitch_key(), None);
    }

    fn custom(server: &str) -> StreamSettings {
        StreamSettings {
            service_type: "rtmp_custom".into(),
            settings: json!({"server": server, "key": "private"}),
        }
    }

    #[test]
    fn twitch_is_told_by_the_host() {
        for server in [
            "rtmp://live.twitch.tv/app",
            "rtmps://live.twitch.tv:443/app",
            "rtmps://ingest.global-contribute.live-video.net/app",
            "rtmp://fra05.contribute.live-video.net/app/",
        ] {
            assert_eq!(custom(server).twitch_key(), Some("private"), "{server}");
        }
        for server in [
            "rtmps://ingest.example.com/twitch.tv/relay",
            "rtmp://twitch.tv.example.net/live",
            "rtmp://ingest.example.com/live?target=twitch.tv",
            "rtmp://nottwitch.tv/app",
            "rtmp://live.twitch.tv@evil.example/app",
        ] {
            assert_eq!(custom(server).twitch_key(), None, "{server}");
        }
        // A listed service only counts as such.
        let stale = StreamSettings {
            service_type: "rtmp_custom".into(),
            settings: json!({"service": "Twitch", "server": "rtmp://ingest.example.com/live", "key": "k"}),
        };
        assert_eq!(stale.twitch_key(), None);
    }

    #[test]
    fn the_server_shown_has_no_credentials() {
        for (server, shown) in [
            ("rtmp://live.twitch.tv/app", "rtmp://live.twitch.tv/app"),
            ("rtmp://127.0.0.1:1935/live/", "rtmp://127.0.0.1:1935/live"),
            (
                "rtmp://user:SECRET@host.example/live",
                "rtmp://…@host.example/live",
            ),
            (
                "rtmp://host.example/live?token=SECRET",
                "rtmp://host.example/live?…",
            ),
            (
                "rtmp://host.example/app/SECRET_live_key",
                "rtmp://host.example/app/…",
            ),
            (
                "rtmp://host.example/app#SECRET",
                "rtmp://host.example/app?…",
            ),
            ("auto", "auto"),
        ] {
            assert_eq!(custom(server).server().as_deref(), Some(shown));
        }
    }

    #[tokio::test]
    async fn unreachable_obs_gives_a_helpful_error() {
        let target = ObsTarget {
            host: "127.0.0.1".into(),
            port: 1,
            password: None,
        };
        let err = Obs::connect(&target)
            .await
            .err()
            .expect("nothing listens on port 1");
        assert!(matches!(err, ObsError::Unreachable(_)), "{err}");
        assert!(err.to_string().contains("WebSocket Server Settings"));
    }
}
