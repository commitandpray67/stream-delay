//! Secret storage: the OS keychain, with a private file as fallback.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::{debug, warn};

use crate::write_private;

const SERVICE: &str = "dev.stream-delay";

/// Where secrets are kept.
pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    fn delete(&self, name: &str) -> Result<(), String>;
    /// Human-readable description, for the UI.
    fn describe(&self) -> String;
}

/// Keychain-backed store that transparently falls back to a private file.
pub struct Secrets {
    use_keychain: bool,
    file: PathBuf,
    lock: Mutex<()>,
}

impl Secrets {
    /// `dir` holds the fallback `secrets.toml`. Set `use_keychain` to false for
    /// headless installs.
    pub fn new(dir: &Path, use_keychain: bool) -> Self {
        let use_keychain = use_keychain && keychain::available();
        if !use_keychain {
            debug!("OS keychain unavailable; secrets are stored in a private file");
        }
        Self {
            use_keychain,
            file: dir.join("secrets.toml"),
            lock: Mutex::new(()),
        }
    }

    fn read_file(&self) -> BTreeMap<String, String> {
        fs::read_to_string(&self.file)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn write_file(&self, map: &BTreeMap<String, String>) -> Result<(), String> {
        if let Some(dir) = self.file.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string(map).map_err(|e| e.to_string())?;
        write_private(&self.file, text.as_bytes()).map_err(|e| e.to_string())
    }
}

impl SecretStore for Secrets {
    fn get(&self, name: &str) -> Option<String> {
        let _g = self.lock.lock().ok()?;
        if self.use_keychain
            && let Some(v) = keychain::get(SERVICE, name)
        {
            return Some(v);
        }
        self.read_file().get(name).cloned()
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let _g = self.lock.lock().map_err(|e| e.to_string())?;
        if self.use_keychain {
            match keychain::set(SERVICE, name, value) {
                Ok(()) => {
                    // Remove any older copy from the fallback file.
                    let mut map = self.read_file();
                    if map.remove(name).is_some() {
                        self.write_file(&map)?;
                    }
                    return Ok(());
                }
                Err(e) => warn!("keychain write failed ({e}); using the private file"),
            }
        }
        let mut map = self.read_file();
        map.insert(name.into(), value.into());
        self.write_file(&map)
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let _g = self.lock.lock().map_err(|e| e.to_string())?;
        if self.use_keychain {
            keychain::delete(SERVICE, name);
        }
        let mut map = self.read_file();
        if map.remove(name).is_some() {
            self.write_file(&map)?;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        if self.use_keychain {
            "your system keychain".into()
        } else {
            format!("{} (readable only by you)", self.file.display())
        }
    }
}

/// Secrets kept in memory only, for `--ephemeral` runs: nothing touches the disk
/// and everything is forgotten when the process exits.
#[derive(Default)]
pub struct MemorySecrets {
    map: Mutex<BTreeMap<String, String>>,
}

impl SecretStore for MemorySecrets {
    fn get(&self, name: &str) -> Option<String> {
        self.map.lock().ok()?.get(name).cloned()
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        self.map
            .lock()
            .map_err(|e| e.to_string())?
            .insert(name.into(), value.into());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        self.map.lock().map_err(|e| e.to_string())?.remove(name);
        Ok(())
    }

    fn describe(&self) -> String {
        "memory only (forgotten when stream-delay stops)".into()
    }
}

#[cfg(all(
    feature = "keychain",
    any(target_os = "macos", windows, target_os = "linux")
))]
mod keychain {
    pub fn available() -> bool {
        // Probe with a harmless read; "no entry" means the store works.
        match keyring::Entry::new(super::SERVICE, "probe") {
            Ok(e) => matches!(e.get_password(), Ok(_) | Err(keyring::Error::NoEntry)),
            Err(_) => false,
        }
    }

    pub fn get(service: &str, name: &str) -> Option<String> {
        keyring::Entry::new(service, name).ok()?.get_password().ok()
    }

    pub fn set(service: &str, name: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new(service, name)
            .and_then(|e| e.set_password(value))
            .map_err(|e| e.to_string())
    }

    pub fn delete(service: &str, name: &str) {
        if let Ok(e) = keyring::Entry::new(service, name) {
            let _ = e.delete_credential();
        }
    }
}

#[cfg(not(all(
    feature = "keychain",
    any(target_os = "macos", windows, target_os = "linux")
)))]
mod keychain {
    pub fn available() -> bool {
        false
    }
    pub fn get(_: &str, _: &str) -> Option<String> {
        None
    }
    pub fn set(_: &str, _: &str, _: &str) -> Result<(), String> {
        Err("no keychain support in this build".into())
    }
    pub fn delete(_: &str, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Secrets::new(dir.path(), false);
        assert_eq!(s.get("k"), None);
        s.set("k", "live_123").unwrap();
        assert_eq!(s.get("k").as_deref(), Some("live_123"));
        s.delete("k").unwrap();
        assert_eq!(s.get("k"), None);
        assert!(s.describe().contains("secrets.toml"));
    }

    #[test]
    fn memory_store_round_trip() {
        let s = MemorySecrets::default();
        assert_eq!(s.get("k"), None);
        s.set("k", "live_123").unwrap();
        assert_eq!(s.get("k").as_deref(), Some("live_123"));
        s.delete("k").unwrap();
        assert_eq!(s.get("k"), None);
    }

    #[cfg(unix)]
    #[test]
    fn existing_files_are_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secrets.toml");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let s = Secrets::new(dir.path(), false);
        s.set("k", "v").unwrap();
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
