//! OS keyring credential storage (design.md §12.4), ported from
//! spike-10-credential-store. Application-internal, not a publishing candidate —
//! it's a thin, app-specific adapter over the `keyring` crate.

mod encrypted_file;
mod error;
mod os_keyring;
mod token_provider_adapter;

pub use encrypted_file::EncryptedFileStore;
pub use error::StoreError;
pub use os_keyring::OsKeyringStore;
pub use token_provider_adapter::CredentialStoreTokenProvider;

pub trait CredentialStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError>;
    fn load(&self, service: &str, account: &str) -> Result<String, StoreError>;
    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError>;
}

/// design.md §12.4: tries the OS keyring first, and falls back to
/// `EncryptedFileStore` only when the backend itself is unavailable (e.g. a headless
/// Linux with no Secret Service provider registered).
///
/// **Why the fallback condition is narrow**: if this fell back on *any* keyring error
/// — including "the backend is running but denied access" or "temporarily locked" —
/// a user could silently drop to the weaker encrypted-file protection without
/// intending to. Only errors that indicate the backend itself doesn't exist trigger
/// the fallback.
pub struct FallbackCredentialStore {
    primary: OsKeyringStore,
    fallback: EncryptedFileStore,
}

impl FallbackCredentialStore {
    pub fn new(fallback_dir: impl Into<std::path::PathBuf>) -> Result<Self, StoreError> {
        Ok(Self { primary: OsKeyringStore, fallback: EncryptedFileStore::new(fallback_dir)? })
    }

    fn should_fallback(err: &StoreError) -> bool {
        match err {
            StoreError::Backend(msg) => {
                // The `keyring` crate's Linux (secret-service) backend reports a
                // missing Secret Service provider as a D-Bus connection error string
                // (observed in this environment: messages containing "org.freedesktop.
                // secrets", "NoSuchMethod"/"ServiceUnknown"). This implementation-
                // dependent string match is a stopgap — replace it with a proper error
                // classification once `keyring` (or a wrapper) exposes one.
                let lower = msg.to_lowercase();
                lower.contains("no such method")
                    || lower.contains("serviceunknown")
                    || lower.contains("was not provided by any")
                    || lower.contains("could not connect")
                    || lower.contains("platform secure storage failure")
            }
            _ => false,
        }
    }
}

impl CredentialStore for FallbackCredentialStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError> {
        match self.primary.save(service, account, secret) {
            Ok(()) => Ok(()),
            Err(e) if Self::should_fallback(&e) => {
                tracing::warn!(error = %e, "OS keyring unavailable; falling back to encrypted file store");
                self.fallback.save(service, account, secret)
            }
            Err(e) => Err(e),
        }
    }

    fn load(&self, service: &str, account: &str) -> Result<String, StoreError> {
        match self.primary.load(service, account) {
            Ok(secret) => Ok(secret),
            Err(e) if Self::should_fallback(&e) => self.fallback.load(service, account),
            Err(e) => Err(e),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError> {
        match self.primary.delete(service, account) {
            Ok(()) => Ok(()),
            Err(e) if Self::should_fallback(&e) => self.fallback.delete(service, account),
            Err(e) => Err(e),
        }
    }
}
