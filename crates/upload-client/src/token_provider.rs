use async_trait::async_trait;
use recorder_domain::UploadError;

/// The seam `credential-store` (task #5) plugs into — `upload-client` never reads a
/// token or a credential store directly, so swapping how tokens are obtained never
/// touches this crate.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    /// The current bearer token to send with a request.
    async fn access_token(&self) -> Result<String, UploadError>;

    /// Called once after a 401 (design.md §13.3: "401はトークン更新後に1回再送"),
    /// before `HttpUploadClient` retries the same request exactly once.
    async fn refresh(&self) -> Result<(), UploadError>;
}

/// A fixed token that never refreshes — for tests and local/manual runs only. A real
/// deployment must supply a `TokenProvider` backed by `credential-store`.
pub struct StaticTokenProvider(pub String);

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn access_token(&self) -> Result<String, UploadError> {
        Ok(self.0.clone())
    }

    async fn refresh(&self) -> Result<(), UploadError> {
        Ok(())
    }
}
