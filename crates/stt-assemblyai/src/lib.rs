//! `stt-api` adapter for AssemblyAI's Streaming v3 speech-to-text API.
//!
//! Protocol summary (verified against AssemblyAI's own docs on 2026-07-17, not from
//! memory — see the links below): connect to `wss://streaming.assemblyai.com/v3/ws`
//! with session config as query parameters and an `Authorization: <key>` header (no
//! `Bearer`/`Token` prefix, unlike Deepgram), then send raw PCM16 little-endian audio
//! as binary WebSocket frames (no JSON wrapping). Results arrive as
//! `{"type":"Turn", "end_of_turn": bool, "transcript": "...", "words": [...] }` JSON
//! text frames — v3 collapses what v2 called `PartialTranscript`/`FinalTranscript`
//! into this single `Turn` type, distinguished by `end_of_turn`. Sending
//! `{"type":"Terminate"}` makes the server drain and reply with
//! `{"type":"Termination", "audio_duration_seconds": ..., "session_duration_seconds":
//! ...}` before closing — that message is this crate's signal that `finalize()` can
//! return.
//!
//! Docs consulted: <https://www.assemblyai.com/docs/streaming/getting-started/transcribe-streaming-audio>,
//! <https://assemblyai.com/docs/api-reference/streaming-api/streaming-api>,
//! <https://www.assemblyai.com/docs/guides/v2_to_v3_migration_js>.
//!
//! **Diarization**: AssemblyAI's own support article
//! (<https://support.assemblyai.com/articles/2338942392-can-i-use-speaker-diarization-with-live-audio-transcription>)
//! states that speaker diarization for a *single* live audio stream isn't supported —
//! their recommended approach is one streaming session per speaker, merged into one
//! transcript downstream. This project already captures Self/Remote audio as separate
//! tracks (and therefore separate `SttSession`s), which is exactly that shape, so
//! per-session diarization would be redundant. This adapter therefore never sends the
//! wire-level `speaker_labels` parameter and [`Word::speaker`] is always `None`
//! regardless of [`SttSessionConfig::diarization`] — track identity is the caller's
//! speaker signal, not a field on `Word`.
//!
//! **VAD events**: v3 has a `SpeechStarted` message but no matching "speech ended"
//! message — silence is instead signalled implicitly by the next `Turn` carrying
//! `end_of_turn: true`. This adapter forwards `SpeechStarted` as [`SttEvent::SpeechStarted`]
//! when [`SttSessionConfig::vad_events`] is set, and never emits `SttEvent::SpeechEnded`.
//!
//! **Sample rate**: this adapter only speaks PCM16 (`encoding=pcm_s16le`), so
//! `sample_rate_hz` must be exactly 16000 — AssemblyAI's Opus encodings ignore
//! `sample_rate` entirely, which isn't a fit for `stt-api`'s "caller picks the rate"
//! contract, so this crate doesn't support them.

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use stt_api::{AudioChunk, SttError, SttEvent, SttProvider, SttSession, SttSessionConfig, Word};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// `credential-store` service/account this crate's API key is expected under
/// (design.md §12.4), matching `stt_deepgram::CREDENTIAL_SERVICE`/`*_ACCOUNT`'s
/// pattern so the settings UI (writer) and the capture pipeline (reader) agree on the
/// same two strings without either hardcoding them independently.
pub const CREDENTIAL_SERVICE: &str = "1on1-recorder";
pub const ASSEMBLYAI_API_KEY_ACCOUNT: &str = "assemblyai-api-key";

const STREAMING_URL: &str = "wss://streaming.assemblyai.com/v3/ws";
/// Verified 2026-07-17 against AssemblyAI's own Streaming v3 API reference: the only
/// valid `speech_model` values are `universal-streaming-english`,
/// `universal-streaming-multilingual`, and `u3-rt-pro` (Universal-3 Pro Streaming,
/// the highest-accuracy option — this project's meeting-transcription use case wants
/// accuracy over the cheaper English-only/multilingual defaults). Any other value is
/// rejected by the server and the session never starts.
const DEFAULT_MODEL: &str = "u3-rt-pro";
/// This adapter sends raw PCM16 (`encoding=pcm_s16le`), which is the only encoding
/// where AssemblyAI actually honors `sample_rate` rather than ignoring it.
const SUPPORTED_SAMPLE_RATE_HZ: u32 = 16_000;
const BYTES_PER_SAMPLE: usize = 2; // PCM16 mono, little-endian.

/// AssemblyAI Streaming v3 requires each binary audio frame to carry 50-1000ms of
/// audio at the negotiated sample rate (frames outside that range get the session
/// closed) — `100ms` is their own documented "good starting point" for a custom
/// pipeline, so [`AssemblyAISession`] buffers caller-provided PCM up to this size
/// before flushing, rather than forwarding whatever chunk size the caller happens to
/// send (which, at 16kHz, can be well under 50ms — e.g. a 10ms capture-side chunk is
/// only 320 bytes).
const TARGET_CHUNK_MS: u32 = 100;
/// The protocol's documented floor; a trailing buffered remainder shorter than this
/// at [`AssemblyAISession::finalize`] time is dropped rather than sent, since sending
/// it would just get the session closed instead of transcribed.
const MIN_CHUNK_MS: u32 = 50;

fn chunk_bytes(ms: u32) -> usize {
    (SUPPORTED_SAMPLE_RATE_HZ as usize * ms as usize / 1000) * BYTES_PER_SAMPLE
}

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A configured AssemblyAI provider. One instance can open many sessions.
pub struct AssemblyAIProvider {
    api_key: String,
    model: String,
}

impl AssemblyAIProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[async_trait]
impl SttProvider for AssemblyAIProvider {
    async fn start_session(
        &self,
        config: SttSessionConfig,
    ) -> Result<(Box<dyn SttSession>, mpsc::UnboundedReceiver<SttEvent>), SttError> {
        if config.sample_rate_hz != SUPPORTED_SAMPLE_RATE_HZ {
            return Err(SttError::PermanentError(format!(
                "sample_rate_hz must be {SUPPORTED_SAMPLE_RATE_HZ} (this adapter only \
                 supports PCM16 at AssemblyAI's default streaming rate), got {}",
                config.sample_rate_hz
            )));
        }

        let vad_events = config.vad_events;
        let url = build_url(&self.model, &config);
        let request = build_request(&url, &self.api_key)?;

        let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(map_connect_error)?;

        let (write, read) = ws_stream.split();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (drained_tx, drained_rx) = oneshot::channel();

        tokio::spawn(writer_task(write, cmd_rx));
        tokio::spawn(reader_task(read, event_tx, drained_tx, vad_events));

        Ok((
            Box::new(AssemblyAISession {
                commands: cmd_tx,
                drained: Some(drained_rx),
                pending: Vec::new(),
            }),
            event_rx,
        ))
    }
}

enum WsCommand {
    Audio(Vec<u8>),
    Close,
}

/// Holds only a command-channel sender, never the WebSocket itself, so this type
/// stays trivially `Send` regardless of whether the underlying TLS stream is —
/// the actual socket lives in `writer_task`/`reader_task` instead.
struct AssemblyAISession {
    commands: mpsc::UnboundedSender<WsCommand>,
    drained: Option<oneshot::Receiver<Result<(), SttError>>>,
    /// PCM16 bytes accumulated across `send_audio` calls, not yet flushed as a
    /// `WsCommand::Audio` frame — see `TARGET_CHUNK_MS`'s doc comment.
    pending: Vec<u8>,
}

#[async_trait]
impl SttSession for AssemblyAISession {
    async fn send_audio(&mut self, chunk: AudioChunk<'_>) -> Result<(), SttError> {
        self.pending.reserve(chunk.pcm.len() * BYTES_PER_SAMPLE);
        for &sample in chunk.pcm {
            let clamped = sample.clamp(-1.0, 1.0);
            let pcm16 = (clamped * i16::MAX as f32).round() as i16;
            self.pending.extend_from_slice(&pcm16.to_le_bytes());
        }
        let target = chunk_bytes(TARGET_CHUNK_MS);
        while self.pending.len() >= target {
            let frame: Vec<u8> = self.pending.drain(..target).collect();
            self.commands
                .send(WsCommand::Audio(frame))
                .map_err(|_| SttError::SessionClosed)?;
        }
        Ok(())
    }

    async fn finalize(mut self: Box<Self>) -> Result<(), SttError> {
        // Flush a trailing buffered remainder if it clears the protocol's 50ms
        // floor; a `send_audio` loop can never leave `pending` above `TARGET_CHUNK_MS`
        // worth of bytes (it flushes as soon as that's reached), so this is always
        // within the 50-1000ms range and safe to send as a single final frame.
        if self.pending.len() >= chunk_bytes(MIN_CHUNK_MS) {
            let frame = std::mem::take(&mut self.pending);
            self.commands
                .send(WsCommand::Audio(frame))
                .map_err(|_| SttError::SessionClosed)?;
        }
        self.commands
            .send(WsCommand::Close)
            .map_err(|_| SttError::SessionClosed)?;
        match self.drained.take() {
            Some(rx) => rx.await.map_err(|_| SttError::SessionClosed)?,
            None => Ok(()),
        }
    }
}

async fn writer_task(
    mut write: SplitSink<WsStream, Message>,
    mut commands: mpsc::UnboundedReceiver<WsCommand>,
) {
    while let Some(cmd) = commands.recv().await {
        let result = match cmd {
            WsCommand::Audio(bytes) => write.send(Message::Binary(bytes)).await,
            WsCommand::Close => {
                write
                    .send(Message::Text(r#"{"type":"Terminate"}"#.to_string()))
                    .await
            }
        };
        if result.is_err() {
            break;
        }
    }
}

async fn reader_task(
    mut read: SplitStream<WsStream>,
    events: mpsc::UnboundedSender<SttEvent>,
    drained: oneshot::Sender<Result<(), SttError>>,
    vad_events: bool,
) {
    let mut drained = Some(drained);

    while let Some(message) = read.next().await {
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                let stt_err = SttError::Transport(err.to_string());
                let _ = events.send(SttEvent::Error(stt_err.clone()));
                if let Some(tx) = drained.take() {
                    let _ = tx.send(Err(stt_err));
                }
                return;
            }
        };

        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };

        match serde_json::from_str::<AssemblyAIMessage>(&text) {
            Ok(AssemblyAIMessage::Turn(turn)) => {
                if let Some(event) = translate_turn(turn) {
                    let _ = events.send(event);
                }
            }
            Ok(AssemblyAIMessage::SpeechStarted) => {
                if vad_events {
                    let _ = events.send(SttEvent::SpeechStarted);
                }
            }
            Ok(AssemblyAIMessage::Begin) => {
                tracing::debug!("assemblyai session begin");
            }
            Ok(AssemblyAIMessage::Termination) => {
                if let Some(tx) = drained.take() {
                    let _ = tx.send(Ok(()));
                }
                break;
            }
            Ok(AssemblyAIMessage::Unknown) => {
                tracing::debug!(%text, "unrecognized assemblyai message type");
            }
            Err(err) => {
                tracing::debug!(%text, %err, "failed to parse assemblyai message");
            }
        }
    }

    if let Some(tx) = drained.take() {
        let _ = tx.send(Err(SttError::Transport(
            "connection closed before Termination was received".to_string(),
        )));
    }
}

fn build_url(model: &str, config: &SttSessionConfig) -> String {
    let language = config.language.clone().unwrap_or_else(|| "ja".to_string());
    // `language_codes` takes a JSON-encoded array even though it's an ordinary query
    // parameter — a single-element list requests a monolingual session (AssemblyAI's
    // own example for this shape: `["es"]`).
    let language_codes =
        serde_json::to_string(&[language]).expect("single-string array always serializes");

    // `speaker_labels` is deliberately never sent — see the module-level doc comment
    // on diarization. `Word::speaker` stays `None` regardless of `config.diarization`.
    let params: Vec<(&str, String)> = vec![
        ("speech_model", model.to_string()),
        ("sample_rate", config.sample_rate_hz.to_string()),
        ("encoding", "pcm_s16le".to_string()),
        ("language_codes", language_codes),
        ("include_partial_turns", config.interim_results.to_string()),
    ];

    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
        .finish();
    format!("{STREAMING_URL}?{query}")
}

fn build_request(
    url: &str,
    api_key: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, SttError> {
    let mut request = url
        .into_client_request()
        .map_err(|err| SttError::Transport(err.to_string()))?;
    // No `Bearer`/`Token` prefix — AssemblyAI's `Authorization` header is the raw key.
    let header_value = HeaderValue::from_str(api_key)
        .map_err(|err| SttError::AuthenticationFailed(err.to_string()))?;
    request.headers_mut().insert("Authorization", header_value);
    Ok(request)
}

fn map_connect_error(err: tokio_tungstenite::tungstenite::Error) -> SttError {
    use tokio_tungstenite::tungstenite::Error as WsError;
    match &err {
        WsError::Http(response) => {
            let status = response.status().as_u16();
            match status {
                401 | 403 => SttError::AuthenticationFailed(format!("HTTP {status}")),
                429 => SttError::RateLimited,
                _ => SttError::Transport(format!("HTTP {status}")),
            }
        }
        other => SttError::Transport(other.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AssemblyAIMessage {
    Begin,
    Turn(TurnMessage),
    SpeechStarted,
    Termination,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct TurnMessage {
    end_of_turn: bool,
    transcript: String,
    #[serde(default)]
    words: Vec<TurnWord>,
}

#[derive(Debug, Deserialize)]
struct TurnWord {
    text: String,
    start: u64,
    end: u64,
    #[serde(default)]
    confidence: Option<f32>,
    // `word_is_final` and `speaker` are intentionally not read: this adapter treats
    // every word in an `end_of_turn` Turn as final, and never attributes speakers
    // (see the module-level doc comment on diarization).
}

fn translate_turn(turn: TurnMessage) -> Option<SttEvent> {
    if turn.transcript.is_empty() {
        // AssemblyAI emits empty-transcript turns during silence; not useful to
        // forward, and an empty FinalTranscript would just be noise downstream.
        return None;
    }

    // Word timestamps are already session-relative milliseconds on the wire, unlike
    // Deepgram's fractional seconds, so no unit conversion is needed here.
    let audio_start_ms = turn.words.first().map(|w| w.start);
    let audio_end_ms = turn.words.last().map(|w| w.end);

    if turn.end_of_turn {
        let words = if turn.words.is_empty() {
            None
        } else {
            Some(
                turn.words
                    .into_iter()
                    .map(|w| Word {
                        text: w.text,
                        start_ms: Some(w.start),
                        end_ms: Some(w.end),
                        confidence: w.confidence,
                        speaker: None,
                    })
                    .collect(),
            )
        };
        Some(SttEvent::FinalTranscript {
            text: turn.transcript,
            words,
            audio_start_ms,
            audio_end_ms,
            extra: Default::default(),
        })
    } else {
        Some(SttEvent::PartialTranscript {
            text: turn.transcript,
            audio_start_ms,
            audio_end_ms,
            extra: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn query_pairs(url: &str) -> HashMap<String, String> {
        let query = url.split_once('?').expect("url has a query string").1;
        url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect()
    }

    #[test]
    fn build_url_includes_required_params() {
        let config = SttSessionConfig::new(16_000).with_interim_results(true);
        let url = build_url(DEFAULT_MODEL, &config);
        assert!(url.starts_with(STREAMING_URL));

        let params = query_pairs(&url);
        assert_eq!(params["speech_model"], DEFAULT_MODEL);
        assert_eq!(params["sample_rate"], "16000");
        assert_eq!(params["encoding"], "pcm_s16le");
        assert_eq!(params["language_codes"], r#"["ja"]"#);
        assert_eq!(params["include_partial_turns"], "true");
    }

    #[test]
    fn build_url_uses_configured_language() {
        let config = SttSessionConfig::new(16_000).with_language("en");
        let url = build_url(DEFAULT_MODEL, &config);
        assert_eq!(query_pairs(&url)["language_codes"], r#"["en"]"#);
    }

    #[test]
    fn build_url_never_sends_speaker_labels() {
        // Diarization for a single live stream isn't supported by AssemblyAI (see
        // module docs) — even when the caller asks for it, this adapter must not
        // request it over the wire.
        let config = SttSessionConfig::new(16_000).with_diarization(true);
        let url = build_url(DEFAULT_MODEL, &config);
        assert!(!url.contains("speaker_labels"));
    }

    #[tokio::test]
    async fn start_session_rejects_non_16k_sample_rate() {
        let provider = AssemblyAIProvider::new("test-key");
        let config = SttSessionConfig::new(8_000);
        let Err(err) = provider.start_session(config).await else {
            panic!("expected start_session to reject a non-16kHz sample rate");
        };
        assert!(matches!(err, SttError::PermanentError(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn parses_partial_turn_message() {
        let raw = r#"{
            "type": "Turn",
            "turn_order": 0,
            "turn_is_formatted": false,
            "end_of_turn": false,
            "transcript": "こんにちは",
            "words": [{ "text": "こんにちは", "start": 1000, "end": 1500, "confidence": 0.9 }]
        }"#;
        let msg: AssemblyAIMessage = serde_json::from_str(raw).unwrap();
        let AssemblyAIMessage::Turn(turn) = msg else {
            panic!("expected Turn variant");
        };
        let event = translate_turn(turn).unwrap();
        match event {
            SttEvent::PartialTranscript {
                text,
                audio_start_ms,
                audio_end_ms,
                ..
            } => {
                assert_eq!(text, "こんにちは");
                assert_eq!(audio_start_ms, Some(1000));
                assert_eq!(audio_end_ms, Some(1500));
            }
            other => panic!("expected PartialTranscript, got {other:?}"),
        }
    }

    #[test]
    fn parses_final_turn_message_with_words_and_no_speaker() {
        let raw = r#"{
            "type": "Turn",
            "turn_order": 0,
            "turn_is_formatted": true,
            "end_of_turn": true,
            "end_of_turn_confidence": 1.0,
            "transcript": "こんにちは",
            "words": [
                { "text": "こんにちは", "start": 1000, "end": 1500, "confidence": 0.98, "word_is_final": true, "speaker": "A" }
            ]
        }"#;
        let msg: AssemblyAIMessage = serde_json::from_str(raw).unwrap();
        let AssemblyAIMessage::Turn(turn) = msg else {
            panic!("expected Turn variant");
        };
        let event = translate_turn(turn).unwrap();
        match event {
            SttEvent::FinalTranscript { text, words, .. } => {
                assert_eq!(text, "こんにちは");
                let words = words.unwrap();
                assert_eq!(words.len(), 1);
                assert_eq!(words[0].text, "こんにちは");
                // Wire has a `speaker` field, but this adapter never surfaces it.
                assert_eq!(words[0].speaker, None);
            }
            other => panic!("expected FinalTranscript, got {other:?}"),
        }
    }

    #[test]
    fn empty_transcript_is_dropped() {
        let raw = r#"{
            "type": "Turn",
            "turn_order": 0,
            "turn_is_formatted": false,
            "end_of_turn": false,
            "transcript": "",
            "words": []
        }"#;
        let msg: AssemblyAIMessage = serde_json::from_str(raw).unwrap();
        let AssemblyAIMessage::Turn(turn) = msg else {
            panic!("expected Turn variant");
        };
        assert!(translate_turn(turn).is_none());
    }

    #[test]
    fn parses_begin_speech_started_and_termination_messages() {
        let begin: AssemblyAIMessage = serde_json::from_str(
            r#"{"type":"Begin","id":"abc","expires_at":1772570132}"#,
        )
        .unwrap();
        assert!(matches!(begin, AssemblyAIMessage::Begin));

        let started: AssemblyAIMessage =
            serde_json::from_str(r#"{"type":"SpeechStarted","timestamp":123,"confidence":0.95}"#)
                .unwrap();
        assert!(matches!(started, AssemblyAIMessage::SpeechStarted));

        let termination: AssemblyAIMessage = serde_json::from_str(
            r#"{"type":"Termination","audio_duration_seconds":30,"session_duration_seconds":32}"#,
        )
        .unwrap();
        assert!(matches!(termination, AssemblyAIMessage::Termination));
    }

    #[test]
    fn unknown_message_type_does_not_error() {
        let msg: AssemblyAIMessage = serde_json::from_str(r#"{"type":"SpeakerRevision"}"#).unwrap();
        assert!(matches!(msg, AssemblyAIMessage::Unknown));
    }
}
