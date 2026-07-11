use std::sync::Arc;

use async_trait::async_trait;
use recorder_domain::UploadError;
use upload_client::TokenProvider;

use crate::CredentialStore;

/// Adapts any `CredentialStore` into `upload-client`'s `TokenProvider` seam, so
/// `HttpUploadClient` never needs to know tokens come from the OS keyring.
///
/// `load`/`save` are blocking calls (the `keyring` crate and file I/O have no async
/// equivalent) run directly on the async task — acceptable here since a token lookup
/// happens at most once per request-with-retry-cycle and is fast (a syscall or a
/// small file read), not a hot loop.
///
/// `refresh()` is a no-op: a static bearer token stored in the keyring has nothing to
/// refresh. A provider backed by an OAuth-style token endpoint would override this to
/// actually fetch and store a new token.
pub struct CredentialStoreTokenProvider<S: CredentialStore + Send + Sync> {
    store: Arc<S>,
    service: String,
    account: String,
}

impl<S: CredentialStore + Send + Sync> CredentialStoreTokenProvider<S> {
    pub fn new(store: Arc<S>, service: impl Into<String>, account: impl Into<String>) -> Self {
        Self { store, service: service.into(), account: account.into() }
    }
}

#[async_trait]
impl<S: CredentialStore + Send + Sync> TokenProvider for CredentialStoreTokenProvider<S> {
    async fn access_token(&self) -> Result<String, UploadError> {
        self.store.load(&self.service, &self.account).map_err(|e| UploadError::Transport(format!("credential store: {e}")))
    }

    async fn refresh(&self) -> Result<(), UploadError> {
        Ok(())
    }
}
