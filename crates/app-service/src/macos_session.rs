//! Top-level wiring: "plug the macOS supervisor into stage 1's pipeline", mirroring
//! `windows_session.rs` exactly — `run_macos_capture_session` collects real
//! `capture-macos` frames via `MacosSupervisor`/`macos_frame_collector` and then
//! calls the same `run_pipeline` both `pseudo_source` and the Windows path use.

use std::path::Path;
use std::sync::{Arc, Mutex};

use capture_api::rebinding::EndpointId;
use capture_macos::device_watch::DeviceWatch;
use recorder_domain::{SessionManifest, SessionSummary, UploadAdapter};
use session_store::SessionStore;

use crate::error::AppServiceError;
use crate::macos_frame_collector::{collect_frames, CollectedFrames, LevelSnapshot};
use crate::macos_supervisor::MacosSupervisor;
use crate::pipeline::run_pipeline;

/// Runs one real macOS capture session end to end and blocks the calling thread for
/// the whole session's duration — same contract as `run_windows_capture_session`,
/// including its in-memory-buffering scope limitation (see that function's doc
/// comment; identical here). Unlike Windows (where the device's own mix format is
/// queried via `GetMixFormat`), `sample_rate_hz`/`channels` must be supplied by the
/// caller since `SCStreamConfiguration` takes them as an explicit request rather
/// than reporting a queried default.
///
/// `mic_device_id`/`render_device_id` — see `run_windows_capture_session`'s doc
/// comment on the same two parameters; identical contract here.
///
/// **Not yet run on real macOS hardware or verified against a real build at all**
/// — see `capture-macos`'s crate doc comment and this crate's README.
#[allow(clippy::too_many_arguments)]
pub async fn run_macos_capture_session(
    manifest: &SessionManifest,
    shutdown_rx: crossbeam_channel::Receiver<()>,
    sample_rate_hz: u32,
    channels: u16,
    session_dir: &Path,
    bitrate_bps: i32,
    store: &SessionStore,
    adapter: &dyn UploadAdapter,
    level_sink: Option<Arc<Mutex<LevelSnapshot>>>,
    mic_device_id: Option<String>,
    render_device_id: Option<String>,
) -> Result<SessionSummary, AppServiceError> {
    let collected = tokio::task::spawn_blocking(move || {
        run_capture_blocking(sample_rate_hz, channels, shutdown_rx, level_sink, mic_device_id, render_device_id)
    })
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

fn latest_frame_end_ns(
    collected: &CollectedFrames,
    self_interval: u64,
    remote_interval: u64,
) -> u64 {
    let self_end = collected
        .self_frames
        .last()
        .map(|f| f.host_time_ns + self_interval)
        .unwrap_or(0);
    let remote_end = collected
        .remote_frames
        .last()
        .map(|f| f.host_time_ns + remote_interval)
        .unwrap_or(0);
    self_end.max(remote_end)
}

/// Everything that has to happen on one dedicated OS thread — same rationale as
/// `windows_session::run_capture_blocking`'s doc comment (CoreAudio's
/// `AudioObjectAddPropertyListenerBlock`-registering thread and
/// `MacosSupervisor::run_until_shutdown`'s blocking loop are kept on the same
/// thread for the same reason `DeviceWatch`/`WindowsSupervisor` are on Windows).
fn run_capture_blocking(
    sample_rate_hz: u32,
    channels: u16,
    shutdown_rx: crossbeam_channel::Receiver<()>,
    level_sink: Option<Arc<Mutex<LevelSnapshot>>>,
    mic_device_id: Option<String>,
    render_device_id: Option<String>,
) -> Result<CollectedFrames, capture_macos::CaptureError> {
    let mut supervisor = MacosSupervisor::new(sample_rate_hz, channels);
    let (frame_tx, frame_rx) = crossbeam_channel::unbounded();
    supervisor.set_frame_sink(frame_tx);

    let (watch_tx, watch_rx) = crossbeam_channel::unbounded();
    let _device_watch = DeviceWatch::start(watch_tx)?;

    // design.md §16.5, identical policy to Windows: pin to whatever's currently in
    // use for the rest of the session, never FollowDefault. `mic_device_id`/
    // `render_device_id` — see `windows_session::run_capture_blocking`'s identical
    // comment on skipping `resolve_current_defaults` per-track when the caller
    // already has an explicit choice.
    let needs_defaults = mic_device_id.is_none() || render_device_id.is_none();
    let defaults = if needs_defaults { Some(supervisor.resolve_current_defaults()?) } else { None };
    let mic_endpoint_id = match mic_device_id {
        Some(id) => EndpointId(id),
        None => defaults.as_ref().expect("resolved when mic_device_id is None").0.clone(),
    };
    let render_endpoint_id = match render_device_id {
        Some(id) => EndpointId(id),
        None => defaults.as_ref().expect("resolved when render_device_id is None").1.clone(),
    };
    supervisor.pin_devices(mic_endpoint_id, render_endpoint_id);
    supervisor.start_all()?;

    let collector = std::thread::spawn(move || collect_frames(&frame_rx, level_sink.as_deref()));

    supervisor.run_until_shutdown(&watch_rx, &shutdown_rx)?;
    drop(supervisor); // drops frame_tx, letting `collector` finish once drained

    Ok(collector.join().expect("frame collector thread panicked"))
}
