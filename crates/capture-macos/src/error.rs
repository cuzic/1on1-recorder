#[derive(thiserror::Error, Debug)]
pub enum CaptureError {
    #[error("ScreenCaptureKit call failed: {0}")]
    ScreenCaptureKit(String),

    #[error("CoreAudio call failed: {0}")]
    CoreAudio(String),

    #[error("target device or application not found: {0}")]
    DeviceNotFound(String),

    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),

    /// TCC denial is a first-class, expected error mode on macOS (design.md §5.2)
    /// with no Windows analogue — callers (the future `macos_supervisor`) should be
    /// able to distinguish this from a transient device error and surface a
    /// "grant permission" prompt instead of a generic capture failure.
    #[error("permission denied: {service}")]
    PermissionDenied { service: TccService },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The two separate TCC grants design.md §5.2 requires for macOS capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TccService {
    ScreenAndSystemAudioRecording,
    Microphone,
}

impl std::fmt::Display for TccService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TccService::ScreenAndSystemAudioRecording => {
                write!(f, "Screen & System Audio Recording")
            }
            TccService::Microphone => write!(f, "Microphone"),
        }
    }
}
