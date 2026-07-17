//! Track/speaker label formatting shared by the live transcript panel (#33/#34)
//! and the summary transcript conversion (#38) — one place decides how a
//! `TranscriptSegment`'s `track`/`speaker` becomes a human-readable label, so the
//! chat view and the text sent to the LLM agree on speaker naming.

use std::collections::{HashMap, HashSet};

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

/// Collapses `list_transcript_segments`'s raw insertion-order rows into what the
/// live panel should actually render (#51): `persist_event` in
/// `live_transcription.rs` inserts every Partial *and* Final update as its own
/// new row, so rendering the raw list re-shows each utterance's superseded
/// interim guesses as separate bubbles forever. A final row is always kept (it's
/// never superseded again); an interim row is kept only if it's the
/// last-occurring row for its track — i.e. that track's in-flight utterance
/// hasn't been finalized yet.
pub fn visible_segments(segments: &[TranscriptSegment]) -> Vec<&TranscriptSegment> {
    let mut last_index_for_track: HashMap<Option<TrackKind>, usize> = HashMap::new();
    for (i, seg) in segments.iter().enumerate() {
        last_index_for_track.insert(seg.track, i);
    }
    let keep_interim: HashSet<usize> =
        last_index_for_track.into_values().filter(|&i| !segments[i].is_final).collect();

    segments.iter().enumerate().filter(|(i, seg)| seg.is_final || keep_interim.contains(i)).map(|(_, seg)| seg).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(track: Option<TrackKind>, text: &str, is_final: bool) -> TranscriptSegment {
        TranscriptSegment {
            session_id: recorder_domain::SessionId::new(),
            track,
            speaker: None,
            text: text.to_string(),
            start_ms: None,
            end_ms: None,
            is_final,
        }
    }

    fn texts<'a>(segments: &'a [&'a TranscriptSegment]) -> Vec<&'a str> {
        segments.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn finals_only_are_all_kept() {
        let segments = vec![seg(Some(TrackKind::SelfMic), "one", true), seg(Some(TrackKind::SelfMic), "two", true)];
        assert_eq!(texts(&visible_segments(&segments)), vec!["one", "two"]);
    }

    #[test]
    fn interim_only_keeps_last_per_track() {
        let segments = vec![seg(Some(TrackKind::SelfMic), "hel", false), seg(Some(TrackKind::SelfMic), "hello", false)];
        assert_eq!(texts(&visible_segments(&segments)), vec!["hello"]);
    }

    #[test]
    fn finals_then_trailing_interim_keeps_both() {
        let segments = vec![
            seg(Some(TrackKind::SelfMic), "final one", true),
            seg(Some(TrackKind::SelfMic), "final two", true),
            seg(Some(TrackKind::SelfMic), "in prog", false),
        ];
        assert_eq!(texts(&visible_segments(&segments)), vec!["final one", "final two", "in prog"]);
    }

    #[test]
    fn interim_superseded_by_a_later_final_is_dropped() {
        let segments = vec![seg(Some(TrackKind::SelfMic), "hel", false), seg(Some(TrackKind::SelfMic), "hello", true)];
        assert_eq!(texts(&visible_segments(&segments)), vec!["hello"]);
    }

    #[test]
    fn mixed_tracks_each_keep_their_own_trailing_interim() {
        let segments = vec![
            seg(Some(TrackKind::SelfMic), "self final", true),
            seg(Some(TrackKind::RemoteAudio), "remote partial", false),
            seg(Some(TrackKind::SelfMic), "self partial", false),
            seg(None, "unknown partial", false),
        ];
        let matched = visible_segments(&segments);
        let visible = texts(&matched);
        assert_eq!(visible.len(), 4);
        assert!(visible.contains(&"self final"));
        assert!(visible.contains(&"remote partial"));
        assert!(visible.contains(&"self partial"));
        assert!(visible.contains(&"unknown partial"));
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let segments: Vec<TranscriptSegment> = vec![];
        assert!(visible_segments(&segments).is_empty());
    }
}
