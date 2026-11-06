//! Access to passwords, TOTP keys and API keys.
//!
//! The configuration only holds `keyring:<id>` references. The real values live
//! in the operating system's keychain under service `docsweep` and entry
//! `docsweep:<id>`. Values are wrapped in [`SecretString`], whose `Debug` output
//! is redacted and whose memory is cleared on drop.

use std::collections::HashMap;
use std::sync::Mutex;

pub use secrecy::{ExposeSecret, SecretString};

use crate::names;

/// Why a secret could not be read or written. Never contains the secret.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("no secret `{id}` in the keychain; store it with `docsweep secret set {id}`")]
    NotFound { id: String },
    #[error("keychain access for secret `{id}` failed: {message}")]
    Backend { id: String, message: String },
}

/// A store for secrets, addressed by the `<id>` of a `keyring:<id>` reference.
pub trait SecretStore: Send + Sync {
    fn get(&self, id: &str) -> Result<SecretString, SecretError>;
    fn set(&self, id: &str, value: &SecretString) -> Result<(), SecretError>;
}

/// The operating system's keychain.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(id: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(names::KEYRING_SERVICE, &names::keyring_entry(id))
            .map_err(|error| backend_error(id, &error))
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, id: &str) -> Result<SecretString, SecretError> {
        match Self::entry(id)?.get_password() {
            Ok(value) => Ok(SecretString::from(value)),
            Err(keyring::Error::NoEntry) => Err(SecretError::NotFound { id: id.to_owned() }),
            Err(error) => Err(backend_error(id, &error)),
        }
    }

    fn set(&self, id: &str, value: &SecretString) -> Result<(), SecretError> {
        Self::entry(id)?
            .set_password(value.expose_secret())
            .map_err(|error| backend_error(id, &error))
    }
}

fn backend_error(id: &str, error: &keyring::Error) -> SecretError {
    SecretError::Backend {
        id: id.to_owned(),
        message: error.to_string(),
    }
}

/// An in-memory store for tests.
#[derive(Debug, Default)]
pub struct MemoryStore {
    values: Mutex<HashMap<String, SecretString>>,
}

impl MemoryStore {
    /// A store holding one secret.
    pub fn with(id: &str, value: &str) -> Self {
        let store = Self::default();
        store
            .values
            .lock()
            .expect("lock is never poisoned")
            .insert(id.to_owned(), SecretString::from(value.to_owned()));
        store
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, id: &str) -> Result<SecretString, SecretError> {
        self.values
            .lock()
            .expect("lock is never poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| SecretError::NotFound { id: id.to_owned() })
    }

    fn set(&self, id: &str, value: &SecretString) -> Result<(), SecretError> {
        self.values
            .lock()
            .expect("lock is never poisoned")
            .insert(id.to_owned(), value.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips() {
        let store = MemoryStore::default();
        store.set("a", &SecretString::from("value")).unwrap();
        assert_eq!(store.get("a").unwrap().expose_secret(), "value");
    }

    #[test]
    fn missing_secret_tells_how_to_store_it() {
        let error = MemoryStore::default().get("mail").unwrap_err().to_string();
        assert!(error.contains("docsweep secret set mail"), "{error}");
    }

    /// Touches the real keychain and may show a system prompt, so it only runs
    /// on request: `cargo test -p docsweep-core keychain -- --ignored`.
    /// It guards against silently falling back to a non-persistent store.
    #[test]
    #[ignore = "uses the real keychain"]
    fn keychain_round_trips_across_entry_instances() {
        let id = "docsweep-self-test";
        let store = KeyringStore;
        store.set(id, &SecretString::from("self-test")).unwrap();
        let read = store.get(id);
        KeyringStore::entry(id)
            .unwrap()
            .delete_credential()
            .unwrap();
        assert_eq!(read.unwrap().expose_secret(), "self-test");
        assert!(matches!(store.get(id), Err(SecretError::NotFound { .. })));
    }
}
