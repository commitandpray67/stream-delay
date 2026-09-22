//! Configuration for stream-delay.
//!
//! Settings live in `config.toml` in the platform config directory. Secrets (stream
//! key, OBS password, the OBS settings backup's key) never go into that file: they
//! are stored in the OS keychain, or in a private `secrets.toml` when no keychain is
//! available (headless Linux, containers).

mod secrets;

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use secrets::{SecretStore, Secrets};
pub use streamdelay_engine::DelayMode;

/// Names of stored secrets.
pub mod secret {
    pub const DESTINATION_KEY: &str = "destination-key";
    pub const OBS_PASSWORD: &str = "obs-password";
    pub const OBS_BACKUP_KEY: &str = "obs-backup-key";
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("could not write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("{path} is not valid: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("no configuration directory could be determined for this user")]
    NoConfigDir,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ingest: IngestConfig,
    pub destination: DestinationConfig,
    pub delay: DelayConfig,
    pub api: ApiConfig,
    pub overlay: OverlayConfig,
    pub obs: ObsConfig,
    pub hotkeys: HotkeyConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestConfig {
    /// Where OBS connects (`rtmp://<bind>/live`).
    pub bind: SocketAddr,
    /// Seconds to keep the destination connected after OBS disconnects.
    pub grace_seconds: u64,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 1935)),
            grace_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeyMode {
    /// Use the key stored in the keychain.
    #[default]
    Stored,
    /// Forward the key OBS publishes with.
    Passthrough,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DestinationConfig {
    /// Destination service id from [`SERVICES`], or "custom".
    pub service: String,
    pub url: String,
    pub key_mode: KeyMode,
}

impl Default for DestinationConfig {
    fn default() -> Self {
        Self {
            service: "twitch".into(),
            url: SERVICES[0].url.into(),
            key_mode: KeyMode::Stored,
        }
    }
}

/// Well-known ingest endpoints offered in the UI.
pub struct Service {
    pub id: &'static str,
    pub name: &'static str,
    pub url: &'static str,
}

pub const SERVICES: &[Service] = &[
    Service {
        id: "twitch",
        name: "Twitch",
        url: "rtmp://live.twitch.tv/app",
    },
    Service {
        id: "twitch-rtmps",
        name: "Twitch (RTMPS)",
        url: "rtmps://live.twitch.tv:443/app",
    },
    Service {
        id: "youtube",
        name: "YouTube",
        url: "rtmp://a.rtmp.youtube.com/live2",
    },
    Service {
        id: "youtube-rtmps",
        name: "YouTube (RTMPS)",
        url: "rtmps://a.rtmps.youtube.com:443/live2",
    },
    Service {
        id: "custom",
        name: "Custom RTMP/RTMPS server",
        url: "",
    },
];

/// A one-click delay setting. `seconds <= 0` means "go live now".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub seconds: f64,
    #[serde(default)]
    pub mode: DelayMode,
}

impl Preset {
    pub fn defaults() -> Vec<Preset> {
        [0.0, 15.0, 30.0, 60.0, 120.0]
            .into_iter()
            .map(|seconds| Preset {
                seconds,
                mode: DelayMode::Rewind,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DelayConfig {
    /// Largest delay that can be set; the buffer holds this much (plus a little).
    pub max_seconds: u64,
    /// Delay applied when the stream starts.
    pub start_seconds: f64,
    pub default_mode: DelayMode,
    pub presets: Vec<Preset>,
    /// Upper bound on buffered data, in MiB.
    pub ram_cap_mb: u64,
}

impl Default for DelayConfig {
    fn default() -> Self {
        Self {
            max_seconds: 120,
            start_seconds: 0.0,
            default_mode: DelayMode::Rewind,
            presets: Preset::defaults(),
            ram_cap_mb: 512,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub bind: SocketAddr,
    /// Generated on first run. Embedded in dock and overlay URLs.
    pub token: String,
    /// Allow other devices on the network to use the API (still token-protected).
    pub allow_lan: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 7788)),
            token: String::new(),
            allow_lan: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    /// Show a small "delay" badge while delayed.
    pub badge: bool,
    /// top-left, top-right, bottom-left or bottom-right.
    pub badge_position: String,
    /// Show a short popup when the delay changes.
    pub popup: bool,
    pub mask_title: String,
    pub mask_subtitle: String,
    pub accent_color: String,
    pub background_color: String,
    pub text_color: String,
    /// Optional image URL shown on the mask slate.
    pub mask_image: String,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            badge: true,
            badge_position: "top-right".into(),
            popup: true,
            mask_title: "Stream delay is being added".into(),
            mask_subtitle: "We'll be right back".into(),
            accent_color: "#9147ff".into(),
            background_color: "#0e0e10".into(),
            text_color: "#efeff1".into(),
            mask_image: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObsConfig {
    pub host: String,
    pub port: u16,
    /// OBS stream settings before stream-delay changed them, for "Restore".
    pub backup: Option<ObsBackup>,
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 4455,
            backup: None,
        }
    }
}

/// OBS stream service settings, minus the stream key (stored as a secret).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObsBackup {
    pub service_type: String,
    /// JSON object of settings with the `key` field removed.
    pub settings_json: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub enabled: bool,
    pub go_live: String,
    pub go_live_after_air: String,
    /// One shortcut per delay preset, in order. Empty strings are unassigned.
    pub presets: Vec<String>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            go_live: "CmdOrCtrl+Alt+Shift+L".into(),
            go_live_after_air: "CmdOrCtrl+Alt+Shift+A".into(),
            presets: (1..=5)
                .map(|i| format!("CmdOrCtrl+Alt+Shift+{i}"))
                .collect(),
        }
    }
}

impl Config {
    /// Default location: `<config dir>/stream-delay/config.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        directories::ProjectDirs::from("dev", "stream-delay", "stream-delay")
            .map(|d| d.config_dir().join("config.toml"))
            .ok_or(ConfigError::NoConfigDir)
    }

    /// Loads the config, creating it (with a fresh API token) if missing.
    pub fn load_or_create(path: &Path) -> Result<Config, ConfigError> {
        let mut config = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.into(),
                source,
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.into(),
                    source,
                });
            }
        };
        if config.api.token.is_empty() || !path.exists() {
            if config.api.token.is_empty() {
                config.api.token = new_token();
            }
            config.save(path)?;
        }
        Ok(config)
    }

    /// Writes the config atomically (write to a temp file, then rename).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let err = |source| ConfigError::Write {
            path: path.into(),
            source,
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(err)?;
        }
        let text = toml::to_string_pretty(self).expect("config always serializes");
        let tmp = path.with_extension("toml.tmp");
        write_private(&tmp, text.as_bytes()).map_err(err)?;
        fs::rename(&tmp, path).map_err(err)
    }
}

/// Generates a random 128-bit API token.
pub fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes a file readable only by the current user (on Unix).
pub(crate) fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        fs::write(path, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_with_token_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        let c = Config::load_or_create(&path).unwrap();
        assert_eq!(c.api.token.len(), 32);
        assert!(path.exists());
        let again = Config::load_or_create(&path).unwrap();
        assert_eq!(c, again);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn partial_files_use_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[delay]\nmax_seconds = 300\n\n[api]\ntoken = \"abc\"\n",
        )
        .unwrap();
        let c = Config::load_or_create(&path).unwrap();
        assert_eq!(c.delay.max_seconds, 300);
        assert_eq!(c.api.token, "abc");
        assert_eq!(c.ingest, IngestConfig::default());
        assert_eq!(c.delay.presets.len(), 5);
    }

    #[test]
    fn invalid_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "delay = [").unwrap();
        assert!(matches!(
            Config::load_or_create(&path),
            Err(ConfigError::Parse { .. })
        ));
    }
}
