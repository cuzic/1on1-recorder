use std::path::Path;
use std::time::Duration;

use recorder_domain::{CaptureState, CapturedFrame, RemoteSession, SessionId, SessionManifest, SessionSummary, TrackKind, UploadAdapter, UploadState};
use segment_store::{commit_segment, encode_segment_to_ogg_opus, CrashPoint, SegmentRequest};
use session_store::SessionStore;

use crate::error::AppServiceError;
use crate::segmenter::segment_pcm;
use crate::session_lifecycle::{begin_session, end_session};
use crate::timeline_adapter::align_track;

/// Backstop for `end_session`'s post-recording upload-draining pass — see
/// `upload_worker::run_until_drained`'s doc comment for why this rarely binds.
const FINALIZE_UPLOAD_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const FINALIZE_MAX_UPLOAD_PASSES: u32 = 10;

/// Runs the full `capture -> align -> segment -> encode -> commit -> upload ->
/// finalize` pipeline for one session, given already-captured frames for both
/// tracks, driving `session-store`'s `CaptureState`/`UploadState` transitions
/// (design.md §10) via `session_lifecycle`. Capture-backend-agnostic: stage 1
/// (task #7) calls it with `pseudo_source`-generated frames; stage 2 (task #10)
/// calls it with frames a Windows supervisor converted from `capture-windows`'s
/// real WASAPI output.
#[allow(clippy::too_many_arguments)]
pub async fn run_pipeline(
    manifest: &SessionManifest,
    self_frames: &[CapturedFrame],
    remote_frames: &[CapturedFrame],
    self_frame_interval_ns: u64,
    remote_frame_interval_ns: u64,
    total_duration_ns: u64,
    session_dir: &Path,
    bitrate_bps: i32,
    store: &SessionStore,
    adapter: &dyn UploadAdapter,
) -> Result<SessionSummary, AppServiceError> {
    let mut remote = begin_session(store, adapter, manifest).await?;

    let sample_rate = manifest.audio.sample_rate;
    let segment_duration_ms = manifest.audio.segment_duration_ms;

    let self_pcm = align_track(self_frames, sample_rate, self_frame_interval_ns, total_duration_ns);
    let remote_pcm = align_track(remote_frames, sample_rate, remote_frame_interval_ns, total_duration_ns);

    commit_and_upload_track(manifest, TrackKind::SelfMic, &self_pcm, sample_rate, segment_duration_ms, session_dir, bitrate_bps, store, adapter, remote.as_ref()).await?;
    commit_and_upload_track(manifest, TrackKind::RemoteAudio, &remote_pcm, sample_rate, segment_duration_ms, session_dir, bitrate_bps, store, adapter, remote.as_ref()).await?;

    if remote.is_none() {
        remote = try_register_remote_session(store, adapter, manifest).await;
    }

    let total_duration_ms = total_duration_ns / 1_000_000;
    match remote {
        Some(remote) => {
            end_session(store, adapter, &remote, manifest.session_id, total_duration_ms, FINALIZE_UPLOAD_RETRY_INTERVAL, FINALIZE_MAX_UPLOAD_PASSES).await
        }
        None => finalize_local_only_session(store, manifest.session_id, total_duration_ms),
    }
}

/// One more attempt at remote-session registration right before finalizing —
/// gives a since-recovered network one more chance, in the same spirit as
/// `commit_and_upload_track`'s own per-segment retry-via-`upload_worker`
/// pattern, before this session is committed to being local-only for good.
async fn try_register_remote_session(store: &SessionStore, adapter: &dyn UploadAdapter, manifest: &SessionManifest) -> Option<RemoteSession> {
    match adapter.create_session(manifest).await {
        Ok(remote) => match store.set_remote_session_id(manifest.session_id, &remote.remote_session_id) {
            Ok(()) => Some(remote),
            Err(err) => {
                tracing::warn!(%err, "failed to persist remote_session_id after late registration");
                None
            }
        },
        Err(err) => {
            tracing::warn!(%err, "remote session registration still failing at finalize time; keeping this session local-only");
            None
        }
    }
}

/// design.md §10: no remote session was ever established (see `begin_session`'s
/// doc comment) — there is nothing to drain/finalize against the API, so this
/// session is marked `Failed { recoverable: true }` rather than `Finalized`
/// (the same tag `recover_incomplete_sessions` already uses for a
/// crash-mid-session; `Finalized` would misleadingly imply the API confirmed
/// it). Local capture still fully succeeded — every segment committed to
/// segment-store — so the returned `SessionSummary` is real, not a placeholder;
/// only the remote registration/upload/finalize steps never happened.
fn finalize_local_only_session(store: &SessionStore, session_id: SessionId, total_duration_ms: u64) -> Result<SessionSummary, AppServiceError> {
    store.update_capture_state(
        session_id,
        &CaptureState::Failed { recoverable: true, reason: "remote session was never registered; segments captured locally only".to_string() },
    )?;
    Ok(SessionSummary { session_id, ended_at: chrono::Utc::now(), segment_counts_by_track: store.segment_counts_by_track(session_id)?, total_duration_ms })
}

/// Commits every segment via `segment-store`, then attempts to upload it
/// immediately (design.md §13.4: upload continuously during recording, not
/// batched at the end). An upload failure here only marks that segment
/// `Failed` in `session-store` and moves on to the next segment — it does not
/// abort the pipeline. `run_pipeline`'s `end_session` call drains anything still
/// outstanding (never attempted, or left `Failed { retryable: true }`) via
/// `upload_worker::run_until_drained` before finalizing.
///
/// `remote` is `None` when `begin_session` couldn't register a remote session
/// (see its doc comment) — in that case every segment is still committed to
/// segment-store as normal, but the upload attempt itself is skipped (there is
/// no `RemoteSession` to attempt it against yet); the segment is left in its
/// default `UploadState::NotStarted`, which `pending_uploads` already treats as
/// pending, so it's picked up once a remote session exists.
#[allow(clippy::too_many_arguments)]
async fn commit_and_upload_track(
    manifest: &SessionManifest,
    track: TrackKind,
    pcm: &[f32],
    sample_rate: u32,
    segment_duration_ms: u32,
    session_dir: &Path,
    bitrate_bps: i32,
    store: &SessionStore,
    adapter: &dyn UploadAdapter,
    remote: Option<&RemoteSession>,
) -> Result<(), AppServiceError> {
    for pending in segment_pcm(pcm, sample_rate, segment_duration_ms) {
        let encoded = encode_segment_to_ogg_opus(pending.pcm, bitrate_bps)?;
        let request = SegmentRequest {
            session_id: manifest.session_id,
            track,
            sequence: pending.sequence,
            timeline_start_ms: pending.timeline_start_ms,
            sample_rate,
            channels: 1,
        };
        let segment = commit_segment(&encoded, session_dir, &request, store, CrashPoint::None)?.expect("CrashPoint::None always commits");

        let Some(remote) = remote else { continue };
        store.update_upload_state(manifest.session_id, track, pending.sequence, &UploadState::Uploading)?;
        match adapter.upload_segment(remote, &segment).await {
            Ok(_receipt) => {
                store.update_upload_state(manifest.session_id, track, pending.sequence, &UploadState::Completed)?;
            }
            Err(e) => {
                let retryable = e.is_retryable() || e.needs_token_refresh_before_retry();
                store.update_upload_state(manifest.session_id, track, pending.sequence, &UploadState::Failed { retryable, reason: e.to_string() })?;
            }
        }
    }
    Ok(())
}
