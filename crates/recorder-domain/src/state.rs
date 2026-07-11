/// design.md §10. Independent from `UploadState`: a session's recording can fail while
/// already-committed segments continue uploading, and uploads can fail or pause without
/// interrupting the recording. See design.md §10's state diagram for the transition
/// rules (`Idle → Preparing → Recording → Stopping → Finalizing → Finalized`, with
/// `Failed` reachable from `Preparing`/`Recording` and still draining into `Finalizing`
/// so already-captured data is preserved).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CaptureState {
    Idle,
    Preparing,
    Recording,
    Stopping,
    Finalizing,
    Finalized,
    Failed { recoverable: bool, reason: String },
}

/// design.md §10. Per-segment upload lifecycle, independent from `CaptureState` — see
/// that type's doc comment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UploadState {
    NotStarted,
    Pending,
    Uploading,
    WaitingForNetwork,
    Paused,
    Completed,
    Failed { retryable: bool, reason: String },
}
