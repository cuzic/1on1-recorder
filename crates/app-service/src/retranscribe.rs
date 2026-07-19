//! Manual re-transcription of a recorded `transcription_gaps` outage (task #91):
//! stitches together `segment-store`'s audio decode (#88), `stt-api`'s
//! `BatchSttProvider` + `stt-deepgram`'s batch adapter (#89), and
//! `session-store`'s gap bookkeeping (#90) into the one operation
//! `apps/desktop`'s future re-transcription UI (task #92) needs: given a closed
//! [`TranscriptionGap`], re-run STT over the `segments` already committed for
//! that stretch and persist the result.
//!
//! Gated behind the `live-transcription` feature (see this crate's `Cargo.toml`)
//! for the same reason `live_transcription` itself is: `stt-api`/`stt-deepgram`
//! are optional dependencies only pulled in by that feature. Unlike
//! `live_transcription::run_live_transcription`, this module has no "feature
//! disabled" stub — there's no meaningful fallback for "re-transcribe some audio"
//! when the STT crates aren't even compiled in, so a caller that needs this
//! either builds with `live-transcription` or doesn't call it at all.

use std::path::Path;

use credential_store::CredentialStore;
use recorder_domain::{AudioSegment, SessionId, TrackKind};
use segment_store::SAMPLE_RATE_HZ;
use session_store::{SessionStore, TranscriptSegment, TranscriptionGap};
use stt_api::{BatchAudioInput, BatchSttProvider, BatchTranscript, SttSessionConfig, Word};
use stt_deepgram::DeepgramBatchProvider;

use crate::stt_provider_kind::SttProviderKind;

/// Errors from [`retranscribe_gap`]. Distinct from `stt_api::SttError` (a
/// failure *within* an already-built batch call, wrapped via [`Self::Stt`]) and
/// from `live_transcription::SttProviderFactoryError` (that one has no
/// `UnsupportedProvider` case, since every [`SttProviderKind`] variant already
/// implements the streaming `SttProvider` — only some implement
/// [`BatchSttProvider`]).
#[derive(Debug, thiserror::Error)]
pub enum RetranscribeError {
    /// `gap.end_ms` is `None` — the outage hasn't closed yet (see
    /// `SessionStore::record_gap_end`'s doc comment: expected to be rare in
    /// practice, but `gaps_for_session` doesn't guarantee it), so there is no
    /// bounded audio range to re-transcribe yet.
    #[error("gap {gap_id} on session {session_id} is still open (no end_ms)")]
    GapStillOpen { gap_id: i64, session_id: SessionId },

    /// `kind` has no [`BatchSttProvider`] adapter yet (task #93 tracks adding
    /// more providers). Its own variant rather than folded into
    /// `SttError::PermanentError`, so a caller (i.e. `apps/desktop`'s future
    /// re-transcription button) can show "このプロバイダでは再文字起こしに未対応
    /// です" by matching on this case, without string-matching an error message.
    #[error("STT provider {kind:?} does not support batch re-transcription yet")]
    UnsupportedProvider { kind: SttProviderKind },

    #[error("no credential configured for STT provider {kind:?}: {source}")]
    CredentialMissing {
        kind: SttProviderKind,
        #[source]
        source: credential_store::StoreError,
    },

    /// No committed `segments` row overlaps `[gap.start_ms, gap.end_ms)` on
    /// `gap.track` — the gap was recorded but the audio for it was never
    /// committed (e.g. the recording crashed before `commit_segment` ran for
    /// that stretch), so there is nothing to decode.
    #[error("no recorded audio segments overlap [{start_ms}, {end_ms}) on track {track:?} for session {session_id}")]
    NoAudioInRange { session_id: SessionId, track: TrackKind, start_ms: u64, end_ms: u64 },

    #[error("segment decode error: {0}")]
    SegmentStore(#[from] segment_store::SegmentStoreError),

    #[error("batch STT call failed: {0}")]
    Stt(#[from] stt_api::SttError),

    #[error("session-store error: {0}")]
    SessionStore(#[from] session_store::StoreError),
}

/// Whether `kind` has a [`BatchSttProvider`] adapter (task #91) — the same
/// question `build_batch_stt_provider`'s match answers, but callable without a
/// `CredentialStore` for a caller that needs to know *before* attempting a
/// call (`apps/desktop`'s re-transcription button, task #92: it shows or hides
/// itself based on this rather than always rendering and only discovering
/// [`RetranscribeError::UnsupportedProvider`] after the user clicks). Every
/// [`SttProviderKind`] variant is listed explicitly, same rationale as
/// `build_batch_stt_provider`: a fifth adapter's enum variant added without a
/// matching arm here fails to compile instead of silently reporting `false`
/// for something that was actually meant to be supported.
pub fn supports_batch_retranscription(kind: SttProviderKind) -> bool {
    match kind {
        SttProviderKind::Deepgram => true,
        SttProviderKind::OpenAi | SttProviderKind::Google | SttProviderKind::AssemblyAi => false,
    }
}

/// Constructs the `Box<dyn BatchSttProvider>` for `kind`, loading whatever
/// credential it needs from `credential_store` — the batch-transcription
/// counterpart of `live_transcription::build_stt_provider`. Every
/// [`SttProviderKind`] variant is listed explicitly (no `_ => ...` catch-all) so
/// a fifth adapter's enum variant added without a matching arm here fails to
/// compile instead of silently reporting `UnsupportedProvider` for something
/// that was actually meant to be supported. Only Deepgram has a
/// [`BatchSttProvider`] adapter today (task #89) — OpenAI/Google/AssemblyAI are
/// tracked as a backlog item (task #93), not attempted here.
fn build_batch_stt_provider(kind: SttProviderKind, credential_store: &dyn CredentialStore) -> Result<Box<dyn BatchSttProvider>, RetranscribeError> {
    match kind {
        SttProviderKind::Deepgram => {
            let api_key = credential_store
                .load(stt_deepgram::CREDENTIAL_SERVICE, stt_deepgram::DEEPGRAM_API_KEY_ACCOUNT)
                .map_err(|source| RetranscribeError::CredentialMissing { kind, source })?;
            Ok(Box::new(DeepgramBatchProvider::new(api_key)))
        }
        SttProviderKind::OpenAi | SttProviderKind::Google | SttProviderKind::AssemblyAi => Err(RetranscribeError::UnsupportedProvider { kind }),
    }
}

/// Segments on `track` whose captured span overlaps `[start_ms, end_ms)`, in
/// sequence order (`segments` is already returned that way by
/// `SessionStore::segments_for_track`). A half-open-interval overlap test — a
/// segment is included as soon as any part of it falls inside the gap — rather
/// than an exact-millisecond containment check, matching `TranscriptionGap`'s
/// own doc comment that it only needs to point at "roughly the right stretch"
/// of `segments`: the re-transcribed audio may run slightly longer than the
/// gap itself at either edge, which is preferable to risking clipping real
/// speech right at the boundary.
fn segments_overlapping(segments: &[AudioSegment], start_ms: u64, end_ms: u64) -> Vec<&AudioSegment> {
    segments
        .iter()
        .filter(|seg| {
            let seg_start = seg.timeline_start_ms;
            let seg_end = seg.timeline_start_ms + seg.duration_ms as u64;
            seg_start < end_ms && seg_end > start_ms
        })
        .collect()
}

/// Builds one `TranscriptSegment` from a contiguous same-speaker run of
/// `words` — `start_ms`/`end_ms` come from the run's first/last word, offset by
/// `audio_start_ms` (the decoded PCM buffer's own absolute position, i.e. the
/// first overlapping segment's `timeline_start_ms` — see
/// `retranscribe_gap_with_provider`), since `Word::start_ms`/`end_ms` from
/// [`BatchSttProvider::transcribe_batch`] are relative to sample 0 of the audio
/// handed to it, not to the recording's own timeline. Falls back to
/// `audio_start_ms` unmodified if a word is missing its own timestamp.
fn turn_to_segment(session_id: SessionId, track: TrackKind, speaker: Option<u32>, words: &[&Word], audio_start_ms: u64) -> TranscriptSegment {
    let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
    let start_ms = words.first().and_then(|w| w.start_ms).map(|ms| audio_start_ms + ms);
    let end_ms = words.last().and_then(|w| w.end_ms).map(|ms| audio_start_ms + ms);
    TranscriptSegment { session_id, track: Some(track), speaker, text, start_ms, end_ms, is_final: true, is_retranscribed: true }
}

/// Converts one [`BatchTranscript`] into the `TranscriptSegment` row(s) to
/// persist for it — every row gets `is_retranscribed: true` (task #91's
/// marking requirement, see `TranscriptSegment::is_retranscribed`'s doc
/// comment). Splits `words` into contiguous same-speaker runs (rather than one
/// row for the whole gap) so a diarized batch result stays comparable to how
/// `live_transcription::persist_event` already stores one row per speaker turn
/// — a single row spanning several speakers would make `TranscriptSegment::
/// speaker`'s "the" speaker for that row meaningless. Falls back to one row
/// covering `[gap_start_ms, gap_end_ms)` when the provider returned no
/// word-level timestamps at all (`transcript.words` is `None`/empty) — the
/// gap's own bounds are the best available range in that case, since there is
/// no per-word timestamp to offset by `audio_start_ms` instead.
fn transcript_to_segments(
    session_id: SessionId,
    track: TrackKind,
    audio_start_ms: u64,
    gap_start_ms: u64,
    gap_end_ms: u64,
    transcript: BatchTranscript,
) -> Vec<TranscriptSegment> {
    let Some(words) = transcript.words.filter(|words| !words.is_empty()) else {
        return vec![TranscriptSegment {
            session_id,
            track: Some(track),
            speaker: None,
            text: transcript.text,
            start_ms: Some(gap_start_ms),
            end_ms: Some(gap_end_ms),
            is_final: true,
            is_retranscribed: true,
        }];
    };

    let mut segments = Vec::new();
    let mut current_speaker = words[0].speaker;
    let mut current_words: Vec<&Word> = Vec::new();
    for word in &words {
        if word.speaker != current_speaker && !current_words.is_empty() {
            segments.push(turn_to_segment(session_id, track, current_speaker, &current_words, audio_start_ms));
            current_words.clear();
        }
        current_speaker = word.speaker;
        current_words.push(word);
    }
    if !current_words.is_empty() {
        segments.push(turn_to_segment(session_id, track, current_speaker, &current_words, audio_start_ms));
    }
    segments
}

/// Re-transcribes one closed [`TranscriptionGap`] (task #91): re-decodes the
/// `segments` already committed for `gap.track` over `[gap.start_ms,
/// gap.end_ms)`, runs them through `provider_kind`'s [`BatchSttProvider`] (if
/// it has one — see [`RetranscribeError::UnsupportedProvider`]), and persists
/// the result as `is_retranscribed: true` `TranscriptSegment` rows.
///
/// `provider_kind` is, in practice, expected to be the *currently* selected
/// provider (the same `credential_store.load(CREDENTIAL_SERVICE,
/// SELECTED_STT_PROVIDER_ACCOUNT)` read `live_transcription::
/// run_live_transcription` does at connect time) — not necessarily whichever
/// provider was actually connected when the gap itself was recorded: nothing in
/// this schema tracks "which provider transcribed this session" per session,
/// only implicitly per live event as it streamed in, so there is no historical
/// record to look up here even in principle. In the common case (the user
/// hasn't changed their STT provider selection since the recording) these are
/// the same provider anyway; if they have, this re-transcribes with whatever is
/// configured *now*, which the caller is expected to make clear to the user
/// (task #92).
///
/// On success, deletes `gap` from `transcription_gaps` via
/// [`SessionStore::discard_gap`] — treating "someone already re-transcribed
/// this outage" the same way `live_transcription` treats "the outage turned out
/// too short to matter" (see that module's `MIN_RECORDED_GAP_MS`): the audio
/// and its now-persisted `TranscriptSegment` rows are the durable record, so
/// leaving a resolved gap row behind would only make `gaps_for_session` keep
/// showing a "needs re-transcription" entry for something already fixed. A
/// failure partway through (decode error, provider error, persistence error)
/// leaves the gap row untouched, so a retry has the same `gaps_for_session`
/// entry to work from. **Known limitation**: if persistence succeeds but the
/// final `discard_gap` call itself fails, a retry will re-insert duplicate
/// `TranscriptSegment` rows for the same span — `transcript_segments` has no
/// natural dedup key for this (unlike `segments`' `(session_id, track,
/// sequence)` primary key), so this is left as a known gap rather than solved
/// here.
pub async fn retranscribe_gap(
    gap: TranscriptionGap,
    provider_kind: SttProviderKind,
    store: &SessionStore,
    credential_store: &dyn CredentialStore,
) -> Result<Vec<TranscriptSegment>, RetranscribeError> {
    let provider = build_batch_stt_provider(provider_kind, credential_store)?;
    retranscribe_gap_with_provider(gap, provider.as_ref(), store).await
}

/// The provider-agnostic core of [`retranscribe_gap`], split out so tests can
/// exercise it against a mock [`BatchSttProvider`] instead of a real network
/// call — `retranscribe_gap` itself only adds `build_batch_stt_provider`'s
/// credential-loading step on top.
async fn retranscribe_gap_with_provider(
    gap: TranscriptionGap,
    provider: &dyn BatchSttProvider,
    store: &SessionStore,
) -> Result<Vec<TranscriptSegment>, RetranscribeError> {
    let Some(gap_end_ms) = gap.end_ms else {
        return Err(RetranscribeError::GapStillOpen { gap_id: gap.id, session_id: gap.session_id });
    };

    let all_segments = store.segments_for_track(gap.session_id, gap.track)?;
    let overlapping = segments_overlapping(&all_segments, gap.start_ms, gap_end_ms);
    if overlapping.is_empty() {
        return Err(RetranscribeError::NoAudioInRange { session_id: gap.session_id, track: gap.track, start_ms: gap.start_ms, end_ms: gap_end_ms });
    }
    // `overlapping` inherits `all_segments`' sequence order (see
    // `segments_overlapping`'s doc comment), so its first entry is the earliest
    // segment decoded into the PCM buffer below — its `timeline_start_ms` is
    // therefore the buffer's own absolute position, needed to offset the
    // provider's buffer-relative word timestamps back to the recording's
    // timeline (see `turn_to_segment`).
    let audio_start_ms = overlapping[0].timeline_start_ms;
    let paths: Vec<&Path> = overlapping.iter().map(|seg| seg.local_path.as_path()).collect();
    let pcm = segment_store::decode_segments_to_pcm(&paths)?;

    let audio = BatchAudioInput { pcm: &pcm, sample_rate_hz: SAMPLE_RATE_HZ, channels: 1 };
    let config = SttSessionConfig::new(SAMPLE_RATE_HZ).with_diarization(true);
    let transcript = provider.transcribe_batch(audio, config).await?;

    let segments = transcript_to_segments(gap.session_id, gap.track, audio_start_ms, gap.start_ms, gap_end_ms, transcript);
    for segment in &segments {
        store.insert_transcript_segment(segment)?;
    }

    store.discard_gap(gap.id)?;

    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use recorder_domain::{AudioManifest, CaptureManifest, ConsentManifest, RemoteSourceKind, SessionManifest};
    use segment_store::{commit_segment, encode_segment_to_ogg_opus, CrashPoint, SegmentRequest};
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn test_manifest(session_id: SessionId) -> SessionManifest {
        SessionManifest {
            schema_version: 1,
            session_id,
            started_at: Utc::now(),
            ended_at: None,
            platform: "linux".to_string(),
            app_version: "0.1.0".to_string(),
            capture: CaptureManifest {
                microphone_device_id: "mic-1".to_string(),
                remote_source_id: "speaker-1".to_string(),
                remote_source_kind: RemoteSourceKind::EndpointLoopback,
            },
            audio: AudioManifest { sample_rate: SAMPLE_RATE_HZ, segment_duration_ms: 1_000, tracks: vec![TrackKind::SelfMic, TrackKind::RemoteAudio] },
            consent: ConsentManifest { confirmed_by_user: true, confirmed_at: Utc::now() },
        }
    }

    fn sine_pcm(seconds: f32) -> Vec<f32> {
        let n = (seconds * SAMPLE_RATE_HZ as f32) as usize;
        (0..n).map(|i| 0.1 * (2.0 * std::f32::consts::PI * 440.0 * (i as f32 / SAMPLE_RATE_HZ as f32)).sin()).collect()
    }

    /// Commits one 1s Opus segment at `sequence` (`timeline_start_ms = sequence *
    /// 1000`) under `dir`, registering it with `store` — mirrors
    /// `segment-store/tests/atomic_commit.rs`'s own helper.
    fn commit_test_segment(dir: &std::path::Path, store: &SessionStore, session_id: SessionId, track: TrackKind, sequence: u64) {
        let pcm = sine_pcm(1.0);
        let encoded = encode_segment_to_ogg_opus(&pcm, 32_000).unwrap();
        let request = SegmentRequest { session_id, track, sequence, timeline_start_ms: sequence * 1_000, sample_rate: SAMPLE_RATE_HZ, channels: 1 };
        commit_segment(&encoded, dir, &request, store, CrashPoint::None).unwrap().unwrap();
    }

    fn word(text: &str, start_ms: u64, end_ms: u64, speaker: u32) -> Word {
        Word { text: text.to_string(), start_ms: Some(start_ms), end_ms: Some(end_ms), confidence: Some(0.9), speaker: Some(speaker) }
    }

    #[test]
    fn segments_overlapping_keeps_only_segments_touching_the_range() {
        let session_id = SessionId::new();
        let seg = |sequence: u64| AudioSegment {
            session_id,
            track: TrackKind::SelfMic,
            sequence,
            timeline_start_ms: sequence * 1_000,
            duration_ms: 1_000,
            codec: recorder_domain::AudioCodec::Opus,
            sample_rate: SAMPLE_RATE_HZ,
            channels: 1,
            sha256: "deadbeef".to_string(),
            local_path: std::path::PathBuf::from(format!("{sequence}.opus")),
            byte_len: 10,
        };
        let segments = vec![seg(0), seg(1), seg(2)];

        // [500, 1500) touches segment 0 ([0,1000)) and segment 1 ([1000,2000)),
        // but not segment 2 ([2000,3000)).
        let overlapping = segments_overlapping(&segments, 500, 1_500);
        assert_eq!(overlapping.iter().map(|s| s.sequence).collect::<Vec<_>>(), vec![0, 1]);

        // An exactly-adjacent range touches nothing (half-open on both ends).
        assert!(segments_overlapping(&segments, 3_000, 4_000).is_empty());
    }

    #[test]
    fn transcript_to_segments_splits_contiguous_same_speaker_runs() {
        let session_id = SessionId::new();
        let transcript = BatchTranscript::new("hello world").with_words(vec![
            word("hello", 100, 400, 0),
            word("world", 1_200, 1_600, 1),
        ]);

        let segments = transcript_to_segments(session_id, TrackKind::RemoteAudio, 0, 0, 2_000, transcript);

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].speaker, Some(0));
        assert_eq!(segments[0].text, "hello");
        assert_eq!(segments[0].start_ms, Some(100));
        assert_eq!(segments[0].end_ms, Some(400));
        assert_eq!(segments[1].speaker, Some(1));
        assert_eq!(segments[1].text, "world");
        assert!(segments.iter().all(|s| s.is_retranscribed && s.is_final));
    }

    #[test]
    fn transcript_to_segments_merges_words_from_the_same_speaker() {
        let session_id = SessionId::new();
        let transcript = BatchTranscript::new("hello there").with_words(vec![
            word("hello", 0, 300, 0),
            word("there", 300, 600, 0),
        ]);

        let segments = transcript_to_segments(session_id, TrackKind::SelfMic, 0, 0, 1_000, transcript);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "hello there");
        assert_eq!(segments[0].start_ms, Some(0));
        assert_eq!(segments[0].end_ms, Some(600));
    }

    #[test]
    fn transcript_to_segments_offsets_by_the_decoded_buffer_start() {
        let session_id = SessionId::new();
        let transcript = BatchTranscript::new("hi").with_words(vec![word("hi", 100, 200, 0)]);

        // audio_start_ms = 5_000: the decoded buffer began at 5s on the
        // recording's own timeline, so a word at buffer-relative 100ms lands at
        // 5_100ms absolute.
        let segments = transcript_to_segments(session_id, TrackKind::SelfMic, 5_000, 5_000, 6_000, transcript);
        assert_eq!(segments[0].start_ms, Some(5_100));
        assert_eq!(segments[0].end_ms, Some(5_200));
    }

    #[test]
    fn transcript_to_segments_falls_back_to_gap_bounds_without_word_timestamps() {
        let session_id = SessionId::new();
        let transcript = BatchTranscript::new("no words here");

        let segments = transcript_to_segments(session_id, TrackKind::RemoteAudio, 500, 1_000, 3_000, transcript);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].speaker, None);
        assert_eq!(segments[0].text, "no words here");
        assert_eq!(segments[0].start_ms, Some(1_000));
        assert_eq!(segments[0].end_ms, Some(3_000));
    }

    /// Minimal in-memory `CredentialStore` — same shape as
    /// `live_transcription`'s own test double (kept local since no shared
    /// test-only crate exposes one; see that module's doc comment).
    struct InMemoryCredentialStore(HashMap<(String, String), String>);

    impl InMemoryCredentialStore {
        fn empty() -> Self {
            Self(HashMap::new())
        }
    }

    impl CredentialStore for InMemoryCredentialStore {
        fn save(&self, _service: &str, _account: &str, _secret: &str) -> Result<(), credential_store::StoreError> {
            unimplemented!("not needed by these tests")
        }
        fn load(&self, service: &str, account: &str) -> Result<String, credential_store::StoreError> {
            self.0.get(&(service.to_string(), account.to_string())).cloned().ok_or_else(|| credential_store::StoreError::NotFound {
                service: service.to_string(),
                account: account.to_string(),
            })
        }
        fn delete(&self, _service: &str, _account: &str) -> Result<(), credential_store::StoreError> {
            unimplemented!("not needed by these tests")
        }
    }

    #[test]
    fn supports_batch_retranscription_agrees_with_build_batch_stt_provider() {
        // `supports_batch_retranscription` is meant to predict
        // `build_batch_stt_provider`'s outcome (task #92's button visibility
        // check) without a `CredentialStore` — assert the two never disagree,
        // using an empty store so `build_batch_stt_provider`'s only possible
        // outcomes are `UnsupportedProvider` (kind not supported) or
        // `CredentialMissing` (kind supported, just no credential loaded here).
        let store = InMemoryCredentialStore::empty();
        for kind in [SttProviderKind::Deepgram, SttProviderKind::OpenAi, SttProviderKind::Google, SttProviderKind::AssemblyAi] {
            let build_result = build_batch_stt_provider(kind, &store);
            match (supports_batch_retranscription(kind), build_result) {
                (true, Err(RetranscribeError::UnsupportedProvider { .. })) => panic!("{kind:?} reported supported but build_batch_stt_provider disagreed"),
                (false, Ok(_)) => panic!("{kind:?} reported unsupported but build_batch_stt_provider built a provider"),
                _ => {}
            }
        }
    }

    #[test]
    fn build_batch_stt_provider_reports_unsupported_for_every_non_deepgram_kind() {
        let store = InMemoryCredentialStore::empty();
        for kind in [SttProviderKind::OpenAi, SttProviderKind::Google, SttProviderKind::AssemblyAi] {
            let Err(err) = build_batch_stt_provider(kind, &store) else { panic!("expected UnsupportedProvider for {kind:?}") };
            assert!(matches!(err, RetranscribeError::UnsupportedProvider { kind: k } if k == kind));
        }
    }

    #[test]
    fn build_batch_stt_provider_reports_credential_missing_for_deepgram_when_store_is_empty() {
        let store = InMemoryCredentialStore::empty();
        let Err(err) = build_batch_stt_provider(SttProviderKind::Deepgram, &store) else { panic!("expected CredentialMissing") };
        assert!(matches!(err, RetranscribeError::CredentialMissing { kind: SttProviderKind::Deepgram, .. }));
    }

    /// Records every `transcribe_batch` call's PCM length and returns a canned
    /// two-speaker transcript — enough to exercise `retranscribe_gap_with_provider`
    /// end to end without a real Deepgram connection (mirrors
    /// `stt-deepgram/src/batch.rs`'s own mock-server tests, but at the
    /// `BatchSttProvider` trait boundary instead of HTTP, since app-service has
    /// no reason to hardcode Deepgram's wire format).
    struct MockBatchProvider {
        last_pcm_len: Mutex<Option<usize>>,
    }

    impl MockBatchProvider {
        fn new() -> Self {
            Self { last_pcm_len: Mutex::new(None) }
        }
    }

    #[async_trait]
    impl BatchSttProvider for MockBatchProvider {
        async fn transcribe_batch(&self, audio: BatchAudioInput<'_>, _config: SttSessionConfig) -> Result<BatchTranscript, stt_api::SttError> {
            *self.last_pcm_len.lock().unwrap() = Some(audio.pcm.len());
            Ok(BatchTranscript::new("hello world").with_words(vec![word("hello", 100, 400, 0), word("world", 1_200, 1_600, 1)]))
        }
    }

    #[tokio::test]
    async fn retranscribe_gap_with_provider_persists_segments_and_discards_the_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::open_in_memory().unwrap();
        let session_id = SessionId::new();
        store.create_session(&test_manifest(session_id)).unwrap();

        // Two 1s segments covering [0, 2000) on RemoteAudio.
        commit_test_segment(tmp.path(), &store, session_id, TrackKind::RemoteAudio, 0);
        commit_test_segment(tmp.path(), &store, session_id, TrackKind::RemoteAudio, 1);

        let gap_id = store.record_gap_start(session_id, TrackKind::RemoteAudio, 500).unwrap();
        store.record_gap_end(gap_id, 1_500).unwrap();
        let gap = store.gaps_for_session(session_id).unwrap().into_iter().find(|g| g.id == gap_id).unwrap();

        let provider = MockBatchProvider::new();
        let segments = retranscribe_gap_with_provider(gap, &provider, &store).await.unwrap();

        assert_eq!(segments.len(), 2);
        assert!(segments.iter().all(|s| s.is_retranscribed));
        assert_eq!(segments[0].start_ms, Some(100));
        assert_eq!(segments[1].start_ms, Some(1_200));

        let persisted = store.list_transcript_segments(session_id).unwrap();
        assert_eq!(persisted.len(), 2);
        assert!(persisted.iter().all(|s| s.is_retranscribed && s.is_final));

        // The gap is resolved, not left around for `gaps_for_session` to keep
        // surfacing (see `retranscribe_gap_with_provider`'s doc comment).
        assert!(store.gaps_for_session(session_id).unwrap().is_empty());

        // 2 committed 1s segments decoded whole -> 2s of PCM at 48kHz.
        assert_eq!(*provider.last_pcm_len.lock().unwrap(), Some(2 * SAMPLE_RATE_HZ as usize));
    }

    #[tokio::test]
    async fn retranscribe_gap_with_provider_rejects_a_still_open_gap() {
        let store = SessionStore::open_in_memory().unwrap();
        let session_id = SessionId::new();
        store.create_session(&test_manifest(session_id)).unwrap();
        let gap_id = store.record_gap_start(session_id, TrackKind::SelfMic, 0).unwrap();
        let gap = TranscriptionGap { id: gap_id, session_id, track: TrackKind::SelfMic, start_ms: 0, end_ms: None };

        let provider = MockBatchProvider::new();
        let err = retranscribe_gap_with_provider(gap, &provider, &store).await.unwrap_err();
        assert!(matches!(err, RetranscribeError::GapStillOpen { gap_id: id, .. } if id == gap_id));
    }

    #[tokio::test]
    async fn retranscribe_gap_with_provider_reports_no_audio_when_nothing_overlaps() {
        let store = SessionStore::open_in_memory().unwrap();
        let session_id = SessionId::new();
        store.create_session(&test_manifest(session_id)).unwrap();
        // No segments ever committed for this track.
        let gap = TranscriptionGap { id: 1, session_id, track: TrackKind::SelfMic, start_ms: 0, end_ms: Some(1_000) };

        let provider = MockBatchProvider::new();
        let err = retranscribe_gap_with_provider(gap, &provider, &store).await.unwrap_err();
        assert!(matches!(err, RetranscribeError::NoAudioInRange { track: TrackKind::SelfMic, start_ms: 0, end_ms: 1_000, .. }));
    }
}
