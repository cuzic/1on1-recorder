#[cfg(windows)]
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use recorder_domain::{AudioManifest, CaptureManifest, ConsentManifest, RemoteSourceKind, SessionId, SessionManifest, SessionSummary, TrackKind};

use crate::app_state::{ActiveRecording, AppState};

/// Placeholder until Phase 1A gets real device-selection UI (design.md §14.1's
/// "マイク選択"/"会議音声ソース選択" — not in task #8's stated scope list for this
/// pass) — both tracks follow whatever WASAPI resolves as the current default
/// device, matching `capture-windows`'s own `DeviceRole::Console` default.
const DEFAULT_DEVICE_ID: &str = "default";

fn build_manifest() -> SessionManifest {
    let now = Utc::now();
    SessionManifest {
        schema_version: 1,
        session_id: SessionId::new(),
        started_at: now,
        ended_at: None,
        platform: std::env::consts::OS.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        capture: CaptureManifest {
            microphone_device_id: DEFAULT_DEVICE_ID.to_string(),
            remote_source_id: DEFAULT_DEVICE_ID.to_string(),
            remote_source_kind: RemoteSourceKind::EndpointLoopback,
        },
        audio: AudioManifest { sample_rate: 48_000, segment_duration_ms: 30_000, tracks: vec![TrackKind::SelfMic, TrackKind::RemoteAudio] },
        consent: ConsentManifest { confirmed_by_user: true, confirmed_at: now },
    }
}

#[cfg(windows)]
pub fn start(state: &AppState) -> Result<SessionId, String> {
    let manifest = build_manifest();
    let session_id = manifest.session_id;

    let level = std::sync::Arc::new(std::sync::Mutex::new(app_service::LevelSnapshot::default()));
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::unbounded();

    let store = state.store.clone();
    let adapter = state.adapter.clone();
    let session_dir = state.config.sessions_root.join(session_id.to_string());
    let bitrate_bps = state.config.bitrate_bps;
    let manifest_for_task = manifest.clone();
    let level_for_task = level.clone();
    // `state.credential_store` also backs the settings screen (`settings.rs`) —
    // `run_windows_capture_session`'s `live_transcription` wiring reads the
    // Deepgram key from it, if any was ever saved there. No key configured just
    // means no live transcription (see that module's doc comment), not a failure.
    let credential_store_for_task: Arc<dyn credential_store::CredentialStore + Send + Sync> = state.credential_store.clone();

    // WASAPI's callback timeout — how long the capture loop waits for a device
    // callback before treating it as a stall (`capture_windows::capture_loop`).
    // Not yet tuned against real hardware; see this crate's README.
    const CALLBACK_TIMEOUT_MS: u32 = 500;

    let join_handle = tokio::spawn(async move {
        app_service::run_windows_capture_session(
            &manifest_for_task,
            shutdown_rx,
            CALLBACK_TIMEOUT_MS,
            &session_dir,
            bitrate_bps,
            &store,
            adapter.as_ref(),
            Some(level_for_task),
            Some(credential_store_for_task),
        )
        .await
    });

    *state.current.lock().unwrap() = Some(ActiveRecording { session_id, manifest, started_at: Instant::now(), level, shutdown_tx, join_handle });
    Ok(session_id)
}

#[cfg(windows)]
pub async fn stop(state: &AppState) -> Result<SessionSummary, String> {
    let active = state.current.lock().unwrap().take().ok_or_else(|| "not recording".to_string())?;
    active.shutdown_tx.send(()).map_err(|e| format!("failed to signal shutdown: {e}"))?;
    active.join_handle.await.map_err(|e| format!("capture task panicked: {e}"))?.map_err(|e| e.to_string())
}

/// The macOS analogue of the `#[cfg(windows)]` `start` above — real
/// ScreenCaptureKit capture via `app_service::run_macos_capture_session`. **Not
/// yet run on real macOS hardware, or even compiled at all** — see
/// `crates/capture-macos`'s crate doc comment and README.
#[cfg(target_os = "macos")]
pub fn start(state: &AppState) -> Result<SessionId, String> {
    let manifest = build_manifest();
    let session_id = manifest.session_id;

    let level = std::sync::Arc::new(std::sync::Mutex::new(
        app_service::macos_frame_collector::LevelSnapshot::default(),
    ));
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::unbounded();

    let store = state.store.clone();
    let adapter = state.adapter.clone();
    let session_dir = state.config.sessions_root.join(session_id.to_string());
    let bitrate_bps = state.config.bitrate_bps;
    let manifest_for_task = manifest.clone();
    let level_for_task = level.clone();
    let sample_rate_hz = manifest.audio.sample_rate;

    // SCStreamConfiguration takes channel count as an explicit request (unlike
    // WASAPI, which reports the device's own mix format) — mono, matching
    // Windows Phase 1A's own capture format.
    const CHANNELS: u16 = 1;

    let join_handle = tokio::spawn(async move {
        app_service::run_macos_capture_session(
            &manifest_for_task,
            shutdown_rx,
            sample_rate_hz,
            CHANNELS,
            &session_dir,
            bitrate_bps,
            &store,
            adapter.as_ref(),
            Some(level_for_task),
        )
        .await
    });

    *state.current.lock().unwrap() = Some(ActiveRecording {
        session_id,
        manifest,
        started_at: Instant::now(),
        level,
        shutdown_tx,
        join_handle,
    });
    Ok(session_id)
}

#[cfg(target_os = "macos")]
pub async fn stop(state: &AppState) -> Result<SessionSummary, String> {
    let active = state.current.lock().unwrap().take().ok_or_else(|| "not recording".to_string())?;
    active.shutdown_tx.send(()).map_err(|e| format!("failed to signal shutdown: {e}"))?;
    active.join_handle.await.map_err(|e| format!("capture task panicked: {e}"))?.map_err(|e| e.to_string())
}

/// No real capture backend exists on this platform (neither `capture-windows` nor
/// `capture-macos` target it) — "recording" here is a stopwatch only. On stop,
/// exactly enough `pseudo_source` audio for the elapsed real time is generated
/// and run through the same `run_pipeline` a real session uses, so the rest of
/// the app (session-store, segment-store, upload-client) is genuinely exercised
/// locally. This is a development/testing fallback, not a claim that anything was
/// actually recorded from a real microphone — see this crate's README.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn start(state: &AppState) -> Result<SessionId, String> {
    let manifest = build_manifest();
    let session_id = manifest.session_id;
    *state.current.lock().unwrap() = Some(ActiveRecording { session_id, manifest, started_at: Instant::now() });
    Ok(session_id)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub async fn stop(state: &AppState) -> Result<SessionSummary, String> {
    use app_service::pseudo_source::{generate_frames, nominal_frame_interval_ns, PseudoSourceConfig};

    let active = state.current.lock().unwrap().take().ok_or_else(|| "not recording".to_string())?;
    let duration_secs = active.started_at.elapsed().as_secs().max(1) as u32;
    let session_dir = state.config.sessions_root.join(active.session_id.to_string());

    let config = PseudoSourceConfig { duration_secs, frame_interval_ms: 20, sample_rate: 48_000, channels: 1, tone_freq_hz: 440.0 };
    let self_frames = generate_frames(TrackKind::SelfMic, &config);
    let remote_frames = generate_frames(TrackKind::RemoteAudio, &config);
    let total_duration_ns = duration_secs as u64 * 1_000_000_000;
    let interval_ns = nominal_frame_interval_ns(&config);

    app_service::run_pipeline(&active.manifest, &self_frames, &remote_frames, interval_ns, interval_ns, total_duration_ns, &session_dir, state.config.bitrate_bps, &state.store, state.adapter.as_ref())
        .await
        .map_err(|e| e.to_string())
}
