//! `CaptureState`/`UploadState` as flat, queryable columns instead of a serialized
//! blob — `state_tag` alone lets `reconcile_on_startup`/`pending_uploads` filter with a
//! plain `WHERE state_tag NOT IN (...)`, without needing SQLite's JSON1 extension.

use recorder_domain::{CaptureState, UploadState};

use crate::error::StoreError;

pub fn capture_state_tag(state: &CaptureState) -> &'static str {
    match state {
        CaptureState::Idle => "idle",
        CaptureState::Preparing => "preparing",
        CaptureState::Recording => "recording",
        CaptureState::Stopping => "stopping",
        CaptureState::Finalizing => "finalizing",
        CaptureState::Finalized => "finalized",
        CaptureState::Failed { .. } => "failed",
    }
}

pub fn capture_state_detail(state: &CaptureState) -> (Option<bool>, Option<String>) {
    match state {
        CaptureState::Failed { recoverable, reason } => (Some(*recoverable), Some(reason.clone())),
        _ => (None, None),
    }
}

pub fn decode_capture_state(
    tag: &str,
    recoverable: Option<bool>,
    reason: Option<String>,
) -> Result<CaptureState, StoreError> {
    Ok(match tag {
        "idle" => CaptureState::Idle,
        "preparing" => CaptureState::Preparing,
        "recording" => CaptureState::Recording,
        "stopping" => CaptureState::Stopping,
        "finalizing" => CaptureState::Finalizing,
        "finalized" => CaptureState::Finalized,
        "failed" => CaptureState::Failed {
            recoverable: recoverable.unwrap_or(false),
            reason: reason.unwrap_or_default(),
        },
        other => return Err(StoreError::UnknownStateTag(other.to_string())),
    })
}

pub fn upload_state_tag(state: &UploadState) -> &'static str {
    match state {
        UploadState::NotStarted => "not_started",
        UploadState::Pending => "pending",
        UploadState::Uploading => "uploading",
        UploadState::WaitingForNetwork => "waiting_for_network",
        UploadState::Paused => "paused",
        UploadState::Completed => "completed",
        UploadState::Failed { .. } => "failed",
    }
}

pub fn upload_state_detail(state: &UploadState) -> (Option<bool>, Option<String>) {
    match state {
        UploadState::Failed { retryable, reason } => (Some(*retryable), Some(reason.clone())),
        _ => (None, None),
    }
}

pub fn decode_upload_state(
    tag: &str,
    retryable: Option<bool>,
    reason: Option<String>,
) -> Result<UploadState, StoreError> {
    Ok(match tag {
        "not_started" => UploadState::NotStarted,
        "pending" => UploadState::Pending,
        "uploading" => UploadState::Uploading,
        "waiting_for_network" => UploadState::WaitingForNetwork,
        "paused" => UploadState::Paused,
        "completed" => UploadState::Completed,
        "failed" => UploadState::Failed {
            retryable: retryable.unwrap_or(false),
            reason: reason.unwrap_or_default(),
        },
        other => return Err(StoreError::UnknownStateTag(other.to_string())),
    })
}
