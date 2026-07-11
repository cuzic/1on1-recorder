use std::path::Path;

use chrono::Utc;
use recorder_domain::{CapturedFrame, RemoteSession, SessionManifest, SessionSummary, TrackKind, UploadAdapter, UploadState};
use segment_store::{commit_segment, encode_segment_to_ogg_opus, CrashPoint, SegmentRequest};
use session_store::SessionStore;

use crate::error::AppServiceError;
use crate::segmenter::segment_pcm;
use crate::timeline_adapter::align_track;

/// Runs the full `capture -> align -> segment -> encode -> commit -> upload ->
/// finalize` pipeline for one session, given already-captured frames for both
/// tracks. This is capture-backend-agnostic: stage 1 (task #7) calls it with
/// `pseudo_source`-generated frames; stage 2 (task #10) will call it with frames a
/// Windows supervisor converted from `capture-windows`'s real WASAPI output.
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
    store.create_session(manifest)?;
    let remote = adapter.create_session(manifest).await?;
    store.set_remote_session_id(manifest.session_id, &remote.remote_session_id)?;

    let sample_rate = manifest.audio.sample_rate;
    let segment_duration_ms = manifest.audio.segment_duration_ms;

    let self_pcm = align_track(self_frames, sample_rate, self_frame_interval_ns, total_duration_ns);
    let remote_pcm = align_track(remote_frames, sample_rate, remote_frame_interval_ns, total_duration_ns);

    commit_and_upload_track(manifest, TrackKind::SelfMic, &self_pcm, sample_rate, segment_duration_ms, session_dir, bitrate_bps, store, adapter, &remote).await?;
    commit_and_upload_track(manifest, TrackKind::RemoteAudio, &remote_pcm, sample_rate, segment_duration_ms, session_dir, bitrate_bps, store, adapter, &remote).await?;

    let summary = SessionSummary {
        session_id: manifest.session_id,
        ended_at: Utc::now(),
        segment_counts_by_track: store.segment_counts_by_track(manifest.session_id)?,
        total_duration_ms: total_duration_ns / 1_000_000,
    };
    adapter.finalize_session(&remote, &summary).await?;

    Ok(summary)
}

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

        adapter.upload_segment(remote, &segment).await?;
        store.update_upload_state(manifest.session_id, track, pending.sequence, &UploadState::Completed)?;
    }
    Ok(())
}
