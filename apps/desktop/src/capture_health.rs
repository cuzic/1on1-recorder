//! Turns `app_service::CaptureHealth` into a warning line for the recording
//! screen (`ui.rs`) — the counterpart to `transcription_status::describe`, but for
//! capture itself rather than STT. Unlike `level::LevelSnapshot`/
//! `transcription_status::TranscriptionStatus`, `app_service::CaptureHealth`
//! carries no platform-specific content and is unconditionally exported (see its
//! own doc comment), so there is no local mirror type here — only this formatter.

use recorder_domain::TrackKind;

use crate::transcript::track_label;

fn describe_track(health: &app_service::TrackHealth) -> Option<String> {
    match health {
        app_service::TrackHealth::Ok => None,
        app_service::TrackHealth::Unavailable => Some("デバイスが切断されました(再接続を待っています)".to_string()),
        app_service::TrackHealth::Retrying { attempt } => Some(format!("音声を取得できません(再試行中 {attempt}回目)")),
        app_service::TrackHealth::Failed { .. } => Some("音声を取得できません(復旧に失敗しました)".to_string()),
    }
}

/// One warning line for the recording screen, or `None` when both tracks are
/// healthy. Same Self/Remote differentiation shape as
/// `transcription_status::describe` — never collapses a single-track problem into
/// an undifferentiated message.
pub fn describe(health: &app_service::CaptureHealth) -> Option<String> {
    match (describe_track(&health.self_health), describe_track(&health.remote_health)) {
        (None, None) => None,
        (Some(both_a), Some(both_b)) if both_a == both_b => Some(both_a),
        (Some(only_self), None) => Some(format!("{}: {only_self}", track_label(Some(TrackKind::SelfMic)))),
        (None, Some(only_remote)) => Some(format!("{}: {only_remote}", track_label(Some(TrackKind::RemoteAudio)))),
        (Some(self_msg), Some(remote_msg)) => Some(format!(
            "{}: {self_msg} / {}: {remote_msg}",
            track_label(Some(TrackKind::SelfMic)),
            track_label(Some(TrackKind::RemoteAudio))
        )),
    }
}

#[cfg(test)]
mod tests {
    use app_service::{CaptureHealth, TrackHealth};

    use super::*;

    #[test]
    fn describe_is_none_when_both_ok() {
        assert_eq!(describe(&CaptureHealth::default()), None);
    }

    #[test]
    fn describe_collapses_identical_statuses_into_one_line() {
        let health = CaptureHealth { self_health: TrackHealth::Unavailable, remote_health: TrackHealth::Unavailable };
        assert_eq!(describe(&health), Some("デバイスが切断されました(再接続を待っています)".to_string()));
    }

    #[test]
    fn describe_distinguishes_a_single_failed_track() {
        let health = CaptureHealth { self_health: TrackHealth::Ok, remote_health: TrackHealth::Failed { reason: "boom".to_string() } };
        let msg = describe(&health).unwrap();
        assert!(msg.contains("相手"));
    }

    #[test]
    fn describe_shows_retry_attempt_number() {
        let health = CaptureHealth { self_health: TrackHealth::Retrying { attempt: 2 }, remote_health: TrackHealth::Ok };
        let msg = describe(&health).unwrap();
        assert!(msg.contains("自分"));
        assert!(msg.contains('2'));
    }
}
