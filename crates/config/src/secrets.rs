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

/// An OS credential store, as [`Secrets`] uses it.
pub trait Keychain: Send + Sync {
    /// `Ok(None)` when there is no such entry.
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    /// Succeeds when there is no such entry.
    fn delete(&self, name: &str) -> Result<(), String>;
}

/// Keychain-backed store that falls back to a private file.
///
/// A value is in one of the two places. When the keychain refuses a write, the
/// value goes to the file, and the file then takes precedence: reads look there
/// first, so an older copy the keychain still holds never comes back. A value
/// written to the keychain is removed from the file. Deleting fails unless both
/// copies are gone.
pub struct Secrets {
    keychain: Option<Box<dyn Keychain>>,
    file: PathBuf,
    lock: Mutex<()>,
}

impl Secrets {
    /// `dir` holds the fallback `secrets.toml`. Set `use_keychain` to false for
    /// headless installs.
    pub fn new(dir: &Path, use_keychain: bool) -> Self {
        let keychain = (use_keychain && keychain::available())
            .then(|| Box::new(keychain::Os) as Box<dyn Keychain>);
        if keychain.is_none() {
            debug!("OS keychain unavailable; secrets are stored in a private file");
        }
        Self::with_keychain(dir, keychain)
    }

    /// With a given keychain (or none), for tests.
    pub fn with_keychain(dir: &Path, keychain: Option<Box<dyn Keychain>>) -> Self {
        Self {
            keychain,
            file: dir.join("secrets.toml"),
            lock: Mutex::new(()),
        }
    }

    /// The file's secrets. One that cannot be parsed counts as empty (nothing in
    /// it can be read back either) and is replaced on the next write; one that
    /// cannot be read is an error, as it may hold a value that must not be
    /// shadowed or kept.
    fn read_file(&self) -> Result<BTreeMap<String, String>, String> {
        match fs::read_to_string(&self.file) {
            Ok(t) => Ok(toml::from_str(&t).unwrap_or_else(|e| {
                warn!("ignoring unreadable {}: {e}", self.file.display());
                BTreeMap::new()
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(format!("{}: {e}", self.file.display())),
        }
    }

    fn write_file(&self, map: &BTreeMap<String, String>) -> Result<(), String> {
        if let Some(dir) = self.file.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string(map).map_err(|e| e.to_string())?;
        write_private(&self.file, text.as_bytes()).map_err(|e| e.to_string())
    }

    /// Removes `name` from the file, if it is there.
    fn remove_from_file(&self, name: &str) -> Result<(), String> {
        let mut map = self.read_file()?;
        if map.remove(name).is_some() {
            self.write_file(&map)?;
        }
        Ok(())
    }
}

impl SecretStore for Secrets {
    fn get(&self, name: &str) -> Option<String> {
        let _g = self.lock.lock().ok()?;
        match self.read_file() {
            Ok(map) => {
                if let Some(v) = map.get(name) {
                    return Some(v.clone());
                }
            }
            // Unreadable: the keychain may still hold the current value.
            Err(e) => warn!("could not read the secrets file: {e}"),
        }
        match self.keychain.as_ref()?.get(name) {
            Ok(v) => v,
            Err(e) => {
                warn!("could not read {name} from the keychain: {e}");
                None
            }
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let _g = self.lock.lock().map_err(|e| e.to_string())?;
        if let Some(k) = &self.keychain {
            match k.set(name, value) {
                // A copy left in the file would take precedence over this one.
                Ok(()) => return self.remove_from_file(name),
                Err(e) => warn!("keychain write failed ({e}); using the private file"),
            }
        }
        let mut map = self.read_file()?;
        map.insert(name.into(), value.into());
        self.write_file(&map)?;
        if let Some(k) = &self.keychain
            && let Err(e) = k.delete(name)
        {
            // Harmless: the file copy takes precedence.
            warn!("could not remove the older copy of {name} from the keychain: {e}");
        }
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let _g = self.lock.lock().map_err(|e| e.to_string())?;
        self.remove_from_file(name)?;
        if let Some(k) = &self.keychain {
            k.delete(name)
                .map_err(|e| format!("could not remove it from the keychain: {e}"))?;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        if self.keychain.is_some() {
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
    use keyring::{Entry, Error};

    pub fn available() -> bool {
        // Probe with a harmless read; "no entry" means the store works.
        match Entry::new(super::SERVICE, "probe") {
            Ok(e) => matches!(e.get_password(), Ok(_) | Err(Error::NoEntry)),
            Err(_) => false,
        }
    }

    /// The OS credential store.
    pub struct Os;

    impl super::Keychain for Os {
        fn get(&self, name: &str) -> Result<Option<String>, String> {
            let entry = Entry::new(super::SERVICE, name).map_err(|e| e.to_string())?;
            match entry.get_password() {
                Ok(v) => Ok(Some(v)),
                Err(Error::NoEntry) => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }

        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            Entry::new(super::SERVICE, name)
                .and_then(|e| e.set_password(value))
                .map_err(|e| e.to_string())
        }

        fn delete(&self, name: &str) -> Result<(), String> {
            let entry = Entry::new(super::SERVICE, name).map_err(|e| e.to_string())?;
            match entry.delete_credential() {
                Ok(()) | Err(Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
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

    /// Never used: `available` is false.
    pub struct Os;

    impl super::Keychain for Os {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Err("no keychain support in this build".into())
        }
        fn delete(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
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

    /// A keychain whose writes and deletes can be made to fail.
    #[derive(Default, Clone)]
    struct FlakyKeychain {
        map: std::sync::Arc<Mutex<BTreeMap<String, String>>>,
        fail_set: std::sync::Arc<std::sync::atomic::AtomicBool>,
        fail_delete: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Keychain for FlakyKeychain {
        fn get(&self, name: &str) -> Result<Option<String>, String> {
            Ok(self.map.lock().unwrap().get(name).cloned())
        }
        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            if self.fail_set.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("locked".into());
            }
            self.map.lock().unwrap().insert(name.into(), value.into());
            Ok(())
        }
        fn delete(&self, name: &str) -> Result<(), String> {
            if self.fail_delete.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("access denied".into());
            }
            self.map.lock().unwrap().remove(name);
            Ok(())
        }
    }

    fn flaky(dir: &Path) -> (Secrets, FlakyKeychain) {
        let k = FlakyKeychain::default();
        (Secrets::with_keychain(dir, Some(Box::new(k.clone()))), k)
    }

    #[test]
    fn a_failed_keychain_delete_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = flaky(dir.path());
        s.set("k", "old").unwrap();
        k.fail_delete
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(s.delete("k").is_err());
        assert_eq!(
            s.get("k").as_deref(),
            Some("old"),
            "still there, and said so"
        );
        k.fail_delete
            .store(false, std::sync::atomic::Ordering::Relaxed);
        s.delete("k").unwrap();
        assert_eq!(s.get("k"), None);
    }

    #[test]
    fn a_value_written_to_the_file_wins_over_an_older_keychain_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = flaky(dir.path());
        s.set("k", "old").unwrap();
        // The keychain refuses the new value, and even removing the old one.
        k.fail_set.store(true, std::sync::atomic::Ordering::Relaxed);
        k.fail_delete
            .store(true, std::sync::atomic::Ordering::Relaxed);
        s.set("k", "new").unwrap();
        assert_eq!(s.get("k").as_deref(), Some("new"));
        // Deleting must remove both copies, or say it could not.
        assert!(s.delete("k").is_err());
        assert_eq!(s.get("k").as_deref(), Some("old"));
        // The keychain works again: a new value there replaces the file copy.
        k.fail_set
            .store(false, std::sync::atomic::Ordering::Relaxed);
        k.fail_delete
            .store(false, std::sync::atomic::Ordering::Relaxed);
        s.set("k", "newest").unwrap();
        assert_eq!(s.get("k").as_deref(), Some("newest"));
        assert!(
            !fs::read_to_string(dir.path().join("secrets.toml"))
                .unwrap()
                .contains("new")
        );
        s.delete("k").unwrap();
        assert_eq!(s.get("k"), None);
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
