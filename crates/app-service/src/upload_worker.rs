//! Task #11: a standing upload pass, independent of capture — segments keep
//! uploading even after capture stops (finalizing), and capture can keep
//! committing new segments while old ones are still uploading/retrying. OS
//! independent (no `windows-supervisor` feature needed): this is the same code
//! whether segments came from `pseudo_source` or a real Windows capture session.

use std::time::Duration;

use recorder_domain::{RemoteSession, SessionId, UploadAdapter, UploadState};
use session_store::StoreError;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UploadPassSummary {
    pub uploaded: u32,
    pub failed: u32,
}

/// One pass over whatever `session-store` currently considers pending for
/// `session_id` — each segment is attempted exactly once per call.
/// `upload-client`'s own retry/backoff already covers transient failures within
/// one `upload_segment` call; this level of "retry" is for a segment that still
/// failed after exhausting that (or a permanent error), and is what
/// `run_until_drained`'s pass loop is for.
pub async fn upload_pending_once(store: &session_store::SessionStore, adapter: &dyn UploadAdapter, remote: &RemoteSession, session_id: SessionId) -> Result<UploadPassSummary, StoreError> {
    let pending = store.pending_uploads(session_id)?;
    let mut summary = UploadPassSummary::default();

    for segment in pending {
        store.update_upload_state(session_id, segment.track, segment.sequence, &UploadState::Uploading)?;
        match adapter.upload_segment(remote, &segment).await {
            Ok(_receipt) => {
                store.update_upload_state(session_id, segment.track, segment.sequence, &UploadState::Completed)?;
                summary.uploaded += 1;
            }
            Err(e) => {
                let retryable = e.is_retryable() || e.needs_token_refresh_before_retry();
                store.update_upload_state(session_id, segment.track, segment.sequence, &UploadState::Failed { retryable, reason: e.to_string() })?;
                summary.failed += 1;
            }
        }
    }

    Ok(summary)
}

/// Repeatedly calls `upload_pending_once` until `session-store` has nothing left
/// pending for `session_id`, or `max_passes` is exhausted (a permanently-failed
/// segment — `retryable: false` — never becomes pending again, so this always
/// terminates as long as every failure is eventually classified one way or the
/// other; `max_passes` is a backstop against a bug in that classification, not
/// something expected to bind in normal operation).
pub async fn run_until_drained(
    store: &session_store::SessionStore,
    adapter: &dyn UploadAdapter,
    remote: &RemoteSession,
    session_id: SessionId,
    retry_interval: Duration,
    max_passes: u32,
) -> Result<UploadPassSummary, StoreError> {
    let mut total = UploadPassSummary::default();
    for _ in 0..max_passes {
        let pass = upload_pending_once(store, adapter, remote, session_id).await?;
        total.uploaded += pass.uploaded;
        total.failed += pass.failed;
        if store.pending_uploads(session_id)?.is_empty() {
            break;
        }
        tokio::time::sleep(retry_interval).await;
    }
    Ok(total)
}
