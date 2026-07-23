#[cfg(windows)]
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use recorder_domain::{AudioManifest, CaptureManifest, ConsentManifest, RemoteSourceKind, SessionId, SessionManifest, SessionSummary, TrackKind};

use crate::app_state::{ActiveRecording, AppState};
use crate::hint_consumer::HintBuffer;
use crate::ui_consumer::TranscriptBuffer;

/// Sentinel meaning "whatever the OS reports as its current default device" —
/// `AppSettings::microphone_device_id`/`render_device_id`'s `None` (nothing chosen
/// in the settings screen's "録音デバイス" section yet) resolves to this before
/// being recorded in the manifest, matching `capture-windows`'s/`capture-macos`'s
/// own `"default"` sentinel (see `capture_windows::device_select::
/// resolve_capture_device`).
const DEFAULT_DEVICE_ID: &str = "default";

fn build_manifest(mic_device_id: &str, render_device_id: &str) -> SessionManifest {
    let now = Utc::now();
    SessionManifest {
        schema_version: 1,
        session_id: SessionId::new(),
        started_at: now,
        ended_at: None,
        platform: std::env::consts::OS.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        capture: CaptureManifest {
            microphone_device_id: mic_device_id.to_string(),
            remote_source_id: render_device_id.to_string(),
            remote_source_kind: RemoteSourceKind::EndpointLoopback,
        },
        audio: AudioManifest { sample_rate: 48_000, segment_duration_ms: 30_000, tracks: vec![TrackKind::SelfMic, TrackKind::RemoteAudio] },
        consent: ConsentManifest { confirmed_by_user: true, confirmed_at: now },
    }
}

/// Tells every session-scoped background task (`ui_consumer`, `hint_consumer`,
/// `RhaiEngine::spawn_session`, `RhaiEngine::spawn_hint_debounce_driver`,
/// `SummaryConsumer::spawn_auto_summary`) that this session is actually over,
/// so each one can finalize (if it needs to — auto-summary) and exit — see
/// those modules' own doc comments for why none of them can reliably detect
/// this on their own: several wait for `UtteranceEnded(SessionEnd)`, which
/// only the Windows-only `app_service::live_transcription` ever publishes.
/// A subject nobody is (or is still) subscribed to is a silent no-op, not an
/// error — this is safe to call unconditionally regardless of which
/// optional consumers (e.g. hints, if disabled) were ever spawned for this
/// session, or whether some of them have already ended via a real
/// `SessionEnd` broadcast by the time this runs.
fn publish_session_stopped(state: &AppState, session_id: SessionId) {
    let _ = state.broker.publish_bytes(&format!("session.{session_id}.stopped"), Vec::new());
}

#[cfg(windows)]
pub fn start(state: &AppState) -> Result<SessionId, String> {
    // The settings screen's "録音デバイス" section (`settings.rs`) persists these as
    // `Some(DeviceInfo::id)` once the user picks a specific mic/speaker; `None`
    // (nothing chosen yet) falls back to `DEFAULT_DEVICE_ID`, matching the
    // pre-existing "always follow the OS default" behavior.
    let (mic_device_id, render_device_id) = {
        let settings = state.app_settings.lock().unwrap();
        (settings.microphone_device_id.clone(), settings.render_device_id.clone())
    };
    let manifest = build_manifest(mic_device_id.as_deref().unwrap_or(DEFAULT_DEVICE_ID), render_device_id.as_deref().unwrap_or(DEFAULT_DEVICE_ID));
    let session_id = manifest.session_id;

    let level = std::sync::Arc::new(std::sync::Mutex::new(app_service::LevelSnapshot::default()));
    // Task #52: same side-channel shape as `level`, for the Deepgram connection
    // status `run_windows_capture_session` -> `live_transcription` maintains.
    let transcription_status = std::sync::Arc::new(std::sync::Mutex::new(app_service::TranscriptionStatus::default()));
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::unbounded();

    let store = state.store.clone();
    let adapter = state.adapter.clone();
    let session_dir = state.config.sessions_root.join(session_id.to_string());
    let bitrate_bps = state.config.bitrate_bps;
    let manifest_for_task = manifest.clone();
    let level_for_task = level.clone();
    let transcription_status_for_task = transcription_status.clone();
    // `state.credential_store` also backs the settings screen (`settings.rs`) —
    // `run_windows_capture_session`'s `live_transcription` wiring reads the
    // Deepgram key from it, if any was ever saved there. No key configured just
    // means no live transcription (see that module's doc comment), not a failure.
    let credential_store_for_task: Arc<dyn credential_store::CredentialStore + Send + Sync> = state.credential_store.clone();
    // See `app_settings::AppSettings::silence_gate_enabled`'s doc comment —
    // `None` (no settings-UI toggle yet, or never saved) means "off", matching
    // the pre-existing always-send-everything behavior.
    let silence_gate_enabled_for_task = state.app_settings.lock().unwrap().silence_gate_enabled.unwrap_or(false);
    let broker_for_task = state.broker.clone();

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
            Some(transcription_status_for_task),
            silence_gate_enabled_for_task,
            Some(&broker_for_task),
            mic_device_id,
            render_device_id,
        )
        .await
    });

    *state.current.lock().unwrap() = Some(ActiveRecording {
        session_id,
        manifest,
        started_at: Instant::now(),
        level,
        transcription_status,
        shutdown_tx,
        join_handle,
        transcript_buffer: TranscriptBuffer::new(),
        hint_buffer: HintBuffer::new(),
    });
    Ok(session_id)
}

#[cfg(windows)]
pub async fn stop(state: &AppState) -> Result<SessionSummary, String> {
    let active = state.current.lock().unwrap().take().ok_or_else(|| "not recording".to_string())?;
    let session_id = active.session_id;

    // Wrapped in a block (rather than `?`-returning directly from `stop`) so
    // that a failed shutdown signal or a capture-task panic can't skip the
    // `publish_session_stopped` call below — an earlier version of this
    // function *did* return early via `?` on exactly these two error paths,
    // silently leaking every session-scoped background task (and skipping
    // auto-summary) on a capture-task panic, i.e. the precise failure this
    // whole redesign exists to prevent.
    let result = async {
        active.shutdown_tx.send(()).map_err(|e| format!("failed to signal shutdown: {e}"))?;
        active.join_handle.await.map_err(|e| format!("capture task panicked: {e}"))?.map_err(|e| e.to_string())
    }
    .await;

    // Published *after* the capture task (and with it,
    // `app_service::live_transcription`'s own `UtteranceEnded(SessionEnd)`
    // publish) has fully resolved, whether or not that resolved
    // successfully — see `publish_session_stopped`'s doc comment. On
    // Windows the real `SessionEnd` normally already finalized
    // auto-summary/hints by this point, making this call a no-op; it's the
    // only signal at all on platforms without live transcription, and the
    // only one at all if the capture task panicked before publishing its
    // own `SessionEnd`.
    publish_session_stopped(state, session_id);
    result
}

/// The macOS analogue of the `#[cfg(windows)]` `start` above — real
/// ScreenCaptureKit capture via `app_service::run_macos_capture_session`. **Not
/// yet run on real macOS hardware, or even compiled at all** — see
/// `crates/capture-macos`'s crate doc comment and README.
#[cfg(target_os = "macos")]
pub fn start(state: &AppState) -> Result<SessionId, String> {
    // See the `#[cfg(windows)]` `start` above's identical comment.
    let (mic_device_id, render_device_id) = {
        let settings = state.app_settings.lock().unwrap();
        (settings.microphone_device_id.clone(), settings.render_device_id.clone())
    };
    let manifest = build_manifest(mic_device_id.as_deref().unwrap_or(DEFAULT_DEVICE_ID), render_device_id.as_deref().unwrap_or(DEFAULT_DEVICE_ID));
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
            mic_device_id,
            render_device_id,
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
        transcript_buffer: TranscriptBuffer::new(),
        hint_buffer: HintBuffer::new(),
    });
    Ok(session_id)
}

#[cfg(target_os = "macos")]
pub async fn stop(state: &AppState) -> Result<SessionSummary, String> {
    let active = state.current.lock().unwrap().take().ok_or_else(|| "not recording".to_string())?;
    let session_id = active.session_id;

    // See the `#[cfg(windows)]` `stop` above's identical comment on why this
    // is wrapped in a block rather than `?`-returning directly.
    let result = async {
        active.shutdown_tx.send(()).map_err(|e| format!("failed to signal shutdown: {e}"))?;
        active.join_handle.await.map_err(|e| format!("capture task panicked: {e}"))?.map_err(|e| e.to_string())
    }
    .await;

    // macOS has no live-transcription pipeline at all yet (see
    // `capture-macos`'s doc comment), so — success or failure above — this
    // is the *only* signal auto-summary/hints ever get that the session
    // actually ended here.
    publish_session_stopped(state, session_id);
    result
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
    let manifest = build_manifest(DEFAULT_DEVICE_ID, DEFAULT_DEVICE_ID);
    let session_id = manifest.session_id;
    *state.current.lock().unwrap() = Some(ActiveRecording {
        session_id,
        manifest,
        started_at: Instant::now(),
        transcript_buffer: TranscriptBuffer::new(),
        hint_buffer: HintBuffer::new(),
    });
    Ok(session_id)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub async fn stop(state: &AppState) -> Result<SessionSummary, String> {
    use app_service::pseudo_source::{generate_frames, nominal_frame_interval_ns, PseudoSourceConfig};

    let active = state.current.lock().unwrap().take().ok_or_else(|| "not recording".to_string())?;
    let session_id = active.session_id;
    let duration_secs = active.started_at.elapsed().as_secs().max(1) as u32;
    let session_dir = state.config.sessions_root.join(active.session_id.to_string());

    let config = PseudoSourceConfig { duration_secs, frame_interval_ms: 20, sample_rate: 48_000, channels: 1, tone_freq_hz: 440.0 };
    let self_frames = generate_frames(TrackKind::SelfMic, &config);
    let remote_frames = generate_frames(TrackKind::RemoteAudio, &config);
    let total_duration_ns = duration_secs as u64 * 1_000_000_000;
    let interval_ns = nominal_frame_interval_ns(&config);

    let result = app_service::run_pipeline(&active.manifest, &self_frames, &remote_frames, interval_ns, interval_ns, total_duration_ns, &session_dir, state.config.bitrate_bps, &state.store, state.adapter.as_ref())
        .await
        .map_err(|e| e.to_string());
    // No real capture/live-transcription pipeline on this platform at all —
    // see `publish_session_stopped`'s doc comment — so this is unconditionally
    // the only signal auto-summary/hints ever get here.
    publish_session_stopped(state, session_id);
    result
}
