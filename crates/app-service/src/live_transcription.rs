//! Real-time transcription side channel (design's "録音中にリアルタイムで文字起こしを
//! 表示したい"): consumes the raw-PCM side channel `windows_frame_collector::collect_frames`
//! feeds (mirroring how `level_sink` is fed, for the same "cheap side channel, batch
//! `run_pipeline` stays untouched" reason — see that module's doc comment), streams it
//! into per-track sessions of whichever `stt-*` adapter the user selected (Deepgram,
//! OpenAI, Google, or AssemblyAI — see [`SttProviderKind`]/`stt_wiring::build_stt_provider`,
//! task #47/#48), resampling to that provider's required rate first (see
//! `crate::resample` and `stt_wiring::target_sample_rate_hz`), and persists every
//! `SttEvent::PartialTranscript`/`FinalTranscript` via
//! `SessionStore::insert_transcript_segment`.
//!
//! Gated behind the `live-transcription` feature (see `app-service`'s `Cargo.toml`):
//! without it, [`run_live_transcription`] below compiles to a stub that just drains
//! and discards `audio_rx`, so `windows_session::run_windows_capture_session` doesn't
//! need any `#[cfg]` of its own at the call site, and a plain `--features
//! windows-supervisor` build never pulls in any `stt-*` adapter's websocket/TLS/gRPC
//! stack.
//!
//! Like the rest of `windows_supervisor`/`windows_session`, never run against a real
//! STT provider connection or real Windows hardware in this environment —
//! cross-compile-checked only (see this crate's README).
//!
//! No macOS equivalent yet: `macos_frame_collector`/`macos_session` would need the
//! identical `stt_sink`/`run_live_transcription` wiring once `capture-macos` is ever
//! actually compiled/run (see that crate's own doc comment) — out of scope here since
//! macOS capture itself isn't in scope yet.

use std::sync::{Arc, Mutex};

use credential_store::CredentialStore;
use recorder_domain::{SessionId, TrackKind};
use session_store::SessionStore;
use tokio::sync::mpsc::UnboundedReceiver;

/// Per-track Deepgram connection status (task #52): the desktop UI can't otherwise
/// tell "nobody has spoken yet" from "STT is broken" when the transcript panel is
/// empty. Updated by [`run_live_transcription`] as each track's session state
/// changes, via the same `Arc<Mutex<_>>` side-channel shape as `LevelSnapshot` (see
/// `windows_frame_collector`). Two independent fields rather than one aggregate
/// status, since Self and Remote each open their own Deepgram session (see this
/// module's doc comment) and can fail independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackTranscriptionStatus {
    /// No Deepgram API key configured — a connection was never attempted.
    NotConfigured,
    /// `start_session` is in flight.
    Connecting,
    /// Session open and streaming.
    Connected,
    /// `start_session` failed, or the session reported an `SttEvent::Error` after
    /// connecting. Carries `SttError::to_string()` rather than the typed error
    /// itself, so this type — and the desktop crate's mirror of it (see
    /// `apps/desktop/src/transcription_status.rs`) — never needs an `stt-api`
    /// dependency.
    Error(String),
    /// `live-transcription` feature disabled, or the running platform has no live
    /// transcription wiring at all (e.g. macOS/dev builds — see this module's doc
    /// comment on scope).
    Unavailable,
}

impl Default for TrackTranscriptionStatus {
    /// Before the first update lands, "unknown yet" reads closer to `NotConfigured`
    /// than to claiming a connection is already in flight.
    fn default() -> Self {
        Self::NotConfigured
    }
}

#[derive(Debug, Clone, Default)]
pub struct TranscriptionStatus {
    pub self_status: TrackTranscriptionStatus,
    pub remote_status: TrackTranscriptionStatus,
}

fn set_status(sink: &Option<Arc<Mutex<TranscriptionStatus>>>, track: TrackKind, status: TrackTranscriptionStatus) {
    let Some(sink) = sink else { return };
    let mut guard = sink.lock().unwrap();
    match track {
        TrackKind::SelfMic => guard.self_status = status,
        TrackKind::RemoteAudio => guard.remote_status = status,
    }
}

fn set_both_status(sink: &Option<Arc<Mutex<TranscriptionStatus>>>, status: TrackTranscriptionStatus) {
    let Some(sink) = sink else { return };
    let mut guard = sink.lock().unwrap();
    guard.self_status = status.clone();
    guard.remote_status = status;
}

/// Re-exported from `crate::stt_provider_kind` (task #49) so existing
/// `live_transcription::{CREDENTIAL_SERVICE, SELECTED_STT_PROVIDER_ACCOUNT,
/// SttProviderKind}` paths keep working — that module is the canonical home now,
/// since it has no `windows-supervisor`-only dependencies and needs to be reachable
/// from every platform (e.g. `apps/desktop`'s settings screen).
pub use crate::stt_provider_kind::{SttProviderKind, CREDENTIAL_SERVICE, SELECTED_STT_PROVIDER_ACCOUNT};

#[cfg(feature = "live-transcription")]
mod stt_wiring {
    use super::*;
    use crate::resample::resample;
    use session_store::TranscriptSegment;
    use stt_api::{AudioChunk, SttEvent, SttProvider, SttSession, SttSessionConfig};
    use stt_assemblyai::AssemblyAIProvider;
    use stt_deepgram::DeepgramProvider;
    use stt_google::{GoogleProvider, GoogleSttCredentials};
    use stt_openai::OpenAiProvider;

    /// Errors from [`build_stt_provider`] — distinct from `stt_api::SttError`, which
    /// covers failures *within* an already-constructed provider's session, not
    /// "there is no provider to construct at all".
    #[derive(Debug, thiserror::Error)]
    pub enum SttProviderFactoryError {
        #[error("no credential configured for STT provider {kind:?}: {source}")]
        CredentialMissing {
            kind: SttProviderKind,
            #[source]
            source: credential_store::StoreError,
        },
        /// Google's stored credential is a JSON blob (see [`GoogleSttCredentials`]
        /// and this module's `build_stt_provider` arm for `SttProviderKind::Google`),
        /// not a bare string like the other three providers' — so unlike
        /// `CredentialMissing`, this is reachable even once *something* is stored
        /// under the account, if that something isn't valid JSON for the shape.
        #[error("stored credential for STT provider {kind:?} is not valid: {source}")]
        InvalidCredential {
            kind: SttProviderKind,
            #[source]
            source: serde_json::Error,
        },
    }

    /// Constructs the `Box<dyn SttProvider>` for `kind`, loading whatever credential
    /// that provider needs from `credential_store`. Every [`SttProviderKind`] variant
    /// is listed explicitly (no `_ => ...` catch-all) so adding a fifth adapter's enum
    /// variant without a matching arm here fails to compile instead of silently
    /// falling through.
    pub fn build_stt_provider(kind: SttProviderKind, credential_store: &dyn CredentialStore) -> Result<Box<dyn SttProvider>, SttProviderFactoryError> {
        match kind {
            SttProviderKind::Deepgram => {
                let api_key = credential_store
                    .load(stt_deepgram::CREDENTIAL_SERVICE, stt_deepgram::DEEPGRAM_API_KEY_ACCOUNT)
                    .map_err(|source| SttProviderFactoryError::CredentialMissing { kind, source })?;
                Ok(Box::new(DeepgramProvider::new(api_key)))
            }
            SttProviderKind::OpenAi => {
                let api_key = credential_store
                    .load(stt_openai::CREDENTIAL_SERVICE, stt_openai::OPENAI_STT_API_KEY_ACCOUNT)
                    .map_err(|source| SttProviderFactoryError::CredentialMissing { kind, source })?;
                Ok(Box::new(OpenAiProvider::new(api_key)))
            }
            SttProviderKind::Google => {
                let raw = credential_store
                    .load(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT)
                    .map_err(|source| SttProviderFactoryError::CredentialMissing { kind, source })?;
                let credentials: GoogleSttCredentials =
                    serde_json::from_str(&raw).map_err(|source| SttProviderFactoryError::InvalidCredential { kind, source })?;
                Ok(Box::new(GoogleProvider::new(credentials)))
            }
            SttProviderKind::AssemblyAi => {
                let api_key = credential_store
                    .load(stt_assemblyai::CREDENTIAL_SERVICE, stt_assemblyai::ASSEMBLYAI_API_KEY_ACCOUNT)
                    .map_err(|source| SttProviderFactoryError::CredentialMissing { kind, source })?;
                Ok(Box::new(AssemblyAIProvider::new(api_key)))
            }
        }
    }

    /// Sample rate each provider's session must be opened at (task #48):
    /// `stt-openai` requires exactly 24kHz and `stt-assemblyai` requires exactly
    /// 16kHz mono PCM16, both hard-rejecting any other rate at `start_session` (see
    /// each crate's module doc comment) — `capture_rate_hz` audio is resampled down
    /// to these before `send_audio` (see `run_live_transcription` below).
    /// `stt-deepgram` accepts whatever rate it's given and `stt-google` only requires
    /// a nonzero one, so for those the capture pipeline's own nominal rate is used
    /// unchanged, avoiding a needless resample. Centralized here — one place to
    /// update if a provider's required rate ever changes — rather than duplicated at
    /// each call site.
    fn target_sample_rate_hz(kind: SttProviderKind, capture_rate_hz: u32) -> u32 {
        match kind {
            SttProviderKind::OpenAi => 24_000,
            SttProviderKind::AssemblyAi => 16_000,
            SttProviderKind::Deepgram | SttProviderKind::Google => capture_rate_hz,
        }
    }

    /// Runs for the lifetime of one recording session, ending when `audio_rx` closes
    /// (i.e. `windows_session::run_capture_blocking`'s collector thread — and the
    /// `stt_tx` it owns — has finished, meaning capture is fully done). Failing to
    /// obtain a Deepgram session at all (no credential configured, auth failure,
    /// connect failure) is logged and simply means no live transcription for that
    /// track — same "failure doesn't take down the whole pipeline" spirit as
    /// `upload_worker`'s retry handling; the batch `run_pipeline` recording itself is
    /// unaffected either way. `status_sink`, if given, is kept up to date with each
    /// track's connection state for #52's UI visibility — `None` is fine, it's just
    /// a side channel, mirroring `level_sink`/`stt_sink` elsewhere in this crate.
    pub async fn run_live_transcription(
        session_id: SessionId,
        sample_rate_hz: u32,
        credential_store: Option<Arc<dyn CredentialStore + Send + Sync>>,
        mut audio_rx: UnboundedReceiver<(TrackKind, Vec<f32>, u32)>,
        store: &SessionStore,
        status_sink: Option<Arc<Mutex<TranscriptionStatus>>>,
    ) {
        let Some(credential_store) = credential_store else {
            tracing::debug!("live transcription: no credential store configured, skipping");
            set_both_status(&status_sink, TrackTranscriptionStatus::NotConfigured);
            drain(&mut audio_rx).await;
            return;
        };

        let selected_kind = credential_store
            .load(CREDENTIAL_SERVICE, SELECTED_STT_PROVIDER_ACCOUNT)
            .ok()
            .and_then(|value| SttProviderKind::from_account_value(&value))
            .unwrap_or_default();

        let provider = match build_stt_provider(selected_kind, credential_store.as_ref()) {
            Ok(provider) => provider,
            Err(err) => {
                tracing::info!(%err, ?selected_kind, "live transcription: no STT provider available, skipping");
                set_both_status(&status_sink, TrackTranscriptionStatus::NotConfigured);
                drain(&mut audio_rx).await;
                return;
            }
        };
        // Phase 1A capture is a fixed format (design.md; see
        // `windows_frame_collector`'s "falls back to 48kHz mono" comment) — the
        // session is opened once, up front, at the target rate rather than
        // per-chunk, since a provider's session config (like every provider's) is
        // fixed for the connection's lifetime. `target_rate_hz` may differ from
        // `sample_rate_hz` (the capture pipeline's own rate) for providers that
        // require a fixed rate (see `target_sample_rate_hz`); each incoming chunk is
        // resampled to it below, right before `send_audio`.
        // Diarization is additional info within a single track (e.g. multiple people
        // on the Remote track in a group call) — the app's primary speaker split is
        // still Self/Remote (see this module's doc comment), not a provider's own
        // `speaker` index alone.
        let target_rate_hz = target_sample_rate_hz(selected_kind, sample_rate_hz);
        let config = SttSessionConfig::new(target_rate_hz).with_interim_results(true).with_vad_events(true).with_diarization(true);

        set_both_status(&status_sink, TrackTranscriptionStatus::Connecting);

        let self_sess = match provider.start_session(config.clone()).await {
            Ok((session, events)) => {
                set_status(&status_sink, TrackKind::SelfMic, TrackTranscriptionStatus::Connected);
                Some((session, events))
            }
            Err(err) => {
                tracing::warn!(%err, track = "self", "live transcription: failed to start STT session");
                set_status(&status_sink, TrackKind::SelfMic, TrackTranscriptionStatus::Error(err.to_string()));
                None
            }
        };
        let remote_sess = match provider.start_session(config).await {
            Ok((session, events)) => {
                set_status(&status_sink, TrackKind::RemoteAudio, TrackTranscriptionStatus::Connected);
                Some((session, events))
            }
            Err(err) => {
                tracing::warn!(%err, track = "remote", "live transcription: failed to start STT session");
                set_status(&status_sink, TrackKind::RemoteAudio, TrackTranscriptionStatus::Error(err.to_string()));
                None
            }
        };

        if self_sess.is_none() && remote_sess.is_none() {
            drain(&mut audio_rx).await;
            return;
        }

        let (mut self_session, mut self_events) = split(self_sess);
        let (mut remote_session, mut remote_events) = split(remote_sess);

        let mut self_samples_sent: u64 = 0;
        let mut remote_samples_sent: u64 = 0;
        let mut audio_open = true;

        // #56: the last still-unconfirmed `PartialTranscript` seen per track, cleared
        // every time a `FinalTranscript` supersedes it. If a track's event channel
        // closes (Deepgram disconnected) while one of these is still set, no final
        // ever arrived for that stretch of speech and it would otherwise vanish
        // entirely from `transcript::to_turns`' summary input (which only reads
        // `is_final` rows) — see this function's tail below.
        let mut self_last_interim: Option<PendingInterim> = None;
        let mut remote_last_interim: Option<PendingInterim> = None;

        while audio_open || self_events.is_some() || remote_events.is_some() {
            tokio::select! {
                maybe = audio_rx.recv(), if audio_open => {
                    match maybe {
                        Some((track, samples, chunk_rate_hz)) => {
                            let (session, samples_sent) = match track {
                                TrackKind::SelfMic => (self_session.as_mut(), &mut self_samples_sent),
                                TrackKind::RemoteAudio => (remote_session.as_mut(), &mut remote_samples_sent),
                            };
                            if let Some(session) = session {
                                // `AudioChunk::start_sample` is documented as "at
                                // `SttSessionConfig::sample_rate_hz`" — i.e. the
                                // *target* rate's sample count, so resample happens
                                // before the counter advances, not after.
                                let resampled = resample(&samples, chunk_rate_hz, target_rate_hz);
                                let chunk = AudioChunk { pcm: &resampled, start_sample: *samples_sent };
                                *samples_sent += resampled.len() as u64;
                                if let Err(err) = session.send_audio(chunk).await {
                                    tracing::warn!(%err, ?track, "live transcription: send_audio failed");
                                }
                            }
                        }
                        None => {
                            audio_open = false;
                            if let Some(session) = self_session.take() {
                                if let Err(err) = session.finalize().await {
                                    tracing::warn!(%err, track = "self", "live transcription: failed to finalize STT session");
                                }
                            }
                            if let Some(session) = remote_session.take() {
                                if let Err(err) = session.finalize().await {
                                    tracing::warn!(%err, track = "remote", "live transcription: failed to finalize STT session");
                                }
                            }
                        }
                    }
                }
                maybe = recv_track_event(&mut self_events) => {
                    match maybe {
                        Some(event) => {
                            note_event(&mut self_last_interim, &event);
                            if let SttEvent::Error(err) = &event {
                                set_status(&status_sink, TrackKind::SelfMic, TrackTranscriptionStatus::Error(err.to_string()));
                            }
                            persist_event(store, session_id, Some(TrackKind::SelfMic), event);
                        }
                        None => {
                            self_events = None;
                            persist_pending_interim(store, session_id, TrackKind::SelfMic, self_last_interim.take());
                        }
                    }
                }
                maybe = recv_track_event(&mut remote_events) => {
                    match maybe {
                        Some(event) => {
                            note_event(&mut remote_last_interim, &event);
                            if let SttEvent::Error(err) = &event {
                                set_status(&status_sink, TrackKind::RemoteAudio, TrackTranscriptionStatus::Error(err.to_string()));
                            }
                            persist_event(store, session_id, Some(TrackKind::RemoteAudio), event);
                        }
                        None => {
                            remote_events = None;
                            persist_pending_interim(store, session_id, TrackKind::RemoteAudio, remote_last_interim.take());
                        }
                    }
                }
            }
        }
    }

    /// Awaits the next event for a track whose events channel may already be
    /// `None` (that track's session never started, or already closed). Used
    /// instead of `events.as_mut().unwrap().recv()` behind a `select!` `if`
    /// guard: `tokio::select!` still evaluates a disabled branch's async
    /// expression (just doesn't poll it — see the `select!` macro docs), so an
    /// `unwrap()` there would panic as soon as the *other* track's branch stays
    /// enabled while this one is `None`. Awaiting `pending()` for the `None` case
    /// instead means the branch simply never becomes ready, with no unwrap.
    async fn recv_track_event(events: &mut Option<UnboundedReceiver<SttEvent>>) -> Option<SttEvent> {
        match events {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Splits `SttProvider::start_session`'s `(Box<dyn SttSession>, UnboundedReceiver<SttEvent>)`
    /// pair (or nothing, if that track's session never started) into independently
    /// tracked `Option`s, so the send-audio and event-draining halves below can each
    /// hold/clear their own half without fighting over one combined `Option`.
    #[allow(clippy::type_complexity)]
    fn split(
        session: Option<(Box<dyn SttSession>, UnboundedReceiver<SttEvent>)>,
    ) -> (Option<Box<dyn SttSession>>, Option<UnboundedReceiver<SttEvent>>) {
        match session {
            Some((session, events)) => (Some(session), Some(events)),
            None => (None, None),
        }
    }

    /// The bits of a `PartialTranscript` needed to persist it as a fallback final
    /// (#56) — just what `TranscriptSegment` needs, not the full `SttEvent`.
    struct PendingInterim {
        text: String,
        audio_start_ms: Option<u64>,
        audio_end_ms: Option<u64>,
    }

    /// Tracks the most recent not-yet-finalized `PartialTranscript` per track:
    /// records it on every partial, clears it on every final (a final always
    /// supersedes the partials leading up to it).
    fn note_event(last_interim: &mut Option<PendingInterim>, event: &SttEvent) {
        match event {
            SttEvent::PartialTranscript { text, audio_start_ms, audio_end_ms, .. } => {
                *last_interim = Some(PendingInterim { text: text.clone(), audio_start_ms: *audio_start_ms, audio_end_ms: *audio_end_ms });
            }
            SttEvent::FinalTranscript { .. } => *last_interim = None,
            _ => {}
        }
    }

    /// #56's fallback: called once a track's event channel has closed, with
    /// whatever `PendingInterim` was still outstanding (`None` if the last partial
    /// was already superseded by a final, or there was none). Persists it as
    /// `is_final: true` so it isn't silently dropped by `transcript::to_turns`.
    /// Loses word-level diarization (`speaker` stays `None`) since
    /// `PartialTranscript` never carries `words` — better than losing the
    /// utterance outright.
    fn persist_pending_interim(store: &SessionStore, session_id: SessionId, track: TrackKind, pending: Option<PendingInterim>) {
        let Some(pending) = pending else { return };
        if pending.text.trim().is_empty() {
            return;
        }
        tracing::info!(?track, "live transcription: finalizing last interim transcript after channel close");
        let segment = TranscriptSegment {
            session_id,
            track: Some(track),
            speaker: None,
            text: pending.text,
            start_ms: pending.audio_start_ms,
            end_ms: pending.audio_end_ms,
            is_final: true,
        };
        if let Err(err) = store.insert_transcript_segment(&segment) {
            tracing::warn!(%err, ?track, "live transcription: failed to persist fallback-final transcript");
        }
    }

    fn persist_event(store: &SessionStore, session_id: SessionId, track: Option<TrackKind>, event: SttEvent) {
        match event {
            SttEvent::PartialTranscript { text, audio_start_ms, audio_end_ms, .. } => {
                let segment = TranscriptSegment { session_id, track, speaker: None, text, start_ms: audio_start_ms, end_ms: audio_end_ms, is_final: false };
                if let Err(err) = store.insert_transcript_segment(&segment) {
                    tracing::warn!(%err, ?track, "live transcription: failed to persist partial transcript");
                }
            }
            SttEvent::FinalTranscript { text, words, audio_start_ms, audio_end_ms, .. } => {
                let speaker = words.as_ref().and_then(|words| words.first()).and_then(|word| word.speaker);
                let segment = TranscriptSegment { session_id, track, speaker, text, start_ms: audio_start_ms, end_ms: audio_end_ms, is_final: true };
                if let Err(err) = store.insert_transcript_segment(&segment) {
                    tracing::warn!(%err, ?track, "live transcription: failed to persist final transcript");
                }
            }
            SttEvent::SpeechStarted => tracing::debug!(?track, "live transcription: speech started"),
            SttEvent::SpeechEnded => tracing::debug!(?track, "live transcription: speech ended"),
            SttEvent::Error(err) => tracing::warn!(?track, %err, "live transcription: STT error"),
        }
    }

    /// Drains `audio_rx` to completion without doing anything with it — used when
    /// live transcription can't start at all (no credential store, no key, both
    /// sessions failed to open), so the sender side
    /// (`windows_frame_collector::collect_frames`) never blocks on a full channel.
    async fn drain(audio_rx: &mut UnboundedReceiver<(TrackKind, Vec<f32>, u32)>) {
        while audio_rx.recv().await.is_some() {}
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn set_status_updates_only_the_named_track() {
            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            set_status(&Some(sink.clone()), TrackKind::SelfMic, TrackTranscriptionStatus::Connected);
            let status = sink.lock().unwrap().clone();
            assert_eq!(status.self_status, TrackTranscriptionStatus::Connected);
            assert_eq!(status.remote_status, TrackTranscriptionStatus::NotConfigured);
        }

        #[test]
        fn set_both_status_updates_both_tracks() {
            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            set_both_status(&Some(sink.clone()), TrackTranscriptionStatus::Connecting);
            let status = sink.lock().unwrap().clone();
            assert_eq!(status.self_status, TrackTranscriptionStatus::Connecting);
            assert_eq!(status.remote_status, TrackTranscriptionStatus::Connecting);
        }

        #[test]
        fn note_event_tracks_the_latest_partial_and_clears_on_final() {
            let mut last_interim = None;
            note_event(&mut last_interim, &SttEvent::PartialTranscript { text: "hel".to_string(), audio_start_ms: Some(0), audio_end_ms: Some(100), extra: Default::default() });
            assert!(last_interim.is_some());
            note_event(&mut last_interim, &SttEvent::FinalTranscript { text: "hello".to_string(), words: None, audio_start_ms: Some(0), audio_end_ms: Some(200), extra: Default::default() });
            assert!(last_interim.is_none());
        }

        #[test]
        fn persist_pending_interim_inserts_a_final_segment() {
            let store = SessionStore::open_in_memory().unwrap();
            let manifest = test_manifest();
            store.create_session(&manifest).unwrap();

            let pending = PendingInterim { text: "こんにち".to_string(), audio_start_ms: Some(10), audio_end_ms: Some(500) };
            persist_pending_interim(&store, manifest.session_id, TrackKind::SelfMic, Some(pending));

            let segments = store.list_transcript_segments(manifest.session_id).unwrap();
            assert_eq!(segments.len(), 1);
            assert!(segments[0].is_final);
            assert_eq!(segments[0].text, "こんにち");
            assert_eq!(segments[0].track, Some(TrackKind::SelfMic));
        }

        #[test]
        fn persist_pending_interim_skips_when_none_or_blank() {
            let store = SessionStore::open_in_memory().unwrap();
            let manifest = test_manifest();
            store.create_session(&manifest).unwrap();

            persist_pending_interim(&store, manifest.session_id, TrackKind::SelfMic, None);
            persist_pending_interim(&store, manifest.session_id, TrackKind::SelfMic, Some(PendingInterim { text: "   ".to_string(), audio_start_ms: None, audio_end_ms: None }));

            assert!(store.list_transcript_segments(manifest.session_id).unwrap().is_empty());
        }

        fn test_manifest() -> recorder_domain::SessionManifest {
            recorder_domain::SessionManifest {
                schema_version: 1,
                session_id: SessionId::new(),
                started_at: chrono::Utc::now(),
                ended_at: None,
                platform: "test".to_string(),
                app_version: "0.0.0".to_string(),
                capture: recorder_domain::CaptureManifest {
                    microphone_device_id: "default".to_string(),
                    remote_source_id: "default".to_string(),
                    remote_source_kind: recorder_domain::RemoteSourceKind::EndpointLoopback,
                },
                audio: recorder_domain::AudioManifest { sample_rate: 48_000, segment_duration_ms: 30_000, tracks: vec![TrackKind::SelfMic, TrackKind::RemoteAudio] },
                consent: recorder_domain::ConsentManifest { confirmed_by_user: true, confirmed_at: chrono::Utc::now() },
            }
        }

        #[test]
        fn target_sample_rate_hz_pins_openai_and_assemblyai_leaves_others_at_capture_rate() {
            assert_eq!(target_sample_rate_hz(SttProviderKind::OpenAi, 48_000), 24_000);
            assert_eq!(target_sample_rate_hz(SttProviderKind::AssemblyAi, 48_000), 16_000);
            assert_eq!(target_sample_rate_hz(SttProviderKind::Deepgram, 48_000), 48_000);
            assert_eq!(target_sample_rate_hz(SttProviderKind::Google, 48_000), 48_000);
        }

        /// Minimal in-memory `CredentialStore` for exercising `build_stt_provider`
        /// without touching a real OS keyring — same "just a `HashMap`" shape as
        /// `credential_store::EncryptedFileStore`'s test doubles elsewhere in this
        /// workspace, kept local since no shared test-only crate exposes one.
        struct InMemoryCredentialStore(std::collections::HashMap<(String, String), String>);

        impl InMemoryCredentialStore {
            fn with(service: &str, account: &str, secret: &str) -> Self {
                let mut map = std::collections::HashMap::new();
                map.insert((service.to_string(), account.to_string()), secret.to_string());
                Self(map)
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
        fn build_stt_provider_reports_credential_missing_for_every_kind_when_store_is_empty() {
            let store = InMemoryCredentialStore(std::collections::HashMap::new());
            for kind in [SttProviderKind::Deepgram, SttProviderKind::OpenAi, SttProviderKind::Google, SttProviderKind::AssemblyAi] {
                let Err(err) = build_stt_provider(kind, &store) else { panic!("expected CredentialMissing for {kind:?}") };
                assert!(matches!(err, SttProviderFactoryError::CredentialMissing { kind: k, .. } if k == kind));
            }
        }

        #[test]
        fn build_stt_provider_constructs_openai_and_assemblyai_from_a_bare_api_key() {
            let store = InMemoryCredentialStore::with(stt_openai::CREDENTIAL_SERVICE, stt_openai::OPENAI_STT_API_KEY_ACCOUNT, "sk-test");
            assert!(build_stt_provider(SttProviderKind::OpenAi, &store).is_ok());

            let store = InMemoryCredentialStore::with(stt_assemblyai::CREDENTIAL_SERVICE, stt_assemblyai::ASSEMBLYAI_API_KEY_ACCOUNT, "aai-test");
            assert!(build_stt_provider(SttProviderKind::AssemblyAi, &store).is_ok());
        }

        #[test]
        fn build_stt_provider_constructs_google_from_valid_credentials_json() {
            let credentials = GoogleSttCredentials::new("my-project", "global");
            let json = serde_json::to_string(&credentials).unwrap();
            let store = InMemoryCredentialStore::with(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT, &json);
            assert!(build_stt_provider(SttProviderKind::Google, &store).is_ok());
        }

        #[test]
        fn build_stt_provider_reports_invalid_credential_for_malformed_google_json() {
            let store = InMemoryCredentialStore::with(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT, "not json");
            let Err(err) = build_stt_provider(SttProviderKind::Google, &store) else { panic!("expected InvalidCredential") };
            assert!(matches!(err, SttProviderFactoryError::InvalidCredential { kind: SttProviderKind::Google, .. }));
        }
    }
}

#[cfg(feature = "live-transcription")]
pub use stt_wiring::run_live_transcription;

/// `live-transcription` feature disabled: no STT provider types are even compiled in,
/// so this just drains and discards `audio_rx` — keeping
/// `windows_session::run_windows_capture_session`'s call site identical regardless of
/// the feature (see this module's doc comment). Reports `Unavailable` on
/// `status_sink` for both tracks immediately, same as the non-Windows desktop path
/// (see `apps/desktop/src/transcription_status.rs`).
#[cfg(not(feature = "live-transcription"))]
pub async fn run_live_transcription(
    _session_id: SessionId,
    _sample_rate_hz: u32,
    _credential_store: Option<Arc<dyn CredentialStore + Send + Sync>>,
    mut audio_rx: UnboundedReceiver<(TrackKind, Vec<f32>, u32)>,
    _store: &SessionStore,
    status_sink: Option<Arc<Mutex<TranscriptionStatus>>>,
) {
    set_both_status(&status_sink, TrackTranscriptionStatus::Unavailable);
    while audio_rx.recv().await.is_some() {}
}
