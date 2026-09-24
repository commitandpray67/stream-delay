//! Secrets saved together with what they belong to: the stream key with its
//! destination server, the OBS password and settings backup with their OBS. A
//! secret is only ever used for its owner, whatever the settings say at the time,
//! so one left behind (a failed delete, two changes at once) cannot go astray.
//!
//! Stored as `{"for": "<owner>", "value": ...}`. Version 0.2.0 and earlier saved
//! the value on its own; such a value has no owner recorded, and each caller
//! decides whom it belongs to (the destination or OBS in the settings file).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use streamdelay_config::{SecretError, SecretStore};

#[derive(Serialize, Deserialize)]
struct Record<T> {
    #[serde(rename = "for")]
    owner: String,
    value: T,
}

/// A saved secret and whom it belongs to.
pub(crate) struct Saved<T> {
    /// `None` for a value saved on its own by an older version.
    pub owner: Option<String>,
    pub value: T,
}

/// Saves `value` as secret `name`, belonging to `owner`.
pub(crate) fn save<T: Serialize>(
    secrets: &dyn SecretStore,
    name: &str,
    owner: &str,
    value: &T,
) -> Result<(), SecretError> {
    let record = serde_json::to_string(&Record {
        owner: owner.to_string(),
        value,
    })
    .expect("strings and JSON values always encode");
    secrets.set(name, &record)
}

/// Secret `name`, if saved. `legacy` reads a value saved on its own by an older
/// version (`None` if it does not make sense).
pub(crate) fn load<T: DeserializeOwned>(
    secrets: &dyn SecretStore,
    name: &str,
    legacy: impl FnOnce(String) -> Option<T>,
) -> Option<Saved<T>> {
    let raw = secrets.get(name)?;
    match serde_json::from_str::<Record<T>>(&raw) {
        Ok(r) => Some(Saved {
            owner: Some(r.owner),
            value: r.value,
        }),
        Err(_) => legacy(raw).map(|value| Saved { owner: None, value }),
    }
}

/// Secret `name` if it belongs to `owner`; one saved on its own by an older
/// version belongs to `legacy_owner`.
pub(crate) fn load_for<T: DeserializeOwned>(
    secrets: &dyn SecretStore,
    name: &str,
    owner: &str,
    legacy_owner: &str,
    legacy: impl FnOnce(String) -> Option<T>,
) -> Option<T> {
    let saved = load(secrets, name, legacy)?;
    (saved.owner.as_deref().unwrap_or(legacy_owner) == owner).then_some(saved.value)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use streamdelay_config::MemorySecrets;

    use super::*;

    #[test]
    fn a_secret_is_only_given_to_its_owner() {
        let s = MemorySecrets::default();
        save(&s, "pw", "127.0.0.1:4455", &"hunter2").unwrap();
        let get = |owner: &str| load_for::<String>(&s, "pw", owner, "127.0.0.1:4455", Some);
        assert_eq!(get("127.0.0.1:4455").as_deref(), Some("hunter2"));
        assert_eq!(get("127.0.0.1:4456"), None);
        // Saved on its own by an older version: its owner is whoever the caller says.
        s.set("pw", "old-style").unwrap();
        assert_eq!(get("127.0.0.1:4455").as_deref(), Some("old-style"));
        assert_eq!(get("127.0.0.1:4456"), None);
        // Structured values too; an old one that is not a value of that kind is none.
        save(&s, "backup", "a", &serde_json::json!({"key": "k"})).unwrap();
        let v = load::<Value>(&s, "backup", |raw| serde_json::from_str(&raw).ok()).unwrap();
        assert_eq!(v.owner.as_deref(), Some("a"));
        assert_eq!(v.value["key"], "k");
        s.set("backup", "not json").unwrap();
        assert!(load::<Value>(&s, "backup", |raw| serde_json::from_str(&raw).ok()).is_none());
    }
}
