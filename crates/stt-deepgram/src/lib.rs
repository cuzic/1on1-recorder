//! `stt-api` adapter for Deepgram's Nova-3 streaming speech-to-text API.
//!
//! Protocol summary (see `stt-transcription-architecture.md` §6 at the repository
//! root for the full writeup and source links): connect to
//! `wss://api.deepgram.com/v1/listen` with session config as query parameters and an
//! `Authorization: Token <key>` header, then send raw PCM16 little-endian audio as
//! binary WebSocket frames (no JSON wrapping, unlike Gemini Live). Results arrive as
//! `{"type":"Results", "is_final": bool, "speech_final": bool, ...}` JSON text frames.
//! Sending `{"type":"CloseStream"}` makes the server drain and reply with
//! `{"type":"Metadata", ...}` before closing — that Metadata message is this crate's
//! signal that `finalize()` can return.

mod batch;

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use stt_api::{
    AudioChunk, KeepAliveEffect, SttError, SttEvent, SttProvider, SttSession, SttSessionConfig,
    Word,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub use batch::DeepgramBatchProvider;

/// `credential-store` service/account this crate's API key is expected under
/// (design.md §12.4), matching `summarize::CREDENTIAL_SERVICE`/`*_ACCOUNT`'s pattern
/// so the settings UI (writer) and the capture pipeline (reader) agree on the same
/// two strings without either hardcoding them independently.
pub const CREDENTIAL_SERVICE: &str = "1on1-recorder";
pub const DEEPGRAM_API_KEY_ACCOUNT: &str = "deepgram-api-key";

const LISTEN_URL: &str = "wss://api.deepgram.com/v1/listen";
const DEFAULT_MODEL: &str = "nova-3";
/// Deepgram's own guidance: `utterance_end_ms` must be 1000ms or higher, since interim
/// results arrive roughly every second.
const UTTERANCE_END_MS: &str = "1000";
/// Bound on `WsCommand`s (audio chunks + keep-alives + the final close) queued for
/// `writer_task`. Capture callbacks hand off roughly one chunk per ~10ms of audio
/// (see WASAPI's typical shared-mode device period), so this buffers on the order of
/// a few seconds before `try_send` starts rejecting — enough slack for a brief TCP
/// stall without letting a stuck write silently grow the queue without bound.
const COMMAND_QUEUE_CAPACITY: usize = 300;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A configured Deepgram provider. One instance can open many sessions.
pub struct DeepgramProvider {
    api_key: String,
    model: String,
}

impl DeepgramProvider {
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
impl SttProvider for DeepgramProvider {
    async fn start_session(
        &self,
        config: SttSessionConfig,
    ) -> Result<(Box<dyn SttSession>, mpsc::UnboundedReceiver<SttEvent>), SttError> {
        if config.sample_rate_hz == 0 {
            return Err(SttError::PermanentError(
                "sample_rate_hz must be nonzero".to_string(),
            ));
        }

        let url = build_url(&self.model, &config);
        let request = build_request(&url, &self.api_key)?;

        let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(map_connect_error)?;

        let (write, read) = ws_stream.split();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (drained_tx, drained_rx) = oneshot::channel();

        tokio::spawn(writer_task(write, cmd_rx));
        tokio::spawn(reader_task(read, event_tx, drained_tx));

        Ok((
            Box::new(DeepgramSession {
                commands: cmd_tx,
                drained: Some(drained_rx),
            }),
            event_rx,
        ))
    }
}

enum WsCommand {
    Audio(Vec<u8>),
    /// Deepgram's `{"type":"KeepAlive"}` control frame. It carries no audio, so
    /// unlike `Audio` it never advances the provider's audio timeline, and
    /// Deepgram does not bill for it (per Deepgram's docs).
    KeepAlive,
    Close,
}

/// Holds only a command-channel sender, never the WebSocket itself, so this type
/// stays trivially `Send` regardless of whether the underlying TLS stream is —
/// the actual socket lives in `writer_task`/`reader_task` instead.
struct DeepgramSession {
    commands: mpsc::Sender<WsCommand>,
    drained: Option<oneshot::Receiver<Result<(), SttError>>>,
}

#[async_trait]
impl SttSession for DeepgramSession {
    async fn send_audio(&mut self, chunk: AudioChunk<'_>) -> Result<(), SttError> {
        let bytes = pcm_f32_to_linear16_le(chunk.pcm);
        self.commands
            .try_send(WsCommand::Audio(bytes))
            .map_err(|err| match err {
                TrySendError::Full(_) => SttError::Transport(
                    "audio send queue is full; writer_task isn't keeping up".to_string(),
                ),
                TrySendError::Closed(_) => SttError::SessionClosed,
            })
    }

    async fn finalize(mut self: Box<Self>) -> Result<(), SttError> {
        self.commands
            .send(WsCommand::Close)
            .await
            .map_err(|_| SttError::SessionClosed)?;
        match self.drained.take() {
            Some(rx) => rx.await.map_err(|_| SttError::SessionClosed)?,
            None => Ok(()),
        }
    }

    /// Sends Deepgram's `{"type":"KeepAlive"}` control frame so the connection
    /// survives stretches where silence is being skipped rather than streamed.
    /// Carries no audio, so it's `ControlMessage`, not `InjectedAudio`.
    async fn keep_alive(&mut self) -> Result<KeepAliveEffect, SttError> {
        self.commands
            .try_send(WsCommand::KeepAlive)
            .map_err(|err| match err {
                TrySendError::Full(_) => SttError::Transport(
                    "audio send queue is full; writer_task isn't keeping up".to_string(),
                ),
                TrySendError::Closed(_) => SttError::SessionClosed,
            })?;
        Ok(KeepAliveEffect::ControlMessage)
    }
}

async fn writer_task(
    mut write: SplitSink<WsStream, Message>,
    mut commands: mpsc::Receiver<WsCommand>,
) {
    while let Some(cmd) = commands.recv().await {
        let result = match cmd {
            WsCommand::Audio(bytes) => write.send(Message::Binary(bytes)).await,
            WsCommand::KeepAlive => {
                write
                    .send(Message::Text(r#"{"type":"KeepAlive"}"#.to_string()))
                    .await
            }
            WsCommand::Close => {
                write
                    .send(Message::Text(r#"{"type":"CloseStream"}"#.to_string()))
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

        match serde_json::from_str::<DeepgramMessage>(&text) {
            Ok(DeepgramMessage::Results(results)) => {
                if let Some(event) = translate_results(results) {
                    let _ = events.send(event);
                }
            }
            Ok(DeepgramMessage::SpeechStarted) => {
                let _ = events.send(SttEvent::SpeechStarted);
            }
            Ok(DeepgramMessage::UtteranceEnd) => {
                let _ = events.send(SttEvent::SpeechEnded);
            }
            Ok(DeepgramMessage::Metadata) => {
                if let Some(tx) = drained.take() {
                    let _ = tx.send(Ok(()));
                }
                break;
            }
            Ok(DeepgramMessage::Error(body)) => {
                let _ = events.send(SttEvent::Error(SttError::PermanentError(body.to_string())));
            }
            Ok(DeepgramMessage::Unknown) => {
                tracing::debug!(%text, "unrecognized deepgram message type");
            }
            Err(err) => {
                tracing::debug!(%text, %err, "failed to parse deepgram message");
            }
        }
    }

    if let Some(tx) = drained.take() {
        let _ = tx.send(Err(SttError::Transport(
            "connection closed before Metadata was received".to_string(),
        )));
    }
}

/// Query parameters shared by both the streaming (`build_url`) and batch
/// (`batch::build_batch_url`) `/v1/listen` requests: model/language/punctuate are
/// always sent, `diarize`/`keywords` only when the caller opted in. Streaming-only
/// concerns (encoding/sample_rate/channels/interim_results/vad_events) are each
/// caller's own responsibility since batch's audio isn't a live `AudioChunk` stream
/// and doesn't carry a `SttSessionConfig::sample_rate_hz`.
fn common_query_params(model: &str, config: &SttSessionConfig) -> Vec<(&'static str, String)> {
    let language = config.language.clone().unwrap_or_else(|| "ja".to_string());
    let mut params: Vec<(&'static str, String)> =
        vec![("model", model.to_string()), ("language", language), ("punctuate", "true".to_string())];
    if config.diarization {
        params.push(("diarize", "true".to_string()));
    }
    if let Some(boost) = &config.extra.vocabulary_boost {
        for word in boost {
            params.push(("keywords", word.clone()));
        }
    }
    params
}

fn encode_query(params: &[(&str, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
        .finish()
}

fn build_url(model: &str, config: &SttSessionConfig) -> String {
    let mut params = common_query_params(model, config);
    params.push(("encoding", "linear16".to_string()));
    params.push(("sample_rate", config.sample_rate_hz.to_string()));
    params.push(("channels", "1".to_string()));
    params.push(("interim_results", config.interim_results.to_string()));
    params.push(("vad_events", config.vad_events.to_string()));
    if config.vad_events {
        params.push(("utterance_end_ms", UTTERANCE_END_MS.to_string()));
    }

    format!("{LISTEN_URL}?{}", encode_query(&params))
}

/// Converts f32 PCM samples (expected in `[-1.0, 1.0]`) to little-endian PCM16 bytes —
/// the wire format Deepgram expects for both the streaming (`Message::Binary` frames)
/// and batch (request body) APIs.
fn pcm_f32_to_linear16_le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let pcm16 = (clamped * i16::MAX as f32).round() as i16;
        bytes.extend_from_slice(&pcm16.to_le_bytes());
    }
    bytes
}

fn build_request(
    url: &str,
    api_key: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, SttError> {
    let mut request = url
        .into_client_request()
        .map_err(|err| SttError::Transport(err.to_string()))?;
    let header_value = HeaderValue::from_str(&format!("Token {api_key}"))
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
enum DeepgramMessage {
    Results(DeepgramResults),
    Metadata,
    SpeechStarted,
    UtteranceEnd,
    Error(serde_json::Value),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct DeepgramResults {
    channel: DeepgramChannel,
    is_final: bool,
    #[serde(default)]
    start: Option<f64>,
    #[serde(default)]
    duration: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DeepgramChannel {
    alternatives: Vec<DeepgramAlternative>,
}

#[derive(Debug, Deserialize)]
struct DeepgramAlternative {
    transcript: String,
    #[serde(default)]
    words: Vec<DeepgramWord>,
}

#[derive(Debug, Deserialize)]
struct DeepgramWord {
    word: String,
    start: f64,
    end: f64,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    speaker: Option<u32>,
}

fn translate_results(results: DeepgramResults) -> Option<SttEvent> {
    let alternative = results.channel.alternatives.into_iter().next()?;
    if alternative.transcript.is_empty() {
        // Deepgram emits empty interim transcripts during silence; not useful to
        // forward, and an empty FinalTranscript would just be noise downstream.
        return None;
    }

    let audio_start_ms = results.start.map(|s| (s * 1000.0).round() as u64);
    let audio_end_ms = match (results.start, results.duration) {
        (Some(start), Some(duration)) => Some(((start + duration) * 1000.0).round() as u64),
        _ => None,
    };

    if results.is_final {
        let words = if alternative.words.is_empty() {
            None
        } else {
            Some(
                alternative
                    .words
                    .into_iter()
                    .map(|w| Word {
                        text: w.word,
                        start_ms: Some((w.start * 1000.0).round() as u64),
                        end_ms: Some((w.end * 1000.0).round() as u64),
                        confidence: w.confidence,
                        speaker: w.speaker,
                    })
                    .collect(),
            )
        };
        Some(SttEvent::FinalTranscript {
            text: alternative.transcript,
            words,
            audio_start_ms,
            audio_end_ms,
            extra: Default::default(),
        })
    } else {
        Some(SttEvent::PartialTranscript {
            text: alternative.transcript,
            audio_start_ms,
            audio_end_ms,
            extra: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_includes_required_params() {
        let config = SttSessionConfig::new(16_000).with_interim_results(true);
        let url = build_url(DEFAULT_MODEL, &config);
        assert!(url.starts_with(LISTEN_URL));
        assert!(url.contains("model=nova-3"));
        assert!(url.contains("language=ja"));
        assert!(url.contains("encoding=linear16"));
        assert!(url.contains("sample_rate=16000"));
        assert!(url.contains("interim_results=true"));
        assert!(!url.contains("utterance_end_ms"));
    }

    #[test]
    fn build_url_adds_utterance_end_ms_only_when_vad_events_enabled() {
        let config = SttSessionConfig::new(16_000).with_vad_events(true);
        let url = build_url(DEFAULT_MODEL, &config);
        assert!(url.contains("vad_events=true"));
        assert!(url.contains("utterance_end_ms=1000"));
    }

    #[test]
    fn parses_partial_results_message() {
        let raw = r#"{
            "type": "Results",
            "channel": { "alternatives": [{ "transcript": "こんにちは" }] },
            "is_final": false,
            "start": 1.0,
            "duration": 0.5
        }"#;
        let msg: DeepgramMessage = serde_json::from_str(raw).unwrap();
        let DeepgramMessage::Results(results) = msg else {
            panic!("expected Results variant");
        };
        let event = translate_results(results).unwrap();
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
    fn parses_final_results_message_with_words() {
        let raw = r#"{
            "type": "Results",
            "channel": {
                "alternatives": [{
                    "transcript": "こんにちは",
                    "words": [{ "word": "こんにちは", "start": 1.0, "end": 1.5, "confidence": 0.98 }]
                }]
            },
            "is_final": true,
            "speech_final": true,
            "start": 1.0,
            "duration": 0.5
        }"#;
        let msg: DeepgramMessage = serde_json::from_str(raw).unwrap();
        let DeepgramMessage::Results(results) = msg else {
            panic!("expected Results variant");
        };
        let event = translate_results(results).unwrap();
        match event {
            SttEvent::FinalTranscript { text, words, .. } => {
                assert_eq!(text, "こんにちは");
                let words = words.unwrap();
                assert_eq!(words.len(), 1);
                assert_eq!(words[0].text, "こんにちは");
            }
            other => panic!("expected FinalTranscript, got {other:?}"),
        }
    }

    #[test]
    fn empty_transcript_is_dropped() {
        let raw = r#"{
            "type": "Results",
            "channel": { "alternatives": [{ "transcript": "" }] },
            "is_final": false
        }"#;
        let msg: DeepgramMessage = serde_json::from_str(raw).unwrap();
        let DeepgramMessage::Results(results) = msg else {
            panic!("expected Results variant");
        };
        assert!(translate_results(results).is_none());
    }

    #[test]
    fn parses_metadata_message() {
        let raw = r#"{"type":"Metadata","request_id":"abc","duration":1.5,"channels":1}"#;
        let msg: DeepgramMessage = serde_json::from_str(raw).unwrap();
        assert!(matches!(msg, DeepgramMessage::Metadata));
    }

    #[test]
    fn parses_speech_started_and_utterance_end() {
        let started: DeepgramMessage =
            serde_json::from_str(r#"{"type":"SpeechStarted","channel":[0,1]}"#).unwrap();
        assert!(matches!(started, DeepgramMessage::SpeechStarted));

        let ended: DeepgramMessage =
            serde_json::from_str(r#"{"type":"UtteranceEnd","channel":[0,1],"last_word_end":3.1}"#)
                .unwrap();
        assert!(matches!(ended, DeepgramMessage::UtteranceEnd));
    }

    #[test]
    fn unknown_message_type_does_not_error() {
        let msg: DeepgramMessage = serde_json::from_str(r#"{"type":"SomethingNew"}"#).unwrap();
        assert!(matches!(msg, DeepgramMessage::Unknown));
    }

    #[tokio::test]
    async fn keep_alive_sends_keep_alive_command_and_reports_control_message() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let mut session = DeepgramSession {
            commands: cmd_tx,
            drained: None,
        };

        let effect = session.keep_alive().await.unwrap();
        assert_eq!(effect, KeepAliveEffect::ControlMessage);

        let sent = cmd_rx.try_recv().expect("expected a queued command");
        assert!(matches!(sent, WsCommand::KeepAlive));
    }

    #[tokio::test]
    async fn send_audio_returns_retryable_transport_error_when_queue_is_full() {
        let (cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut session = DeepgramSession {
            commands: cmd_tx,
            drained: None,
        };
        // Fill the one queue slot without draining it, so the next send finds no room.
        session
            .send_audio(AudioChunk {
                pcm: &[0.0],
                start_sample: 0,
            })
            .await
            .unwrap();

        let err = session
            .send_audio(AudioChunk {
                pcm: &[0.0],
                start_sample: 1,
            })
            .await
            .unwrap_err();

        assert!(matches!(err, SttError::Transport(_)));
        assert!(err.is_retryable());

        drop(cmd_rx);
    }

    #[tokio::test]
    async fn send_audio_returns_session_closed_when_receiver_dropped() {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        drop(cmd_rx);
        let mut session = DeepgramSession {
            commands: cmd_tx,
            drained: None,
        };

        let err = session
            .send_audio(AudioChunk {
                pcm: &[0.0],
                start_sample: 0,
            })
            .await
            .unwrap_err();

        assert!(matches!(err, SttError::SessionClosed));
    }
}
