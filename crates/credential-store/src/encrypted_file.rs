//! Fallback for when the OS keyring backend itself is unavailable (design.md §12.4 —
//! e.g. a headless Linux without a Secret Service provider registered).
//!
//! **Protection level note**: the master key is stored in plaintext on disk, with only
//! OS file permissions (0600) as the protection boundary. This is *not* the same
//! strength as Windows Credential Manager (DPAPI, tied to the user's login
//! credentials) or macOS Keychain (its own OS-managed key store) — it only protects
//! against other users on the same machine, not an attacker who can read the disk
//! directly or has root. Any deployment that relies on this path must document that
//! limitation to the user (UI/logs/docs), not just in this comment.

use crate::error::StoreError;
use crate::CredentialStore;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use rand::RngCore;
use std::path::{Path, PathBuf};

pub struct EncryptedFileStore {
    dir: PathBuf,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl EncryptedFileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { dir })
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join(".master.key")
    }

    fn entry_path(&self, service: &str, account: &str) -> PathBuf {
        let id = format!("{service}:{account}");
        self.dir.join(hex_encode(id.as_bytes()))
    }

    fn load_or_create_master_key(&self) -> Result<[u8; 32], StoreError> {
        let path = self.key_path();
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                return Ok(key);
            }
        }
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        write_private_file(&path, &key)?;
        Ok(key)
    }
}

fn write_private_file(path: &Path, data: &[u8]) -> Result<(), StoreError> {
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    // Generate the nonce via `rand`'s OsRng directly rather than aes-gcm's own
    // `AeadCore::generate_nonce`, to avoid pinning this crate to whatever rand_core
    // version aes-gcm's `aead` dependency happens to bundle.
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext).map_err(|e| StoreError::Crypto(e.to_string()))?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>, StoreError> {
    if data.len() < 12 {
        return Err(StoreError::Crypto("ciphertext too short".to_string()));
    }
    let (nonce_bytes, ciphertext) = data.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ciphertext).map_err(|e| StoreError::Crypto(e.to_string()))
}

impl CredentialStore for EncryptedFileStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError> {
        let key = self.load_or_create_master_key()?;
        let ciphertext = encrypt(&key, secret.as_bytes())?;
        write_private_file(&self.entry_path(service, account), &ciphertext)
    }

    fn load(&self, service: &str, account: &str) -> Result<String, StoreError> {
        let path = self.entry_path(service, account);
        let data = std::fs::read(&path).map_err(|_| StoreError::NotFound { service: service.to_string(), account: account.to_string() })?;
        let key = self.load_or_create_master_key()?;
        let plaintext = decrypt(&key, &data)?;
        String::from_utf8(plaintext).map_err(|e| StoreError::Crypto(e.to_string()))
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError> {
        let path = self.entry_path(service, account);
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
