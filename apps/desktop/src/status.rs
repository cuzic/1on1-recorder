use recorder_domain::{SessionId, TrackKind};

use crate::app_state::AppState;

/// design.md §14.1/§14.2's initial + recording screens, combined into one DTO the
/// UI polls (`status::current`) and switches its view on — Rust stays the single
/// source of truth for capture/upload state (design.md §6.1); the UI never tracks
/// its own copy.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub recording: bool,
    pub elapsed_ms: u64,
    pub self_rms: f32,
    pub self_peak: f32,
    pub remote_rms: f32,
    pub remote_peak: f32,
    pub consent_confirmed: bool,
    pub uploaded_segments: usize,
    pub pending_segments: usize,
    pub last_error: Option<String>,
    pub last_session_id: Option<String>,
    pub last_total_duration_ms: Option<u64>,
    /// #52's STT connection visibility — `crate::transcription_status::describe`
    /// turns this into the transcript panel's status line.
    pub transcription_status: crate::transcription_status::TranscriptionStatus,
}

fn segment_progress(state: &AppState, session_id: SessionId) -> (usize, usize) {
    let pending = state.store.pending_uploads(session_id).map(|v| v.len()).unwrap_or(0);
    let total: usize = [TrackKind::SelfMic, TrackKind::RemoteAudio]
        .iter()
        .map(|track| state.store.segments_for_track(session_id, *track).map(|v| v.len()).unwrap_or(0))
        .sum();
    (total.saturating_sub(pending), pending)
}

pub fn current(state: &AppState) -> Status {
    let consent_confirmed = *state.consent_confirmed.lock().unwrap();
    let last_error = state.last_error.lock().unwrap().clone();
    let current = state.current.lock().unwrap();

    let Some(active) = current.as_ref() else {
        drop(current);
        let last_summary = state.last_summary.lock().unwrap();
        return Status {
            consent_confirmed,
            last_error,
            last_session_id: last_summary.as_ref().map(|s| s.session_id.to_string()),
            last_total_duration_ms: last_summary.as_ref().map(|s| s.total_duration_ms),
            ..Status::default()
        };
    };

    let elapsed = active.started_at.elapsed();
    #[cfg(windows)]
    let level: crate::level::LevelSnapshot = (*active.level.lock().unwrap()).into();
    #[cfg(target_os = "macos")]
    let level: crate::level::LevelSnapshot = (*active.level.lock().unwrap()).into();
    #[cfg(not(any(windows, target_os = "macos")))]
    let level = crate::level::dev_placeholder_level(elapsed);

    // #52: only Windows wires up live transcription at all (see
    // `app_state::ActiveRecording`'s doc comment); every other platform reports
    // `Unavailable` for both tracks rather than the default `NotConfigured`, since
    // there's no settings-screen fix for "unavailable".
    #[cfg(windows)]
    let transcription_status: crate::transcription_status::TranscriptionStatus = active.transcription_status.lock().unwrap().clone().into();
    #[cfg(not(windows))]
    let transcription_status = crate::transcription_status::TranscriptionStatus::unavailable();

    let (uploaded_segments, pending_segments) = segment_progress(state, active.session_id);

    Status {
        recording: true,
        elapsed_ms: elapsed.as_millis() as u64,
        self_rms: level.self_rms,
        self_peak: level.self_peak,
        remote_rms: level.remote_rms,
        remote_peak: level.remote_peak,
        consent_confirmed,
        uploaded_segments,
        pending_segments,
        last_error,
        last_session_id: Some(active.session_id.to_string()),
        last_total_duration_ms: None,
        transcription_status,
    }
}
