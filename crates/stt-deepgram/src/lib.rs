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
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
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
    Close,
}

/// Holds only a command-channel sender, never the WebSocket itself, so this type
/// stays trivially `Send` regardless of whether the underlying TLS stream is —
/// the actual socket lives in `writer_task`/`reader_task` instead.
struct DeepgramSession {
    commands: mpsc::UnboundedSender<WsCommand>,
    drained: Option<oneshot::Receiver<Result<(), SttError>>>,
}

#[async_trait]
impl SttSession for DeepgramSession {
    async fn send_audio(&mut self, chunk: AudioChunk<'_>) -> Result<(), SttError> {
        let mut bytes = Vec::with_capacity(chunk.pcm.len() * 2);
        for &sample in chunk.pcm {
            let clamped = sample.clamp(-1.0, 1.0);
            let pcm16 = (clamped * i16::MAX as f32).round() as i16;
            bytes.extend_from_slice(&pcm16.to_le_bytes());
        }
        self.commands
            .send(WsCommand::Audio(bytes))
            .map_err(|_| SttError::SessionClosed)
    }

    async fn finalize(mut self: Box<Self>) -> Result<(), SttError> {
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

fn build_url(model: &str, config: &SttSessionConfig) -> String {
    let language = config.language.clone().unwrap_or_else(|| "ja".to_string());

    let mut params: Vec<(&str, String)> = vec![
        ("model", model.to_string()),
        ("language", language),
        ("encoding", "linear16".to_string()),
        ("sample_rate", config.sample_rate_hz.to_string()),
        ("channels", "1".to_string()),
        ("interim_results", config.interim_results.to_string()),
        ("punctuate", "true".to_string()),
        ("vad_events", config.vad_events.to_string()),
    ];
    if config.diarization {
        params.push(("diarize", "true".to_string()));
    }
    if config.vad_events {
        params.push(("utterance_end_ms", UTTERANCE_END_MS.to_string()));
    }
    if let Some(boost) = &config.extra.vocabulary_boost {
        for word in boost {
            params.push(("keywords", word.clone()));
        }
    }

    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
        .finish();
    format!("{LISTEN_URL}?{query}")
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
}
