//! Track/speaker label formatting shared by the live transcript panel (#33/#34)
//! and the summary transcript conversion (#38) — one place decides how a
//! `TranscriptSegment`'s `track`/`speaker` becomes a human-readable label, so the
//! chat view and the text sent to the LLM agree on speaker naming.

use std::collections::{HashMap, HashSet};

use recorder_domain::TrackKind;
use session_store::{TranscriptSegment, TranscriptionGap};
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

/// One row of the transcript panel's merged timeline (task #92): either a
/// transcript bubble or a `transcription_gaps` marker (task #90), interleaved
/// by position rather than shown as two separate lists — see
/// [`timeline_items`]'s doc comment for how position is derived.
#[derive(Debug, Clone, Copy)]
pub enum TimelineItem<'a> {
    Segment(&'a TranscriptSegment),
    Gap(&'a TranscriptionGap),
}

/// Merges [`visible_segments`] with every gap in `gaps` into one chronological
/// list for the transcript panel (task #92), ordered by each item's own start
/// time: a segment's `start_ms` (pushed to the end when unset — the provider
/// never sent one, so there's no better position to guess at than "last"), or
/// a gap's `start_ms` (always present). This can't just be "render segments,
/// then gaps" or rely on `list_transcript_segments`' own insertion order the
/// way [`visible_segments`] does: a re-transcribed segment (task #91) is
/// persisted well after the live rows around it, so it needs to be sorted back
/// into its actual chronological spot rather than trailing at the end where it
/// was inserted. `sort_by_key` is stable, so items that tie on position (most
/// commonly two rows with no `start_ms` at all) keep `visible_segments`'/
/// `gaps`' own relative order instead of shuffling.
pub fn timeline_items<'a>(segments: &'a [TranscriptSegment], gaps: &'a [TranscriptionGap]) -> Vec<TimelineItem<'a>> {
    let mut items: Vec<TimelineItem<'a>> = visible_segments(segments).into_iter().map(TimelineItem::Segment).collect();
    items.extend(gaps.iter().map(TimelineItem::Gap));
    items.sort_by_key(|item| match item {
        TimelineItem::Segment(s) => s.start_ms.unwrap_or(u64::MAX),
        TimelineItem::Gap(g) => g.start_ms,
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use recorder_domain::SessionId;

    fn gap(id: i64, track: TrackKind, start_ms: u64, end_ms: Option<u64>) -> TranscriptionGap {
        TranscriptionGap { id, session_id: SessionId::new(), track, start_ms, end_ms }
    }

    fn seg(track: Option<TrackKind>, text: &str, is_final: bool) -> TranscriptSegment {
        TranscriptSegment {
            session_id: recorder_domain::SessionId::new(),
            track,
            speaker: None,
            text: text.to_string(),
            start_ms: None,
            end_ms: None,
            is_final,
            is_retranscribed: false,
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

    /// Like `seg`, but with an explicit `start_ms` — `timeline_items` sorts on
    /// it, unlike `visible_segments`, so its tests need real values instead of
    /// `seg`'s hardcoded `None`.
    fn seg_at(track: Option<TrackKind>, text: &str, start_ms: u64) -> TranscriptSegment {
        TranscriptSegment {
            session_id: SessionId::new(),
            track,
            speaker: None,
            text: text.to_string(),
            start_ms: Some(start_ms),
            end_ms: Some(start_ms + 100),
            is_final: true,
            is_retranscribed: false,
        }
    }

    fn timeline_labels(items: &[TimelineItem<'_>]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                TimelineItem::Segment(s) => s.text.clone(),
                TimelineItem::Gap(g) => format!("gap#{}", g.id),
            })
            .collect()
    }

    #[test]
    fn timeline_items_interleaves_a_gap_between_segments_by_start_ms() {
        let segments = vec![seg_at(Some(TrackKind::SelfMic), "before", 0), seg_at(Some(TrackKind::SelfMic), "after", 5_000)];
        let gaps = vec![gap(1, TrackKind::RemoteAudio, 1_000, Some(3_000))];

        let items = timeline_items(&segments, &gaps);
        assert_eq!(timeline_labels(&items), vec!["before", "gap#1", "after"]);
    }

    #[test]
    fn timeline_items_places_a_retranscribed_segment_back_in_chronological_order() {
        // Mirrors `retranscribe_gap`'s real effect: a gap closes, its
        // `TranscriptSegment`s are `insert_transcript_segment`-ed (so they're
        // last in `list_transcript_segments`' insertion order), but their
        // `start_ms` sits *before* segments that were live-transcribed earlier
        // in wall-clock time. `list_transcript_segments` would put the
        // re-transcribed row last; `timeline_items` must not.
        let live_early = seg_at(Some(TrackKind::SelfMic), "live early", 0);
        let live_late = seg_at(Some(TrackKind::SelfMic), "live late", 10_000);
        // Inserted after both live rows (as `retranscribe_gap` would), but its
        // own start_ms falls between them.
        let retranscribed = seg_at(Some(TrackKind::RemoteAudio), "retranscribed", 5_000);
        let segments = vec![live_early, live_late, retranscribed];

        let items = timeline_items(&segments, &[]);
        assert_eq!(timeline_labels(&items), vec!["live early", "retranscribed", "live late"]);
    }

    #[test]
    fn timeline_items_keeps_stable_order_for_ties() {
        let segments = vec![seg_at(Some(TrackKind::SelfMic), "one", 100), seg_at(Some(TrackKind::RemoteAudio), "two", 100)];
        let items = timeline_items(&segments, &[]);
        assert_eq!(timeline_labels(&items), vec!["one", "two"]);
    }

    #[test]
    fn timeline_items_with_no_gaps_matches_visible_segments() {
        let segments = vec![seg(Some(TrackKind::SelfMic), "final one", true), seg(Some(TrackKind::SelfMic), "in prog", false)];
        let items = timeline_items(&segments, &[]);
        assert_eq!(timeline_labels(&items), vec!["final one", "in prog"]);
    }
}
