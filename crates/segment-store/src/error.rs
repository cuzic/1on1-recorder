#[derive(Debug, thiserror::Error)]
pub enum SegmentStoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("opus encode error: {0}")]
    Opus(#[from] opus::Error),
    #[error("ogg container error: {0}")]
    Ogg(#[from] ogg::OggReadError),
    #[error("session-store error: {0}")]
    Store(#[from] session_store::StoreError),
}
