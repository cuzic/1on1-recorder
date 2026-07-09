#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("OS keyring backend unavailable or errored: {0}")]
    Backend(String),
    #[error("no credential found for {service}/{account}")]
    NotFound { service: String, account: String },
    #[error("encrypted file store I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("encrypted file store crypto error: {0}")]
    Crypto(String),
}
