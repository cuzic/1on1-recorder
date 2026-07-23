//! Task #11: the recording-session lifecycle (design.md §10's `CaptureState`
//! state diagram) and force-quit crash recovery, tying `upload_worker` and
//! `session-store`/`segment-store` together into explicit start/stop/finalize
//! use-cases. OS-independent, like `upload_worker` — used identically whether
//! segments came from `pseudo_source` (stage 1) or a real Windows capture session
//! (stage 2).

use std::path::{Path, PathBuf};
use std::time::Duration;

use recorder_domain::{CaptureState, RemoteSession, SessionId, SessionManifest, SessionSummary, TrackKind, UploadAdapter};
use session_store::SessionStore;

use crate::error::AppServiceError;
use crate::upload_worker::run_until_drained;

/// design.md §21's Phase 1A tracks — hardcoded here (rather than read from a
/// manifest that might not exist yet, e.g. during recovery) since Phase 1A always
/// records exactly these two.
const PHASE_1A_TRACKS: [TrackKind; 2] = [TrackKind::SelfMic, TrackKind::RemoteAudio];

/// design.md §10: `Idle -> Preparing -> Recording`. Registers the session
/// locally, and attempts to register it with the remote API too — but a
/// credential/network failure on that second half is **not** fatal to local
/// capture (unlike a `store.create_session` failure, which is a real local bug
/// and still propagates): local recording has no dependency on the remote API
/// being reachable, matching the resilience `commit_and_upload_track` already
/// gives per-segment upload failures. Returns `None` in that case — the caller
/// still proceeds to capture and commit segments locally, and `run_pipeline`
/// gets one more chance to register a remote session before finalizing.
pub async fn begin_session(store: &SessionStore, adapter: &dyn UploadAdapter, manifest: &SessionManifest) -> Result<Option<RemoteSession>, AppServiceError> {
    store.create_session(manifest)?; // starts in CaptureState::Preparing
    match adapter.create_session(manifest).await {
        Ok(remote) => {
            store.set_remote_session_id(manifest.session_id, &remote.remote_session_id)?;
            store.update_capture_state(manifest.session_id, &CaptureState::Recording)?;
            Ok(Some(remote))
        }
        Err(err) => {
            tracing::warn!(%err, "remote session registration failed; proceeding with local-only capture");
            store.update_capture_state(manifest.session_id, &CaptureState::Recording)?;
            Ok(None)
        }
    }
}

/// design.md §10: `Recording -> Stopping -> Finalizing -> Finalized`. Call once
/// capture has fully stopped and every segment is committed to `segment-store`
/// (i.e. after `pipeline::run_pipeline`'s track loops return). Drains any
/// still-pending uploads before finalizing with the API — a capture failure or a
/// slow network must not block finalization forever, hence `max_upload_passes` as
/// a backstop (see `upload_worker::run_until_drained`).
pub async fn end_session(
    store: &SessionStore,
    adapter: &dyn UploadAdapter,
    remote: &RemoteSession,
    session_id: SessionId,
    total_duration_ms: u64,
    upload_retry_interval: Duration,
    max_upload_passes: u32,
) -> Result<SessionSummary, AppServiceError> {
    store.update_capture_state(session_id, &CaptureState::Stopping)?;
    store.update_capture_state(session_id, &CaptureState::Finalizing)?;
    finalize_and_upload(store, adapter, remote, session_id, total_duration_ms, upload_retry_interval, max_upload_passes).await
}

/// Startup crash recovery (this task's own description: wire `segment-store`'s
/// restart scan in at startup). Call once, before starting any *new* recording:
///
/// 1. `session-store`'s own `reconcile_on_startup` finds sessions a previous
///    process instance left mid-flight and marks them `Failed { recoverable: true }`
///    (design.md §10's diagram: `Failed -> Finalizing`, to persist whatever data
///    is available rather than discard it).
/// 2. Also collects any session already `Failed` with no `remote_session_id` —
///    whether from a crash reconciled just now, or from a *clean* end-of-session
///    that never managed to register with the remote API at all (see
///    `pipeline::finalize_local_only_session`, which leaves sessions in exactly
///    this state so they're retried here rather than silently dropped).
/// 3. For each, `segment_store::scan_and_recover` re-scans its segment
///    directories — discarding orphaned `.partial` files, registering any
///    complete-but-unregistered `.opus` file a crash landed between rename and
///    DB registration (a no-op for a cleanly-ended local-only session, since
///    nothing was left mid-write).
/// 4. If it still has no `remote_session_id`, reconstructs its original
///    `SessionManifest` via `SessionStore::session_manifest` and retries
///    `UploadAdapter::create_session` — on success, proceeds to finalize exactly
///    like a session that already had one; on failure, it's left `Failed` (i.e.
///    still matched by step 2 on the *next* restart, so this keeps retrying
///    indefinitely rather than being a one-shot attempt).
#[allow(clippy::too_many_arguments)]
pub async fn recover_incomplete_sessions(
    store: &SessionStore,
    adapter: &dyn UploadAdapter,
    sessions_root: &Path,
    nominal_segment_duration_ms: u64,
    sample_rate: u32,
    channels: u16,
    upload_retry_interval: Duration,
    max_upload_passes: u32,
) -> Result<Vec<SessionId>, AppServiceError> {
    let mut session_ids = store.reconcile_on_startup()?;
    for session_id in store.failed_sessions_missing_remote_registration()? {
        if !session_ids.contains(&session_id) {
            session_ids.push(session_id);
        }
    }
    let mut finalized = Vec::new();

    for session_id in session_ids {
        let session_dir = session_dir_for(sessions_root, session_id);
        for track in PHASE_1A_TRACKS {
            // Best-effort: a directory that was never created (e.g. that track
            // never captured anything before the crash) isn't an error.
            let _ = segment_store::scan_and_recover(&session_dir, session_id, track, nominal_segment_duration_ms, sample_rate, channels, store);
        }

        let remote_session_id = match store.remote_session_id(session_id)? {
            Some(id) => id,
            None => match try_register_remote_session(store, adapter, session_id).await? {
                Some(id) => id,
                None => continue, // still unreachable — retried again on the next restart
            },
        };
        let remote = RemoteSession { session_id, remote_session_id };
        let total_duration_ms = estimate_total_duration_ms(store, session_id, nominal_segment_duration_ms)?;

        finalize_and_upload(store, adapter, &remote, session_id, total_duration_ms, upload_retry_interval, max_upload_passes).await?;
        finalized.push(session_id);
    }

    Ok(finalized)
}

/// Reconstructs `session_id`'s manifest and makes one attempt at
/// `UploadAdapter::create_session` — the recovery-time counterpart of
/// `pipeline::try_register_remote_session`. Returns `None` (not an error) for
/// either an unreachable/unauthenticated remote API or a missing manifest (the
/// latter should be unreachable in practice, since every session row is written
/// by `store.create_session` with every field this reconstructs).
async fn try_register_remote_session(store: &SessionStore, adapter: &dyn UploadAdapter, session_id: SessionId) -> Result<Option<String>, AppServiceError> {
    let Some(manifest) = store.session_manifest(session_id)? else {
        return Ok(None);
    };
    match adapter.create_session(&manifest).await {
        Ok(remote) => {
            store.set_remote_session_id(session_id, &remote.remote_session_id)?;
            Ok(Some(remote.remote_session_id))
        }
        Err(err) => {
            tracing::warn!(%err, "remote session registration still failing during recovery; will retry on next restart");
            Ok(None)
        }
    }
}

/// Shared tail of `end_session` and `recover_incomplete_sessions`: drain pending
/// uploads, then finalize with the API and mark `Finalized` locally.
async fn finalize_and_upload(
    store: &SessionStore,
    adapter: &dyn UploadAdapter,
    remote: &RemoteSession,
    session_id: SessionId,
    total_duration_ms: u64,
    upload_retry_interval: Duration,
    max_upload_passes: u32,
) -> Result<SessionSummary, AppServiceError> {
    run_until_drained(store, adapter, remote, session_id, upload_retry_interval, max_upload_passes).await?;

    let summary = SessionSummary {
        session_id,
        ended_at: chrono::Utc::now(),
        segment_counts_by_track: store.segment_counts_by_track(session_id)?,
        total_duration_ms,
    };
    adapter.finalize_session(remote, &summary).await?;
    store.update_capture_state(session_id, &CaptureState::Finalized)?;
    Ok(summary)
}

/// Derives a session's total duration from the ledger alone (no in-memory capture
/// bookkeeping survives a crash) — the latest `timeline_start_ms + duration_ms`
/// across every committed segment in either track.
fn estimate_total_duration_ms(store: &SessionStore, session_id: SessionId, fallback_segment_duration_ms: u64) -> Result<u64, AppServiceError> {
    let mut latest_ms = 0u64;
    for track in PHASE_1A_TRACKS {
        if let Some(last) = store.segments_for_track(session_id, track)?.last() {
            latest_ms = latest_ms.max(last.timeline_start_ms + last.duration_ms as u64);
        }
    }
    Ok(if latest_ms == 0 { fallback_segment_duration_ms } else { latest_ms })
}

fn session_dir_for(sessions_root: &Path, session_id: SessionId) -> PathBuf {
    sessions_root.join(session_id.to_string())
}
