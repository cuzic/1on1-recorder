//! Manual "export latest session to Markdown" feature: writes the current/most
//! recent session's finalized transcript (and its latest summary, if one has
//! been generated) to a local `.md` file. Triggered only by the "エクスポート"
//! button in `ui.rs`'s summary panel — there is no automatic/background export.
//!
//! Split into a pure rendering half (`render_markdown`, no I/O) and a thin file
//! I/O half (`export_session`) so the Markdown shape can be unit tested without
//! a `SessionStore` or filesystem.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use recorder_domain::SessionId;
use session_store::TranscriptSegment;

use crate::app_state::AppState;
use crate::transcript;

/// Renders one session's transcript (and optional summary) as Markdown.
///
/// `segments` is expected to be `list_transcript_segments`'s raw, unfiltered
/// result — `persist_event` (`app-service::live_transcription`) inserts every
/// `PartialTranscript` *and* `FinalTranscript` update as its own row, so this
/// function itself filters down to `is_final == true` rows before rendering.
/// Forgetting this filter (e.g. by pre-filtering at the call site and assuming
/// this function trusts its input) would leak discarded interim recognition
/// results into the exported file — see this crate's task note.
///
/// Segments without a `start_ms` timestamp (a provider that doesn't report
/// ranges) are rendered with no leading timestamp rather than a placeholder or
/// panic.
pub fn render_markdown(started_at: DateTime<Utc>, segments: &[TranscriptSegment], summary: Option<&str>) -> String {
    let mut out = String::new();

    out.push_str(&format!("# 1on1 セッション {}\n\n", started_at.format("%Y-%m-%d %H:%M:%S")));

    out.push_str("## 文字起こし\n\n");
    let finals: Vec<&TranscriptSegment> = segments.iter().filter(|s| s.is_final).collect();
    if finals.is_empty() {
        out.push_str("_(文字起こしはありません)_\n");
    } else {
        for seg in finals {
            let label = transcript::speaker_label(seg.track, seg.speaker);
            match seg.start_ms {
                Some(start_ms) => out.push_str(&format!("- **[{}] {label}**: {}\n", format_timestamp(start_ms), seg.text)),
                None => out.push_str(&format!("- **{label}**: {}\n", seg.text)),
            }
        }
    }
    out.push('\n');

    if let Some(summary) = summary {
        out.push_str("## 要約\n\n");
        out.push_str(summary);
        out.push('\n');
    }

    out
}

/// `mm:ss` for a millisecond offset — mirrors `ui.rs`'s (private) `format_elapsed`,
/// duplicated rather than shared since that one is scoped to the elapsed-recording
/// display and this one is scoped to exported transcript timestamps.
fn format_timestamp(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

/// Export directory: `AppSettings::exports_root` if the user has set one,
/// otherwise `app_data_dir/exports`. Created if it doesn't exist yet.
fn export_dir(state: &AppState) -> Result<PathBuf, String> {
    let configured = state.app_settings.lock().unwrap().exports_root.clone();
    let dir = configured.unwrap_or_else(|| state.app_data_dir.join("exports"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("エクスポート先フォルダの作成に失敗しました: {e}"))?;
    Ok(dir)
}

/// Exports `session_id`'s finalized transcript and latest summary (if any) to a
/// Markdown file named after the session's start time, returning the written
/// path on success.
///
/// The start time comes from `session_id`'s own ULID timestamp rather than a
/// fresh `SessionStore` lookup: `session-store` has no `get_manifest`/read-back
/// API for `SessionManifest` today (only `create_session`, which is
/// write-only), and `SessionId::new()` is always minted from the same
/// `Utc::now()` call that becomes `SessionManifest::started_at`
/// (`recording.rs::build_manifest`), so decoding it here needs no schema change
/// and stays accurate to the second.
///
/// Same-named files (i.e. two exports of the same session) are overwritten —
/// no numbered-suffix handling, per this feature's scope.
pub fn export_session(state: &AppState, session_id: SessionId) -> Result<PathBuf, String> {
    let segments = state.store.list_transcript_segments(session_id).map_err(|e| format!("文字起こしの取得に失敗しました: {e}"))?;
    let summary = state.store.get_latest_summary(session_id).map_err(|e| format!("要約の取得に失敗しました: {e}"))?;

    let started_at = DateTime::<Utc>::from_timestamp_millis(session_id.0.timestamp_ms() as i64).unwrap_or_else(Utc::now);

    let markdown = render_markdown(started_at, &segments, summary.as_ref().map(|s| s.text.as_str()));

    let dir = export_dir(state)?;
    let file_name = format!("{}.md", started_at.format("%Y-%m-%d_%H%M%S"));
    let path = dir.join(file_name);
    std::fs::write(&path, markdown).map_err(|e| format!("ファイルの書き込みに失敗しました: {e}"))?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use recorder_domain::TrackKind;

    fn seg(track: Option<TrackKind>, speaker: Option<u32>, text: &str, start_ms: Option<u64>, end_ms: Option<u64>, is_final: bool) -> TranscriptSegment {
        TranscriptSegment { session_id: SessionId::new(), track, speaker, text: text.to_string(), start_ms, end_ms, is_final }
    }

    fn sample_started_at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-18T14:30:22Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn renders_heading_with_session_start_time() {
        let markdown = render_markdown(sample_started_at(), &[], None);
        assert!(markdown.starts_with("# 1on1 セッション 2026-07-18 14:30:22\n\n"));
    }

    #[test]
    fn interim_segments_are_filtered_out() {
        let segments = vec![
            seg(Some(TrackKind::SelfMic), None, "final one", Some(0), Some(1000), true),
            seg(Some(TrackKind::SelfMic), None, "interim in-progress guess", Some(1000), None, false),
        ];
        let markdown = render_markdown(sample_started_at(), &segments, None);

        assert!(markdown.contains("final one"), "final segment should be included:\n{markdown}");
        assert!(!markdown.contains("interim in-progress guess"), "interim segment must be dropped:\n{markdown}");
    }

    #[test]
    fn empty_finals_render_a_placeholder_line() {
        let segments = vec![seg(Some(TrackKind::SelfMic), None, "not yet final", None, None, false)];
        let markdown = render_markdown(sample_started_at(), &segments, None);
        assert!(markdown.contains("(文字起こしはありません)"));
    }

    #[test]
    fn segment_with_timestamps_renders_a_bracketed_range_start() {
        let segments = vec![seg(Some(TrackKind::RemoteAudio), None, "hello there", Some(65_000), Some(67_000), true)];
        let markdown = render_markdown(sample_started_at(), &segments, None);
        assert!(markdown.contains("- **[01:05] 相手**: hello there"), "unexpected markdown:\n{markdown}");
    }

    #[test]
    fn segment_without_timestamp_omits_the_bracket_without_panicking() {
        let segments = vec![seg(Some(TrackKind::SelfMic), None, "no timing info", None, None, true)];
        let markdown = render_markdown(sample_started_at(), &segments, None);
        assert!(markdown.contains("- **自分**: no timing info"), "unexpected markdown:\n{markdown}");
        assert!(!markdown.contains("- **[]"));
    }

    #[test]
    fn diarized_speaker_label_is_reused_from_transcript_module() {
        let segments = vec![seg(Some(TrackKind::RemoteAudio), Some(1), "second speaker", Some(0), None, true)];
        let markdown = render_markdown(sample_started_at(), &segments, None);
        assert!(markdown.contains("相手 (話者2)"), "unexpected markdown:\n{markdown}");
    }

    #[test]
    fn summary_none_omits_summary_section() {
        let markdown = render_markdown(sample_started_at(), &[], None);
        assert!(!markdown.contains("## 要約"));
    }

    #[test]
    fn summary_some_renders_summary_section() {
        let markdown = render_markdown(sample_started_at(), &[], Some("要約本文です。"));
        assert!(markdown.contains("## 要約\n\n要約本文です。"));
    }
}
