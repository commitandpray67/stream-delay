//! Secret storage: the OS keychain, with a private file as fallback.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use thiserror::Error;
use tracing::{debug, warn};

use crate::replace_private;

const SERVICE: &str = "dev.stream-delay";

/// Why a secret could not be read, saved or removed.
#[derive(Debug, Error)]
pub enum SecretError {
    /// The system keychain failed.
    #[error("the system keychain: {0}")]
    Keychain(String),
    /// The secrets file could not be read or written.
    #[error("{}: {source}", path.display())]
    File { path: PathBuf, source: io::Error },
    /// The secrets file exists but does not parse.
    #[error("{} is damaged: {why}", path.display())]
    Damaged { path: PathBuf, why: String },
    /// The keychain refused a new value and still holds an older one that could
    /// not be removed; keeping both would let the older one come back.
    #[error(
        "the system keychain refused it, and still holds an older copy that could not be \
         removed ({0})"
    )]
    StaleCopy(String),
}

/// Where secrets are kept.
pub trait SecretStore: Send + Sync {
    /// The value, or `None` if there is none or it cannot be read.
    fn get(&self, name: &str) -> Option<String>;
    /// Like [`SecretStore::get`], but tells a value that cannot be read (an error)
    /// from no value (`Ok(None)`).
    fn try_get(&self, name: &str) -> Result<Option<String>, SecretError> {
        Ok(self.get(name))
    }
    fn set(&self, name: &str, value: &str) -> Result<(), SecretError>;
    fn delete(&self, name: &str) -> Result<(), SecretError>;
    /// Human-readable description, for the UI.
    fn describe(&self) -> String;
}

/// An OS credential store, as [`Secrets`] uses it. Its errors are the system's
/// own messages.
pub trait Keychain: Send + Sync {
    /// `Ok(None)` when there is no such entry.
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    /// Succeeds when there is no such entry.
    fn delete(&self, name: &str) -> Result<(), String>;
}

/// Keychain-backed store that falls back to a private file.
///
/// Each value is kept in one place. When the keychain refuses a value, it goes
/// to the file, which reads look at first; an older copy the keychain still
/// holds is removed, and if that fails the value is not saved at all, so an
/// outdated one can never come back from the keychain later. A value the
/// keychain takes is removed from the file. Deleting fails, changing nothing,
/// unless the keychain copy could be removed.
///
/// The file is replaced whole on every write (see [`replace_private`]). One that
/// no longer parses is moved aside to `secrets.toml.damaged`, for recovery,
/// rather than overwritten.
pub struct Secrets {
    keychain: Option<Box<dyn Keychain>>,
    file: PathBuf,
    lock: Mutex<()>,
}

/// What the secrets file holds.
enum FileState {
    Readable(BTreeMap<String, String>),
    /// It exists but does not parse.
    Damaged(String),
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

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn file_error(&self, source: io::Error) -> SecretError {
        SecretError::File {
            path: self.file.clone(),
            source,
        }
    }

    /// The file's content. An error if it cannot be read at all.
    fn read_file(&self) -> Result<FileState, SecretError> {
        match fs::read_to_string(&self.file) {
            Ok(t) => Ok(match toml::from_str(&t) {
                Ok(map) => FileState::Readable(map),
                Err(e) => FileState::Damaged(e.to_string()),
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Ok(FileState::Readable(BTreeMap::new()))
            }
            Err(e) => Err(self.file_error(e)),
        }
    }

    /// The file's secrets, to change and write back. A damaged file is moved
    /// aside first; one that cannot be read is an error.
    fn load_for_write(&self) -> Result<BTreeMap<String, String>, SecretError> {
        match self.read_file()? {
            FileState::Readable(map) => Ok(map),
            FileState::Damaged(why) => {
                let mut aside = self.file.as_os_str().to_owned();
                aside.push(".damaged");
                fs::rename(&self.file, &aside).map_err(|e| self.file_error(e))?;
                warn!(
                    "{} was damaged ({why}); it was moved to {} and a new one started",
                    self.file.display(),
                    PathBuf::from(aside).display()
                );
                Ok(BTreeMap::new())
            }
        }
    }

    fn write_file(&self, map: &BTreeMap<String, String>) -> Result<(), SecretError> {
        if let Some(dir) = self.file.parent() {
            fs::create_dir_all(dir).map_err(|e| self.file_error(e))?;
        }
        let text = toml::to_string(map).map_err(|e| SecretError::Damaged {
            path: self.file.clone(),
            why: e.to_string(),
        })?;
        replace_private(&self.file, text.as_bytes()).map_err(|e| self.file_error(e))
    }

    /// Removes `name` from the file, if it is there.
    fn remove_from_file(&self, name: &str) -> Result<(), SecretError> {
        let mut map = self.load_for_write()?;
        if map.remove(name).is_some() {
            self.write_file(&map)?;
        }
        Ok(())
    }

    fn get_locked(&self, name: &str) -> Result<Option<String>, SecretError> {
        let file_error = match self.read_file() {
            Ok(FileState::Readable(map)) => match map.get(name) {
                Some(v) => return Ok(Some(v.clone())),
                None => None,
            },
            // Nothing in it is readable; values are kept in one place only, so the
            // keychain holds none of what it held.
            Ok(FileState::Damaged(why)) => Some(SecretError::Damaged {
                path: self.file.clone(),
                why,
            }),
            Err(e) => Some(e),
        };
        let from_keychain = match &self.keychain {
            Some(k) => k.get(name).map_err(SecretError::Keychain)?,
            None => None,
        };
        match (from_keychain, file_error) {
            (Some(v), _) => Ok(Some(v)),
            (None, Some(e)) => Err(e),
            (None, None) => Ok(None),
        }
    }
}

impl SecretStore for Secrets {
    fn get(&self, name: &str) -> Option<String> {
        self.try_get(name).unwrap_or_else(|e| {
            warn!("could not read {name}: {e}");
            None
        })
    }

    fn try_get(&self, name: &str) -> Result<Option<String>, SecretError> {
        let _g = self.lock();
        self.get_locked(name)
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretError> {
        let _g = self.lock();
        if let Some(k) = &self.keychain {
            match k.set(name, value) {
                Ok(()) => {
                    // A copy left in the file would take precedence over this one.
                    if let Err(e) = self.remove_from_file(name) {
                        // Undo, so the file's value stays the one in effect.
                        if let Err(undo) = k.delete(name) {
                            warn!("could not undo saving {name} to the keychain: {undo}");
                        }
                        return Err(e);
                    }
                    return Ok(());
                }
                Err(e) => warn!("keychain write failed ({e}); using the private file"),
            }
        }
        let mut map = self.load_for_write()?;
        let before = map.clone();
        map.insert(name.into(), value.into());
        self.write_file(&map)?;
        if let Some(k) = &self.keychain {
            // Two copies must not be kept: the older one would come back if the
            // file were ever lost. Keep the old state if it cannot be removed.
            let older = k.get(name).map(|v| v.is_some());
            if older != Ok(false)
                && let Err(e) = k.delete(name)
            {
                if let Err(undo) = self.write_file(&before) {
                    warn!("could not undo saving {name} to the private file: {undo}");
                }
                return Err(SecretError::StaleCopy(e));
            }
        }
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), SecretError> {
        let _g = self.lock();
        // The keychain first: if that fails, nothing has changed.
        if let Some(k) = &self.keychain {
            k.delete(name).map_err(SecretError::Keychain)?;
        }
        self.remove_from_file(name)
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

impl MemorySecrets {
    fn map(&self) -> MutexGuard<'_, BTreeMap<String, String>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SecretStore for MemorySecrets {
    fn get(&self, name: &str) -> Option<String> {
        self.map().get(name).cloned()
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretError> {
        self.map().insert(name.into(), value.into());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), SecretError> {
        self.map().remove(name);
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
    fn a_value_is_never_kept_in_two_places() {
        use std::sync::atomic::Ordering::Relaxed;
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = flaky(dir.path());
        s.set("k", "old").unwrap();
        // The keychain refuses the new value and cannot remove the old one:
        // nothing is saved, rather than two copies.
        k.fail_set.store(true, Relaxed);
        k.fail_delete.store(true, Relaxed);
        assert!(matches!(s.set("k", "new"), Err(SecretError::StaleCopy(_))));
        assert_eq!(s.get("k").as_deref(), Some("old"));
        // It can remove the old one: the new value goes to the file alone.
        k.fail_delete.store(false, Relaxed);
        s.set("k", "new").unwrap();
        assert_eq!(s.get("k").as_deref(), Some("new"));
        assert_eq!(k.get("k").unwrap(), None);
        // So losing the file can never bring the old value back.
        fs::write(dir.path().join("secrets.toml"), "not = [toml").unwrap();
        assert_eq!(s.get("k"), None);
        assert!(
            s.try_get("k").is_err(),
            "a damaged file is an error, not an empty one"
        );
        // The keychain works again: the damaged file is set aside, not lost.
        k.fail_set.store(false, Relaxed);
        s.set("k", "newest").unwrap();
        assert_eq!(s.get("k").as_deref(), Some("newest"));
        assert_eq!(
            fs::read_to_string(dir.path().join("secrets.toml.damaged")).unwrap(),
            "not = [toml"
        );
        // Deleting fails, changing nothing, unless the keychain copy goes.
        k.fail_delete.store(true, Relaxed);
        assert!(s.delete("k").is_err());
        assert_eq!(s.get("k").as_deref(), Some("newest"));
        k.fail_delete.store(false, Relaxed);
        s.delete("k").unwrap();
        assert_eq!(s.get("k"), None);
        assert_eq!(s.try_get("k").unwrap(), None);
    }

    #[test]
    fn a_failed_write_leaves_the_file_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let s = Secrets::new(dir.path(), false);
        s.set("a", "1").unwrap();
        // Where the new file is written first: a folder, so writing fails.
        fs::create_dir(dir.path().join("secrets.toml.tmp")).unwrap();
        assert!(s.set("b", "2").is_err());
        assert_eq!(s.get("a").as_deref(), Some("1"));
        assert_eq!(s.get("b"), None);
        fs::remove_dir(dir.path().join("secrets.toml.tmp")).unwrap();
        s.set("b", "2").unwrap();
        assert_eq!(s.get("b").as_deref(), Some("2"));
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
