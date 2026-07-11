use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::segment::AudioSegment;
use crate::session::{SessionId, SessionManifest};
use crate::track::TrackKind;

/// The API's own identifier for a session, returned by `create_session` and threaded
/// back into every later call — kept distinct from `SessionId` since the two are
/// allowed to differ (e.g. if the API assigns its own opaque ID rather than reusing
/// the ULID verbatim).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSession {
    pub session_id: SessionId,
    pub remote_session_id: String,
}

/// design.md §13.1/§13.2: acknowledges one `PUT .../tracks/{track}/segments/{sequence}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadReceipt {
    pub track: TrackKind,
    pub sequence: u64,
    pub accepted_at: DateTime<Utc>,
}

/// design.md §13.1/§13.3: sent to `finalize_session` only once every segment the local
/// session recorded has a receipt — used by the API to confirm nothing is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub ended_at: DateTime<Utc>,
    pub segment_counts_by_track: BTreeMap<TrackKind, u64>,
    pub total_duration_ms: u64,
}

/// design.md §13.3's retry rules, encoded as distinct variants instead of a raw status
/// code so `upload-client`'s retry loop can match on them directly rather than
/// re-deriving the classification (timeout/5xx/429 retryable as-is; 401 retryable only
/// once and only after a token refresh; 400-series is permanent).
#[derive(Debug, Clone, thiserror::Error)]
pub enum UploadError {
    #[error("request timed out")]
    Timeout,
    #[error("server error: HTTP {status}")]
    ServerError { status: u16 },
    #[error("rate limited")]
    RateLimited,
    #[error("authentication expired")]
    AuthExpired,
    #[error("permanent client error: HTTP {status}: {reason}")]
    PermanentClientError { status: u16, reason: String },
    #[error("transport error: {0}")]
    Transport(String),
}

impl UploadError {
    /// design.md §13.3: retryable as-is (timeout, 5xx, 429), with exponential backoff
    /// + jitter left to the caller.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            UploadError::Timeout
                | UploadError::ServerError { .. }
                | UploadError::RateLimited
                | UploadError::Transport(_)
        )
    }

    /// design.md §13.3: a 401 is retried exactly once, and only after the caller
    /// refreshes its token — distinct from `is_retryable` since retrying the identical
    /// request without a refresh would just fail again.
    pub fn needs_token_refresh_before_retry(&self) -> bool {
        matches!(self, UploadError::AuthExpired)
    }
}

/// design.md §13. Decouples `app-service` from any one vendor's upload API — a fixed
/// endpoint is Phase 1A's only implementation, but nothing above this trait should know
/// that.
#[async_trait]
pub trait UploadAdapter: Send + Sync {
    async fn create_session(&self, manifest: &SessionManifest) -> Result<RemoteSession, UploadError>;

    async fn upload_segment(
        &self,
        remote: &RemoteSession,
        segment: &AudioSegment,
    ) -> Result<UploadReceipt, UploadError>;

    async fn finalize_session(
        &self,
        remote: &RemoteSession,
        summary: &SessionSummary,
    ) -> Result<(), UploadError>;
}
