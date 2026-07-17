//! Real-time transcription side channel (design's "録音中にリアルタイムで文字起こしを
//! 表示したい"): consumes the raw-PCM side channel `windows_frame_collector::collect_frames`
//! feeds (mirroring how `level_sink` is fed, for the same "cheap side channel, batch
//! `run_pipeline` stays untouched" reason — see that module's doc comment), streams it
//! into per-track Deepgram (`stt-deepgram`) sessions, and persists every
//! `SttEvent::PartialTranscript`/`FinalTranscript` via
//! `SessionStore::insert_transcript_segment`.
//!
//! Gated behind the `live-transcription` feature (see `app-service`'s `Cargo.toml`):
//! without it, [`run_live_transcription`] below compiles to a stub that just drains
//! and discards `audio_rx`, so `windows_session::run_windows_capture_session` doesn't
//! need any `#[cfg]` of its own at the call site, and a plain `--features
//! windows-supervisor` build never pulls in stt-deepgram's websocket/TLS stack.
//!
//! Like the rest of `windows_supervisor`/`windows_session`, never run against a real
//! Deepgram connection or real Windows hardware in this environment —
//! cross-compile-checked only (see this crate's README).
//!
//! No macOS equivalent yet: `macos_frame_collector`/`macos_session` would need the
//! identical `stt_sink`/`run_live_transcription` wiring once `capture-macos` is ever
//! actually compiled/run (see that crate's own doc comment) — out of scope here since
//! macOS capture itself isn't in scope yet.

use std::sync::Arc;

use credential_store::CredentialStore;
use recorder_domain::{SessionId, TrackKind};
use session_store::SessionStore;
use tokio::sync::mpsc::UnboundedReceiver;

#[cfg(feature = "live-transcription")]
mod deepgram_wiring {
    use super::*;
    use session_store::TranscriptSegment;
    use stt_api::{AudioChunk, SttEvent, SttProvider, SttSession, SttSessionConfig};
    use stt_deepgram::{DeepgramProvider, CREDENTIAL_SERVICE, DEEPGRAM_API_KEY_ACCOUNT};

    /// Runs for the lifetime of one recording session, ending when `audio_rx` closes
    /// (i.e. `windows_session::run_capture_blocking`'s collector thread — and the
    /// `stt_tx` it owns — has finished, meaning capture is fully done). Failing to
    /// obtain a Deepgram session at all (no credential configured, auth failure,
    /// connect failure) is logged and simply means no live transcription for that
    /// track — same "failure doesn't take down the whole pipeline" spirit as
    /// `upload_worker`'s retry handling; the batch `run_pipeline` recording itself is
    /// unaffected either way.
    pub async fn run_live_transcription(
        session_id: SessionId,
        sample_rate_hz: u32,
        credential_store: Option<Arc<dyn CredentialStore + Send + Sync>>,
        mut audio_rx: UnboundedReceiver<(TrackKind, Vec<f32>, u32)>,
        store: &SessionStore,
    ) {
        let Some(credential_store) = credential_store else {
            tracing::debug!("live transcription: no credential store configured, skipping");
            drain(&mut audio_rx).await;
            return;
        };

        let api_key = match credential_store.load(CREDENTIAL_SERVICE, DEEPGRAM_API_KEY_ACCOUNT) {
            Ok(key) => key,
            Err(err) => {
                tracing::info!(%err, "live transcription: no Deepgram API key configured, skipping");
                drain(&mut audio_rx).await;
                return;
            }
        };

        let provider = DeepgramProvider::new(api_key);
        // Phase 1A capture is a fixed format (design.md; see
        // `windows_frame_collector`'s "falls back to 48kHz mono" comment) — the
        // session is opened once, up front, at the manifest's nominal rate rather
        // than per-chunk, since Deepgram's session config (like every other
        // provider's) is fixed for the connection's lifetime.
        // Diarization is additional info within a single track (e.g. multiple people
        // on the Remote track in a group call) — the app's primary speaker split is
        // still Self/Remote (see this module's doc comment), not Deepgram's `speaker`
        // index alone.
        let config = SttSessionConfig::new(sample_rate_hz).with_interim_results(true).with_vad_events(true).with_diarization(true);

        let self_sess = match provider.start_session(config.clone()).await {
            Ok((session, events)) => Some((session, events)),
            Err(err) => {
                tracing::warn!(%err, track = "self", "live transcription: failed to start STT session");
                None
            }
        };
        let remote_sess = match provider.start_session(config).await {
            Ok((session, events)) => Some((session, events)),
            Err(err) => {
                tracing::warn!(%err, track = "remote", "live transcription: failed to start STT session");
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

        while audio_open || self_events.is_some() || remote_events.is_some() {
            tokio::select! {
                maybe = audio_rx.recv(), if audio_open => {
                    match maybe {
                        Some((track, samples, _sample_rate)) => {
                            let (session, samples_sent) = match track {
                                TrackKind::SelfMic => (self_session.as_mut(), &mut self_samples_sent),
                                TrackKind::RemoteAudio => (remote_session.as_mut(), &mut remote_samples_sent),
                            };
                            if let Some(session) = session {
                                let chunk = AudioChunk { pcm: &samples, start_sample: *samples_sent };
                                *samples_sent += samples.len() as u64;
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
                maybe = self_events.as_mut().unwrap().recv(), if self_events.is_some() => {
                    match maybe {
                        Some(event) => persist_event(store, session_id, Some(TrackKind::SelfMic), event),
                        None => self_events = None,
                    }
                }
                maybe = remote_events.as_mut().unwrap().recv(), if remote_events.is_some() => {
                    match maybe {
                        Some(event) => persist_event(store, session_id, Some(TrackKind::RemoteAudio), event),
                        None => remote_events = None,
                    }
                }
            }
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
}

#[cfg(feature = "live-transcription")]
pub use deepgram_wiring::run_live_transcription;

/// `live-transcription` feature disabled: no STT provider types are even compiled in,
/// so this just drains and discards `audio_rx` — keeping
/// `windows_session::run_windows_capture_session`'s call site identical regardless of
/// the feature (see this module's doc comment).
#[cfg(not(feature = "live-transcription"))]
pub async fn run_live_transcription(
    _session_id: SessionId,
    _sample_rate_hz: u32,
    _credential_store: Option<Arc<dyn CredentialStore + Send + Sync>>,
    mut audio_rx: UnboundedReceiver<(TrackKind, Vec<f32>, u32)>,
    _store: &SessionStore,
) {
    while audio_rx.recv().await.is_some() {}
}
