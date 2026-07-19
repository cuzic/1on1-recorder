//! Task #10's top-level wiring: "plug the Windows supervisor into stage 1's
//! pipeline" (Codex's phrasing for stage 2), rather than building a second,
//! Windows-specific pipeline. `run_windows_capture_session` collects real
//! `capture-windows` frames via `WindowsSupervisor`/`windows_frame_collector` and
//! then calls the exact same `run_pipeline` stage 1 validated with
//! `pseudo_source`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use capture_windows::device_watch::DeviceWatch;
use recorder_domain::{SessionManifest, SessionSummary, TrackKind, UploadAdapter};
use session_store::SessionStore;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::AppServiceError;
use crate::live_transcription::{run_live_transcription, TranscriptionStatus};
use crate::pipeline::run_pipeline;
use crate::windows_frame_collector::{collect_frames, CollectedFrames, LevelSnapshot};
use crate::windows_supervisor::WindowsSupervisor;

/// Runs one real Windows capture session end to end and blocks the calling thread
/// for the whole session's duration: starts `WindowsSupervisor`, waits for
/// `shutdown_rx` (the caller is expected to wire this to Ctrl+C or a UI "stop"
/// action), then feeds every collected frame through `run_pipeline`.
///
/// Buffers the entire session's audio in memory (see
/// `windows_frame_collector::collect_frames`'s doc comment) — fine for validating
/// that real capture flows through stage 1's pipeline, not how a long recording
/// should work in production; incremental segmenting belongs to stage 3 (task
/// #11). Like the rest of `windows_supervisor`, this has only been
/// cross-compile-checked, never run on real Windows hardware — see this crate's
/// README.
///
/// `credential_store`, if given, is where `live_transcription` looks up a
/// Deepgram API key (see that module) to stream real-time transcription
/// alongside the batch capture this function already does — `None`, or no key
/// found under it, just means no live transcription for this session (recording
/// itself is unaffected either way). Wired up unconditionally (not only under the
/// `live-transcription` feature) so this function's signature doesn't need its
/// own `#[cfg]`; without that feature, `live_transcription::run_live_transcription`
/// is a stub that ignores it (and reports `Unavailable` on `transcription_status_sink`
/// — see that module).
///
/// `transcription_status_sink`, if given, is `live_transcription`'s #52 side
/// channel — kept up to date with each track's Deepgram connection state so the
/// desktop UI can tell "STT not connected" apart from "nobody has spoken yet".
///
/// `silence_gate_enabled` is passed straight through to `run_live_transcription`
/// (see that function's parameter doc comment) — this function doesn't interpret
/// it itself, it just plumbs the caller's (`apps/desktop`'s) resolved
/// `AppSettings::silence_gate_enabled` value down to where the gate actually
/// lives.
#[allow(clippy::too_many_arguments)]
pub async fn run_windows_capture_session(
    manifest: &SessionManifest,
    shutdown_rx: crossbeam_channel::Receiver<()>,
    callback_timeout_ms: u32,
    session_dir: &Path,
    bitrate_bps: i32,
    store: &SessionStore,
    adapter: &dyn UploadAdapter,
    level_sink: Option<Arc<Mutex<LevelSnapshot>>>,
    credential_store: Option<Arc<dyn credential_store::CredentialStore + Send + Sync>>,
    transcription_status_sink: Option<Arc<Mutex<TranscriptionStatus>>>,
    silence_gate_enabled: bool,
) -> Result<SessionSummary, AppServiceError> {
    // Same shape as `level_sink`'s side channel (see `windows_frame_collector`'s doc
    // comment), but for raw PCM instead of RMS/peak. `stt_tx` is moved into
    // `run_capture_blocking`'s closure below and dropped there once capture and the
    // collector thread are fully done, which is what lets `run_live_transcription`'s
    // `audio_rx.recv()` loop end (and finalize both STT sessions) at the right time
    // without a separate shutdown signal.
    let (stt_tx, stt_rx) = tokio::sync::mpsc::unbounded_channel();
    let live_transcription_fut = run_live_transcription(
        manifest.session_id,
        manifest.audio.sample_rate,
        credential_store,
        stt_rx,
        store,
        transcription_status_sink,
        silence_gate_enabled,
    );

    let capture_fut = tokio::task::spawn_blocking(move || run_capture_blocking(callback_timeout_ms, shutdown_rx, level_sink, stt_tx));

    let (collected, ()) = tokio::join!(capture_fut, live_transcription_fut);
    let collected = collected.expect("capture supervisor thread panicked").map_err(|e| AppServiceError::Capture(e.to_string()))?;

    let self_interval = collected.self_nominal_frame_interval_ns.max(1);
    let remote_interval = collected.remote_nominal_frame_interval_ns.max(1);
    let total_duration_ns = latest_frame_end_ns(&collected, self_interval, remote_interval);

    run_pipeline(
        manifest,
        &collected.self_frames,
        &collected.remote_frames,
        self_interval,
        remote_interval,
        total_duration_ns,
        session_dir,
        bitrate_bps,
        store,
        adapter,
    )
    .await
}

fn latest_frame_end_ns(collected: &CollectedFrames, self_interval: u64, remote_interval: u64) -> u64 {
    let self_end = collected.self_frames.last().map(|f| f.host_time_ns + self_interval).unwrap_or(0);
    let remote_end = collected.remote_frames.last().map(|f| f.host_time_ns + remote_interval).unwrap_or(0);
    self_end.max(remote_end)
}

/// Everything that has to happen on one dedicated OS thread: `DeviceWatch::start`
/// requires its creating thread to stay alive for as long as it's alive, and that
/// same thread is what `WindowsSupervisor::run_until_shutdown` blocks.
fn run_capture_blocking(
    callback_timeout_ms: u32,
    shutdown_rx: crossbeam_channel::Receiver<()>,
    level_sink: Option<Arc<Mutex<LevelSnapshot>>>,
    stt_tx: UnboundedSender<(TrackKind, Vec<f32>, u32)>,
) -> Result<CollectedFrames, capture_windows::CaptureError> {
    let mut supervisor = WindowsSupervisor::new(callback_timeout_ms);
    let (frame_tx, frame_rx) = crossbeam_channel::unbounded();
    supervisor.set_frame_sink(frame_tx);

    let (watch_tx, watch_rx) = crossbeam_channel::unbounded();
    let _device_watch = DeviceWatch::start(watch_tx)?;

    // design.md §16.5: use whatever's currently in use for each of Microphone
    // and EndpointLoopback, then pin to those exact devices for the rest of the
    // session — not `FollowDefault`, which would auto-rebind on every later OS
    // default-device change while running.
    let (mic_endpoint_id, render_endpoint_id) = supervisor.resolve_current_defaults()?;
    supervisor.pin_devices(mic_endpoint_id, render_endpoint_id);
    supervisor.start_all()?;

    // `stt_tx` is moved into and dropped at the end of this closure (i.e. once
    // `collect_frames` returns) — see `run_windows_capture_session`'s doc comment
    // on why that's what lets `run_live_transcription` know capture is done.
    let collector = std::thread::spawn(move || collect_frames(&frame_rx, level_sink.as_deref(), Some(&stt_tx)));

    supervisor.run_until_shutdown(&watch_rx, &shutdown_rx)?;
    drop(supervisor); // drops frame_tx, letting `collector` finish once drained

    Ok(collector.join().expect("frame collector thread panicked"))
}
