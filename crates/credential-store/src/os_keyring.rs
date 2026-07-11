//! Windows Credential Manager / macOS Keychain / Linux Secret Service, all through the
//! `keyring` crate's one API — the same module runs unmodified on all three OSes.

use crate::error::StoreError;
use crate::CredentialStore;

pub struct OsKeyringStore;

impl CredentialStore for OsKeyringStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError> {
        let entry = keyring::Entry::new(service, account).map_err(|e| StoreError::Backend(e.to_string()))?;
        entry.set_password(secret).map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn load(&self, service: &str, account: &str) -> Result<String, StoreError> {
        let entry = keyring::Entry::new(service, account).map_err(|e| StoreError::Backend(e.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(secret),
            Err(keyring::Error::NoEntry) => Err(StoreError::NotFound { service: service.to_string(), account: account.to_string() }),
            Err(e) => Err(StoreError::Backend(e.to_string())),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError> {
        let entry = keyring::Entry::new(service, account).map_err(|e| StoreError::Backend(e.to_string()))?;
        match entry.delete_password() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()), // already gone counts as deleted
            Err(e) => Err(StoreError::Backend(e.to_string())),
        }
    }
}
