#[derive(Debug, thiserror::Error)]
pub enum AppServiceError {
    #[error("segment-store error: {0}")]
    SegmentStore(#[from] segment_store::SegmentStoreError),
    #[error("session-store error: {0}")]
    SessionStore(#[from] session_store::StoreError),
    #[error("upload error: {0}")]
    Upload(#[from] recorder_domain::UploadError),
}
