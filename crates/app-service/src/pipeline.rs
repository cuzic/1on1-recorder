use std::path::Path;
use std::time::Duration;

use recorder_domain::{CapturedFrame, RemoteSession, SessionManifest, SessionSummary, TrackKind, UploadAdapter, UploadState};
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
    let remote = begin_session(store, adapter, manifest).await?;

    let sample_rate = manifest.audio.sample_rate;
    let segment_duration_ms = manifest.audio.segment_duration_ms;

    let self_pcm = align_track(self_frames, sample_rate, self_frame_interval_ns, total_duration_ns);
    let remote_pcm = align_track(remote_frames, sample_rate, remote_frame_interval_ns, total_duration_ns);

    commit_and_upload_track(manifest, TrackKind::SelfMic, &self_pcm, sample_rate, segment_duration_ms, session_dir, bitrate_bps, store, adapter, &remote).await?;
    commit_and_upload_track(manifest, TrackKind::RemoteAudio, &remote_pcm, sample_rate, segment_duration_ms, session_dir, bitrate_bps, store, adapter, &remote).await?;

    end_session(
        store,
        adapter,
        &remote,
        manifest.session_id,
        total_duration_ns / 1_000_000,
        FINALIZE_UPLOAD_RETRY_INTERVAL,
        FINALIZE_MAX_UPLOAD_PASSES,
    )
    .await
}

/// Commits every segment via `segment-store`, then attempts to upload it
/// immediately (design.md §13.4: upload continuously during recording, not
/// batched at the end). An upload failure here only marks that segment
/// `Failed` in `session-store` and moves on to the next segment — it does not
/// abort the pipeline. `run_pipeline`'s `end_session` call drains anything still
/// outstanding (never attempted, or left `Failed { retryable: true }`) via
/// `upload_worker::run_until_drained` before finalizing.
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
    remote: &RemoteSession,
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
