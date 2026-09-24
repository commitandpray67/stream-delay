//! Configuration for stream-delay.
//!
//! Settings live in `config.toml` in the platform config directory. Secrets (stream
//! key, OBS password, the backup of OBS's stream settings) never go into that file:
//! they are stored in the OS keychain, or in a private `secrets.toml` when no
//! keychain is available (headless Linux, containers).

mod secrets;

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use secrets::{Keychain, MemorySecrets, SecretStore, Secrets};
pub use streamdelay_engine::DelayMode;

/// Names of stored secrets.
pub mod secret {
    pub const DESTINATION_KEY: &str = "destination-key";
    pub const OBS_PASSWORD: &str = "obs-password";
    /// OBS's stream service settings (JSON) from before stream-delay changed them.
    pub const OBS_BACKUP: &str = "obs-backup";
    /// The stream key of a backup made by an older version, which kept the other
    /// settings in the config file.
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
    /// If set, encoders must use this stream key. Recommended when `bind` is not a
    /// loopback address (two-PC setups, servers).
    pub key: Option<String>,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 1935)),
            grace_seconds: 30,
            key: None,
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
    /// Keep a rolling buffer of the stream so delay can be added instantly
    /// (Rewind). When off, only what the current delay needs is kept and every
    /// increase uses Mask.
    pub keep_buffer: bool,
}

impl Default for DelayConfig {
    fn default() -> Self {
        Self {
            max_seconds: 120,
            start_seconds: 0.0,
            default_mode: DelayMode::Rewind,
            presets: Preset::defaults(),
            ram_cap_mb: 512,
            keep_buffer: true,
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

/// A backup of OBS's stream settings. The settings themselves (server, stream key,
/// any server password) are kept in the secret store as [`secret::OBS_BACKUP`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObsBackup {
    pub service_type: String,
    /// The OBS they were read from (`host:port`); they are only put back there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obs: Option<String>,
    /// Settings as older versions saved them, without the stream key. Moved into
    /// the secret store at startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub enabled: bool,
    pub go_live: String,
    pub go_live_after_air: String,
    /// Ends the broadcast without airing the buffer. Unassigned by default so it
    /// can't be pressed by accident.
    pub end_stream: String,
    /// Ends the broadcast once what is buffered has aired. Unassigned by default.
    pub end_stream_after_air: String,
    /// Throws away what has not aired yet. Unassigned by default.
    pub dump: String,
    /// One shortcut per delay preset, in order. Empty strings are unassigned.
    pub presets: Vec<String>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            go_live: "CmdOrCtrl+Alt+Shift+L".into(),
            go_live_after_air: "CmdOrCtrl+Alt+Shift+A".into(),
            end_stream: String::new(),
            end_stream_after_air: String::new(),
            dump: String::new(),
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
        replace_private(path, text.as_bytes()).map_err(err)
    }
}

/// Replaces `path` with a private file holding `data` (see [`write_private`]):
/// written next to it first and then moved into place, so a crash or a full disk
/// leaves either the old file or the new one, never a partial one.
pub(crate) fn replace_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    write_private(&tmp, data)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Generates a random 128-bit API token.
pub fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes a file readable only by the current user: mode 0600 on Unix, and on
/// Windows an access list naming only the current user, with nothing inherited
/// from the folder. The file is restricted before anything is written, and
/// nothing is written if that fails.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        // `mode` only applies to new files: tighten an existing one before writing,
        // which also fails (so nothing is changed) if another user owns it.
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.set_len(0)?;
        f.write_all(data)?;
        f.sync_all()
    }
    #[cfg(windows)]
    {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .access_mode(GENERIC_WRITE | WRITE_DAC | READ_CONTROL)
            .open(path)?;
        windows_acl::restrict_to_current_user(&f)?;
        f.set_len(0)?;
        f.write_all(data)?;
        f.sync_all()
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::write(path, data)
    }
}

/// Windows access lists, through the Win32 API: the only unsafe code in the
/// project (see this crate's `Cargo.toml`).
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_acl {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
        SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetTokenInformation, NO_INHERITANCE,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The current user's `TOKEN_USER`, in a buffer aligned for it.
    fn current_user() -> io::Result<Vec<u64>> {
        let mut token: HANDLE = null_mut();
        // SAFETY: the pseudo-handle of the current process needs no closing;
        // `token` is closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        // SAFETY: asks for the size only.
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `len` bytes.
        let ok = unsafe {
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len)
        };
        let err = io::Error::last_os_error();
        // SAFETY: opened above.
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err(err);
        }
        Ok(buf)
    }

    /// Replaces the file's access list with one entry, full control for the
    /// current user, and stops it inheriting any from its folder.
    pub fn restrict_to_current_user(file: &File) -> io::Result<()> {
        let user = current_user()?;
        // SAFETY: `current_user` filled the buffer with a TOKEN_USER.
        let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.cast(),
            },
        };
        let mut acl: *mut ACL = null_mut();
        // SAFETY: one entry, no old list; the new list is freed below.
        let r = unsafe { SetEntriesInAclW(1, &access, null(), &mut acl) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        // SAFETY: `file` is open with WRITE_DAC; `acl` is valid until freed.
        let r = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl,
                null(),
            )
        };
        // SAFETY: allocated by SetEntriesInAclW.
        unsafe { LocalFree(acl.cast()) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Even in a folder shared with everyone, the file names only its owner.
    #[cfg(windows)]
    #[test]
    fn private_files_do_not_inherit_access_on_windows() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let shared = Command::new("icacls")
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .output()
            .unwrap();
        assert!(shared.status.success(), "{shared:?}");
        let path = dir.path().join("secrets.toml");
        write_private(&path, b"key = 'x'").unwrap();
        // Again, over an existing file.
        write_private(&path, b"key = 'y'").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "key = 'y'");
        let acl = Command::new("icacls").arg(&path).output().unwrap();
        let acl = String::from_utf8_lossy(&acl.stdout).to_string();
        let user = std::env::var("USERNAME").unwrap().to_lowercase();
        let entries: Vec<&str> = acl.lines().take_while(|l| !l.trim().is_empty()).collect();
        assert_eq!(entries.len(), 1, "{acl}");
        assert!(
            entries[0].to_lowercase().contains(&format!("{user}:(f)")),
            "{acl}"
        );
        assert!(!acl.contains("(I)"), "inherited entries: {acl}");
    }

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
