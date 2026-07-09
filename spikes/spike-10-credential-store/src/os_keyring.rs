//! spike-plan.md SPIKE-10 検証手順1: `keyring` crateでのsave/load/delete。
//! Windows Credential Manager / macOS Keychain / Linux Secret Serviceを
//! 同じAPIで吸収する(3 OSともこのモジュールをそのまま使う)。

use crate::error::StoreError;
use crate::CredentialStore;

pub struct OsKeyringStore;

impl CredentialStore for OsKeyringStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        entry
            .set_password(secret)
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn load(&self, service: &str, account: &str) -> Result<String, StoreError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(secret),
            Err(keyring::Error::NoEntry) => Err(StoreError::NotFound {
                service: service.to_string(),
                account: account.to_string(),
            }),
            Err(e) => Err(StoreError::Backend(e.to_string())),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match entry.delete_password() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()), // 既に無いなら削除成功と同義
            Err(e) => Err(StoreError::Backend(e.to_string())),
        }
    }
}
