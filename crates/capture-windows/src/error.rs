#[derive(thiserror::Error, Debug)]
pub enum CaptureError {
    #[error("COM/WASAPI call failed: {0}")]
    Com(#[from] windows::core::Error),

    #[error("target device not found: {0}")]
    DeviceNotFound(String),

    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
