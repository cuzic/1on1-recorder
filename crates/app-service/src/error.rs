#[derive(Debug, thiserror::Error)]
pub enum AppServiceError {
    #[error("segment-store error: {0}")]
    SegmentStore(#[from] segment_store::SegmentStoreError),
    #[error("session-store error: {0}")]
    SessionStore(#[from] session_store::StoreError),
    #[error("upload error: {0}")]
    Upload(#[from] recorder_domain::UploadError),
    /// `capture_windows::CaptureError`, stringified rather than wrapped with
    /// `#[from]` — that type only exists under the (optional, Windows-only)
    /// `windows-supervisor` feature, and this error type must stay buildable
    /// without it for stage 1's OS-independent default build.
    #[error("capture error: {0}")]
    Capture(String),
}
