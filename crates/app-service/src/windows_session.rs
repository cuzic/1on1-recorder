//! Task #10's top-level wiring: "plug the Windows supervisor into stage 1's
//! pipeline" (Codex's phrasing for stage 2), rather than building a second,
//! Windows-specific pipeline. `run_windows_capture_session` collects real
//! `capture-windows` frames via `WindowsSupervisor`/`windows_frame_collector` and
//! then calls the exact same `run_pipeline` stage 1 validated with
//! `pseudo_source`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use capture_windows::device_watch::DeviceWatch;
use recorder_domain::{SessionManifest, SessionSummary, UploadAdapter};
use session_store::SessionStore;

use crate::error::AppServiceError;
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
) -> Result<SessionSummary, AppServiceError> {
    let collected = tokio::task::spawn_blocking(move || run_capture_blocking(callback_timeout_ms, shutdown_rx, level_sink))
        .await
        .expect("capture supervisor thread panicked")
        .map_err(|e| AppServiceError::Capture(e.to_string()))?;

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
fn run_capture_blocking(callback_timeout_ms: u32, shutdown_rx: crossbeam_channel::Receiver<()>, level_sink: Option<Arc<Mutex<LevelSnapshot>>>) -> Result<CollectedFrames, capture_windows::CaptureError> {
    let mut supervisor = WindowsSupervisor::new(callback_timeout_ms);
    let (frame_tx, frame_rx) = crossbeam_channel::unbounded();
    supervisor.set_frame_sink(frame_tx);

    let (watch_tx, watch_rx) = crossbeam_channel::unbounded();
    let _device_watch = DeviceWatch::start(watch_tx)?;

    supervisor.seed_default_routes()?;
    supervisor.start_all()?;

    let collector = std::thread::spawn(move || collect_frames(&frame_rx, level_sink.as_deref()));

    supervisor.run_until_shutdown(&watch_rx, &shutdown_rx)?;
    drop(supervisor); // drops frame_tx, letting `collector` finish once drained

    Ok(collector.join().expect("frame collector thread panicked"))
}
