//! Task #52's desktop-side mirror of `app_service::{TrackTranscriptionStatus,
//! TranscriptionStatus}` — same duplication rationale as `level::LevelSnapshot`:
//! this crate's status DTO (`status::Status`) needs one plain, always-available
//! type, not the `windows-supervisor`-gated `app_service` one, so non-Windows
//! builds (where `app_service::TranscriptionStatus` doesn't even exist) still
//! compile.

use recorder_domain::TrackKind;

use crate::transcript::track_label;

// `Connecting`/`Connected`/`Error` are only ever constructed by the `#[cfg(windows)]`
// `From<app_service::TrackTranscriptionStatus>` impl below — on every other
// platform this crate only ever produces `NotConfigured` (`Default`) or
// `Unavailable` (`TranscriptionStatus::unavailable`), so those variants read as
// dead code there.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TrackTranscriptionStatus {
    #[default]
    NotConfigured,
    Connecting,
    Connected,
    Error(String),
    Unavailable,
}

#[derive(Debug, Clone, Default)]
pub struct TranscriptionStatus {
    pub self_status: TrackTranscriptionStatus,
    pub remote_status: TrackTranscriptionStatus,
}

impl TranscriptionStatus {
    /// Used on platforms with no live transcription wiring at all (macOS, dev
    /// fallback — see `app_state::ActiveRecording`'s doc comment): both tracks
    /// report `Unavailable` rather than the default `NotConfigured`, since
    /// "unconfigured" would incorrectly suggest setting an API key could fix it.
    /// Unused on a Windows build (the only platform with a `transcription_status`
    /// field to report from — see `status::current`).
    #[cfg_attr(windows, allow(dead_code))]
    pub fn unavailable() -> Self {
        Self { self_status: TrackTranscriptionStatus::Unavailable, remote_status: TrackTranscriptionStatus::Unavailable }
    }
}

#[cfg(windows)]
impl From<app_service::TrackTranscriptionStatus> for TrackTranscriptionStatus {
    fn from(s: app_service::TrackTranscriptionStatus) -> Self {
        match s {
            app_service::TrackTranscriptionStatus::NotConfigured => Self::NotConfigured,
            app_service::TrackTranscriptionStatus::Connecting => Self::Connecting,
            app_service::TrackTranscriptionStatus::Connected => Self::Connected,
            app_service::TrackTranscriptionStatus::Error(msg) => Self::Error(msg),
            app_service::TrackTranscriptionStatus::Unavailable => Self::Unavailable,
        }
    }
}

#[cfg(windows)]
impl From<app_service::TranscriptionStatus> for TranscriptionStatus {
    fn from(s: app_service::TranscriptionStatus) -> Self {
        Self { self_status: s.self_status.into(), remote_status: s.remote_status.into() }
    }
}

fn describe_track(status: &TrackTranscriptionStatus) -> Option<String> {
    match status {
        TrackTranscriptionStatus::NotConfigured => Some("キー未設定です(設定画面でAPIキーを入力してください)".to_string()),
        TrackTranscriptionStatus::Connecting => Some("接続中...".to_string()),
        TrackTranscriptionStatus::Connected => None,
        TrackTranscriptionStatus::Error(msg) => Some(format!("接続エラー({msg})")),
        TrackTranscriptionStatus::Unavailable => Some("この環境では文字起こしを利用できません".to_string()),
    }
}

/// One status line for the transcript panel (`ui.rs`), or `None` when both tracks
/// are `Connected` (nothing worth surfacing). Never collapses Self/Remote into a
/// single undifferentiated message — see this module's doc comment — but does
/// collapse to one shared sentence when both tracks currently say the same thing,
/// so a fully-broken session doesn't read as two redundant lines.
pub fn describe(status: &TranscriptionStatus) -> Option<String> {
    match (describe_track(&status.self_status), describe_track(&status.remote_status)) {
        (None, None) => None,
        (Some(both_a), Some(both_b)) if both_a == both_b => Some(format!("文字起こし: {both_a}")),
        (Some(only_self), None) => Some(format!("文字起こし: {}のみ{only_self}", track_label(Some(TrackKind::SelfMic)))),
        (None, Some(only_remote)) => Some(format!("文字起こし: {}のみ{only_remote}", track_label(Some(TrackKind::RemoteAudio)))),
        (Some(self_msg), Some(remote_msg)) => Some(format!(
            "文字起こし: {}={self_msg} / {}={remote_msg}",
            track_label(Some(TrackKind::SelfMic)),
            track_label(Some(TrackKind::RemoteAudio))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_is_none_when_both_connected() {
        let status = TranscriptionStatus { self_status: TrackTranscriptionStatus::Connected, remote_status: TrackTranscriptionStatus::Connected };
        assert_eq!(describe(&status), None);
    }

    #[test]
    fn describe_collapses_identical_statuses_into_one_line() {
        let status = TranscriptionStatus { self_status: TrackTranscriptionStatus::Connecting, remote_status: TrackTranscriptionStatus::Connecting };
        assert_eq!(describe(&status), Some("文字起こし: 接続中...".to_string()));
    }

    #[test]
    fn describe_distinguishes_a_single_failed_track() {
        let status = TranscriptionStatus { self_status: TrackTranscriptionStatus::Connected, remote_status: TrackTranscriptionStatus::Error("boom".to_string()) };
        let msg = describe(&status).unwrap();
        assert!(msg.contains("相手"));
        assert!(msg.contains("boom"));
    }

    #[test]
    fn unavailable_reports_both_tracks_unavailable() {
        let status = TranscriptionStatus::unavailable();
        assert_eq!(status.self_status, TrackTranscriptionStatus::Unavailable);
        assert_eq!(status.remote_status, TrackTranscriptionStatus::Unavailable);
    }
}
