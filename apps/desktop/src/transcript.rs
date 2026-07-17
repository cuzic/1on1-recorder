//! Track/speaker label formatting shared by the live transcript panel (#33/#34)
//! and the summary transcript conversion (#38) — one place decides how a
//! `TranscriptSegment`'s `track`/`speaker` becomes a human-readable label, so the
//! chat view and the text sent to the LLM agree on speaker naming.

use recorder_domain::TrackKind;
use session_store::TranscriptSegment;
use summarize::TranscriptTurn;

/// Base label for a segment's track — the app's own primary speaker axis (see
/// `live_transcription.rs`'s doc comment: this app always captures Self/Remote as
/// separate tracks, so that split — not Deepgram diarization — is the main one).
pub fn track_label(track: Option<TrackKind>) -> &'static str {
    match track {
        Some(TrackKind::SelfMic) => "自分",
        Some(TrackKind::RemoteAudio) => "相手",
        None => "不明",
    }
}

/// Combines the track label with Deepgram's diarization `speaker` index (only
/// populated once `SttSessionConfig::with_diarization` is set — see
/// `live_transcription.rs`) — e.g. "相手 (話者2)" for a track with more than one
/// speaker on it (a multi-person Remote call). Deepgram's index is 0-based.
pub fn speaker_label(track: Option<TrackKind>, speaker: Option<u32>) -> String {
    match speaker {
        Some(n) => format!("{} (話者{})", track_label(track), n + 1),
        None => track_label(track).to_string(),
    }
}

/// Converts finalized transcript segments into `summarize::TranscriptTurn`s —
/// interim (`is_final: false`) rows are dropped since a later final row
/// supersedes them (see #38's task note).
pub fn to_turns(segments: &[TranscriptSegment]) -> Vec<TranscriptTurn> {
    segments
        .iter()
        .filter(|s| s.is_final)
        .map(|s| TranscriptTurn { speaker: Some(speaker_label(s.track, s.speaker)), text: s.text.clone() })
        .collect()
}
