//! `stt-api` adapter for OpenAI's Realtime transcription API (WebSocket).
//!
//! Protocol summary (confirmed 2026-07-17 against
//! <https://developers.openai.com/api/docs/guides/realtime-transcription>,
//! <https://developers.openai.com/api/docs/guides/realtime-websocket>, and the
//! `realtime-server-events`/`realtime-client-events` API reference pages — see
//! `stt-transcription-architecture.md` §2.2 at the repository root for this
//! provider's row in the cross-provider comparison table): connect to
//! `wss://api.openai.com/v1/realtime` with an `Authorization: Bearer <key>` header
//! (the GA API dropped the beta-era `OpenAI-Beta: realtime=v1` header requirement),
//! then send a `session.update` client event with `session.type: "transcription"` to
//! configure the transcription model/language/turn-detection before any audio.
//! Audio is sent as base64-encoded PCM16 little-endian wrapped in
//! `{"type":"input_audio_buffer.append","audio":"..."}` JSON text frames — unlike
//! Deepgram, there is no raw-binary-frame path for this API. Results arrive as
//! `conversation.item.input_audio_transcription.delta` (incremental text) and
//! `.completed` (final text for one segmented turn) JSON text frames, correlated by
//! `item_id`. `{"type":"input_audio_buffer.commit"}` forces the buffered audio to be
//! transcribed immediately; this crate uses it as the finalize-drain trigger, waiting
//! for the `completed`/`failed` event whose `item_id` matches the `committed`
//! acknowledgement produced by that specific commit.
//!
//! Unlike `stt-deepgram` (which accepts whatever `SttSessionConfig::sample_rate_hz`
//! the caller passes and forwards it to Deepgram's `sample_rate` query param), this
//! crate requires exactly 24kHz mono PCM16 — the sample rate OpenAI's realtime audio
//! input format is documented for — and does **not** resample. Callers must resample
//! before calling [`SttSession::send_audio`]; [`OpenAiProvider::start_session`]
//! rejects any other rate with [`SttError::PermanentError`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use stt_api::{AudioChunk, SttError, SttEvent, SttProvider, SttSession, SttSessionConfig};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// `credential-store` service/account this crate's API key is expected under
/// (design.md §12.4), matching `stt-deepgram::CREDENTIAL_SERVICE`/
/// `DEEPGRAM_API_KEY_ACCOUNT`'s pattern. Deliberately a different account name than
/// `summarize::OPENAI_API_KEY_ACCOUNT` — this is a Realtime-transcription-scoped key,
/// not the summarization key, even though both happen to be issued by OpenAI.
pub const CREDENTIAL_SERVICE: &str = "1on1-recorder";
pub const OPENAI_STT_API_KEY_ACCOUNT: &str = "openai-stt-api-key";

const REALTIME_URL: &str = "wss://api.openai.com/v1/realtime";
/// `gpt-realtime-whisper`: the current (as of 2026-07-17) GA model documented as the
/// low-latency streaming choice for realtime transcription
/// (<https://developers.openai.com/api/docs/models/gpt-realtime-whisper>).
/// Deliberately not `whisper-1`/`gpt-4o-transcribe`/`gpt-4o-mini-transcribe` — those
/// are the batch-oriented models task #41 called out as approaching a 2026-06
/// deprecation window; `gpt-realtime-whisper` is not on that list.
const DEFAULT_MODEL: &str = "gpt-realtime-whisper";
/// OpenAI's realtime audio input format is documented as 16-bit PCM at a 24kHz
/// sample rate, single channel — not configurable per-session like Deepgram's
/// `sample_rate` query param.
const REQUIRED_SAMPLE_RATE_HZ: u32 = 24_000;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A configured OpenAI Realtime transcription provider. One instance can open many
/// sessions.
///
/// **Diarization is not supported**: `SttSessionConfig::diarization` is accepted but
/// ignored (never sent to the API), and [`Word::speaker`] is always `None` — OpenAI's
/// realtime transcription `completed` event returns a flat `transcript` string with
/// no per-word data at all, so there is nothing to attach a speaker label to even in
/// principle. (A separate `gpt-4o-transcribe-diarize` model exists for
/// speaker-labeled *batch* transcription, but that is a different API surface than
/// this crate implements.)
pub struct OpenAiProvider {
    api_key: String,
    model: String,
}

impl OpenAiProvider {
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
impl SttProvider for OpenAiProvider {
    async fn start_session(
        &self,
        config: SttSessionConfig,
    ) -> Result<(Box<dyn SttSession>, mpsc::UnboundedReceiver<SttEvent>), SttError> {
        if config.sample_rate_hz != REQUIRED_SAMPLE_RATE_HZ {
            return Err(SttError::PermanentError(format!(
                "sample_rate_hz must be {REQUIRED_SAMPLE_RATE_HZ} (OpenAI Realtime \
                 transcription requires 24kHz mono PCM16 and this crate does not \
                 resample — the caller must resample before calling send_audio), \
                 got {}",
                config.sample_rate_hz
            )));
        }

        let request = build_request(&self.api_key)?;
        let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(map_connect_error)?;

        let (write, read) = ws_stream.split();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (drained_tx, drained_rx) = oneshot::channel();
        let draining = Arc::new(AtomicBool::new(false));

        // Queued before returning the session to the caller, so `session.update`
        // is guaranteed to reach the server before any `send_audio` call's
        // `input_audio_buffer.append`.
        let _ = cmd_tx.send(WsCommand::Json(build_session_update(&self.model, &config)));

        tokio::spawn(writer_task(write, cmd_rx, draining.clone()));
        tokio::spawn(reader_task(
            read,
            event_tx,
            drained_tx,
            draining,
            config.interim_results,
            config.vad_events,
        ));

        Ok((
            Box::new(OpenAiSession {
                commands: cmd_tx,
                drained: Some(drained_rx),
            }),
            event_rx,
        ))
    }
}

enum WsCommand {
    Json(Value),
    Audio(Vec<u8>),
    Commit,
}

/// Holds only a command-channel sender, never the WebSocket itself, so this type
/// stays trivially `Send` regardless of whether the underlying TLS stream is — the
/// actual socket lives in `writer_task`/`reader_task` instead (same pattern as
/// `stt-deepgram::DeepgramSession`).
struct OpenAiSession {
    commands: mpsc::UnboundedSender<WsCommand>,
    drained: Option<oneshot::Receiver<Result<(), SttError>>>,
}

#[async_trait]
impl SttSession for OpenAiSession {
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
            .send(WsCommand::Commit)
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
    draining: Arc<AtomicBool>,
) {
    while let Some(cmd) = commands.recv().await {
        let result = match cmd {
            WsCommand::Json(value) => write.send(Message::Text(value.to_string())).await,
            WsCommand::Audio(bytes) => {
                let audio = json!({
                    "type": "input_audio_buffer.append",
                    "audio": BASE64.encode(bytes),
                });
                write.send(Message::Text(audio.to_string())).await
            }
            WsCommand::Commit => {
                // Set *before* sending, so the reader task never mistakes an
                // in-flight (auto-committed-by-VAD) `completed` event that was
                // already queued for delivery as this commit's answer — see the
                // module doc comment and `reader_task`'s `expected_item_id` logic.
                draining.store(true, Ordering::SeqCst);
                write
                    .send(Message::Text(
                        r#"{"type":"input_audio_buffer.commit"}"#.to_string(),
                    ))
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
    draining: Arc<AtomicBool>,
    emit_interim: bool,
    emit_vad_events: bool,
) {
    let mut drained = Some(drained);
    // Accumulates delta text per `item_id`, so `PartialTranscript::text` reports the
    // full text-so-far for the in-progress turn (matching `stt-deepgram`'s semantics
    // of a cumulative `alternative.transcript` per partial result) rather than just
    // the latest incremental `delta` chunk OpenAI's event carries.
    let mut partials: HashMap<String, String> = HashMap::new();
    // Captured from the first `input_audio_buffer.committed` event seen after
    // `finalize()` requested a commit; `None` means either no commit was requested
    // yet, or one was requested but the `committed` ack hasn't arrived yet.
    let mut expected_item_id: Option<String> = None;

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

        match serde_json::from_str::<OpenAiServerEvent>(&text) {
            Ok(OpenAiServerEvent::InputAudioBufferCommitted { item_id }) => {
                // Keep overwriting rather than latching onto the first commit seen
                // while draining: `finalize()` sends at most one commit and never
                // sends more audio afterward (it consumes `self: Box<Self>`), so our
                // own manual commit is always the *last* `committed` event to arrive
                // while draining — the first one can be a VAD auto-commit for an
                // unrelated turn that was already in flight when finalize() ran, and
                // locking onto it would mean our own commit's `completed` event is
                // never recognized (see the module doc comment's race description).
                if draining.load(Ordering::SeqCst) {
                    expected_item_id = Some(item_id);
                }
            }
            Ok(OpenAiServerEvent::InputAudioBufferSpeechStarted) => {
                if emit_vad_events {
                    let _ = events.send(SttEvent::SpeechStarted);
                }
            }
            Ok(OpenAiServerEvent::InputAudioBufferSpeechStopped) => {
                if emit_vad_events {
                    let _ = events.send(SttEvent::SpeechEnded);
                }
            }
            Ok(OpenAiServerEvent::TranscriptionDelta { item_id, delta }) => {
                let accumulated = partials.entry(item_id).or_default();
                accumulated.push_str(&delta);
                if emit_interim {
                    let _ = events.send(SttEvent::PartialTranscript {
                        text: accumulated.clone(),
                        audio_start_ms: None,
                        audio_end_ms: None,
                        extra: Default::default(),
                    });
                }
            }
            Ok(OpenAiServerEvent::TranscriptionCompleted { item_id, transcript }) => {
                partials.remove(&item_id);
                let _ = events.send(SttEvent::FinalTranscript {
                    text: transcript,
                    // OpenAI's `completed` event returns a flat `transcript` string,
                    // no per-word timestamps — see module doc comment.
                    words: None,
                    audio_start_ms: None,
                    audio_end_ms: None,
                    extra: Default::default(),
                });
                if draining.load(Ordering::SeqCst) && expected_item_id.as_deref() == Some(&item_id)
                {
                    if let Some(tx) = drained.take() {
                        let _ = tx.send(Ok(()));
                    }
                    break;
                }
            }
            Ok(OpenAiServerEvent::TranscriptionFailed { item_id, error }) => {
                partials.remove(&item_id);
                let _ = events.send(SttEvent::Error(SttError::PermanentError(error.to_string())));
                if draining.load(Ordering::SeqCst) && expected_item_id.as_deref() == Some(&item_id)
                {
                    // The commit itself succeeded (we got an ack for it) but that
                    // turn's transcription failed; nothing more will arrive for it,
                    // so finalize() can still return successfully — the failure was
                    // already surfaced via `SttEvent::Error` above.
                    if let Some(tx) = drained.take() {
                        let _ = tx.send(Ok(()));
                    }
                    break;
                }
            }
            Ok(OpenAiServerEvent::Error { error }) => {
                let stt_err = SttError::PermanentError(error.to_string());
                let _ = events.send(SttEvent::Error(stt_err));
                // A commit on an empty/too-short buffer is rejected with a bare
                // `error` event rather than a `committed` ack, so this is the only
                // signal we'll ever see for that commit attempt.
                if draining.load(Ordering::SeqCst) && expected_item_id.is_none() {
                    if let Some(tx) = drained.take() {
                        let _ = tx.send(Ok(()));
                    }
                    break;
                }
            }
            Ok(OpenAiServerEvent::Unknown) => {
                tracing::debug!(%text, "unrecognized openai realtime message type");
            }
            Err(err) => {
                tracing::debug!(%text, %err, "failed to parse openai realtime message");
            }
        }
    }

    if let Some(tx) = drained.take() {
        let _ = tx.send(Err(SttError::Transport(
            "connection closed before the commit-triggered transcription drained"
                .to_string(),
        )));
    }
}

fn build_session_update(model: &str, config: &SttSessionConfig) -> Value {
    let language = config.language.clone().unwrap_or_else(|| "ja".to_string());
    let prompt = config
        .extra
        .vocabulary_boost
        .as_ref()
        .filter(|words| !words.is_empty())
        .map(|words| words.join(", "));

    // Tied directly to `vad_events`: enabling server-side VAD (`turn_detection`) is
    // also what makes OpenAI auto-segment audio into successive turns (each getting
    // its own `completed` event), not just what gates `SpeechStarted`/`SpeechEnded`.
    // With `vad_events: false`, `turn_detection` is `null` and no `completed` event
    // fires until `finalize()`'s manual commit — i.e. exactly one `FinalTranscript`
    // for the whole session. Set `vad_events: true` for continuous per-utterance
    // finals, matching `stt-deepgram`'s behavior.
    let turn_detection = if config.vad_events {
        json!({ "type": "server_vad" })
    } else {
        Value::Null
    };

    let mut transcription = json!({
        "model": model,
        "language": language,
    });
    if let (Some(prompt), Value::Object(map)) = (prompt, &mut transcription) {
        map.insert("prompt".to_string(), Value::String(prompt));
    }

    json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": REQUIRED_SAMPLE_RATE_HZ },
                    "transcription": transcription,
                    "turn_detection": turn_detection,
                }
            }
        }
    })
}

fn build_request(
    api_key: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, SttError> {
    let mut request = REALTIME_URL
        .into_client_request()
        .map_err(|err| SttError::Transport(err.to_string()))?;
    let header_value = HeaderValue::from_str(&format!("Bearer {api_key}"))
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
enum OpenAiServerEvent {
    #[serde(rename = "input_audio_buffer.committed")]
    InputAudioBufferCommitted { item_id: String },
    #[serde(rename = "input_audio_buffer.speech_started")]
    InputAudioBufferSpeechStarted,
    #[serde(rename = "input_audio_buffer.speech_stopped")]
    InputAudioBufferSpeechStopped,
    #[serde(rename = "conversation.item.input_audio_transcription.delta")]
    TranscriptionDelta { item_id: String, delta: String },
    #[serde(rename = "conversation.item.input_audio_transcription.completed")]
    TranscriptionCompleted { item_id: String, transcript: String },
    #[serde(rename = "conversation.item.input_audio_transcription.failed")]
    TranscriptionFailed {
        item_id: String,
        error: serde_json::Value,
    },
    #[serde(rename = "error")]
    Error { error: serde_json::Value },
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_24khz_sample_rate() {
        let provider = OpenAiProvider::new("test-key");
        let config = SttSessionConfig::new(16_000);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(provider.start_session(config));
        match result {
            Err(SttError::PermanentError(msg)) => {
                assert!(msg.contains("24000"), "unexpected message: {msg}");
            }
            Err(other) => panic!("expected PermanentError, got {other:?}"),
            Ok(_) => panic!("expected PermanentError, got Ok"),
        }
    }

    #[test]
    fn build_request_sets_bearer_auth_header() {
        let request = build_request("sk-test").unwrap();
        let header = request.headers().get("Authorization").unwrap();
        assert_eq!(header.to_str().unwrap(), "Bearer sk-test");
        assert_eq!(request.uri().to_string(), REALTIME_URL);
    }

    #[test]
    fn build_session_update_defaults_language_and_disables_turn_detection() {
        let config = SttSessionConfig::new(REQUIRED_SAMPLE_RATE_HZ);
        let value = build_session_update(DEFAULT_MODEL, &config);
        assert_eq!(value["type"], "session.update");
        assert_eq!(value["session"]["type"], "transcription");
        assert_eq!(
            value["session"]["audio"]["input"]["format"]["type"],
            "audio/pcm"
        );
        assert_eq!(
            value["session"]["audio"]["input"]["format"]["rate"],
            REQUIRED_SAMPLE_RATE_HZ
        );
        assert_eq!(
            value["session"]["audio"]["input"]["transcription"]["model"],
            DEFAULT_MODEL
        );
        assert_eq!(
            value["session"]["audio"]["input"]["transcription"]["language"],
            "ja"
        );
        assert!(value["session"]["audio"]["input"]["turn_detection"].is_null());
    }

    #[test]
    fn build_session_update_enables_server_vad_when_vad_events_requested() {
        let config = SttSessionConfig::new(REQUIRED_SAMPLE_RATE_HZ).with_vad_events(true);
        let value = build_session_update(DEFAULT_MODEL, &config);
        assert_eq!(
            value["session"]["audio"]["input"]["turn_detection"]["type"],
            "server_vad"
        );
    }

    #[test]
    fn build_session_update_joins_vocabulary_boost_into_prompt() {
        let config = SttSessionConfig::new(REQUIRED_SAMPLE_RATE_HZ).with_extra(
            stt_api::SttExtraRequest::default()
                .with_vocabulary_boost(vec!["Kubernetes".to_string(), "kubectl".to_string()]),
        );
        let value = build_session_update(DEFAULT_MODEL, &config);
        assert_eq!(
            value["session"]["audio"]["input"]["transcription"]["prompt"],
            "Kubernetes, kubectl"
        );
    }

    #[test]
    fn parses_delta_and_completed_events() {
        let delta: OpenAiServerEvent = serde_json::from_str(
            r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"item_1","content_index":0,"delta":"こんにちは"}"#,
        )
        .unwrap();
        assert!(matches!(
            delta,
            OpenAiServerEvent::TranscriptionDelta { item_id, delta }
                if item_id == "item_1" && delta == "こんにちは"
        ));

        let completed: OpenAiServerEvent = serde_json::from_str(
            r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"item_1","content_index":0,"transcript":"こんにちは、元気ですか"}"#,
        )
        .unwrap();
        assert!(matches!(
            completed,
            OpenAiServerEvent::TranscriptionCompleted { item_id, transcript }
                if item_id == "item_1" && transcript == "こんにちは、元気ですか"
        ));
    }

    #[test]
    fn parses_committed_and_speech_events() {
        let committed: OpenAiServerEvent = serde_json::from_str(
            r#"{"type":"input_audio_buffer.committed","event_id":"evt_1","item_id":"item_2"}"#,
        )
        .unwrap();
        assert!(matches!(
            committed,
            OpenAiServerEvent::InputAudioBufferCommitted { item_id } if item_id == "item_2"
        ));

        let started: OpenAiServerEvent =
            serde_json::from_str(r#"{"type":"input_audio_buffer.speech_started"}"#).unwrap();
        assert!(matches!(
            started,
            OpenAiServerEvent::InputAudioBufferSpeechStarted
        ));

        let stopped: OpenAiServerEvent =
            serde_json::from_str(r#"{"type":"input_audio_buffer.speech_stopped"}"#).unwrap();
        assert!(matches!(
            stopped,
            OpenAiServerEvent::InputAudioBufferSpeechStopped
        ));
    }

    #[test]
    fn unknown_message_type_does_not_error() {
        let msg: OpenAiServerEvent = serde_json::from_str(r#"{"type":"session.created"}"#).unwrap();
        assert!(matches!(msg, OpenAiServerEvent::Unknown));
    }

    #[test]
    fn parses_failed_and_error_events() {
        let failed: OpenAiServerEvent = serde_json::from_str(
            r#"{"type":"conversation.item.input_audio_transcription.failed","item_id":"item_3","content_index":0,"error":{"message":"boom"}}"#,
        )
        .unwrap();
        assert!(matches!(
            failed,
            OpenAiServerEvent::TranscriptionFailed { item_id, .. } if item_id == "item_3"
        ));

        let error: OpenAiServerEvent = serde_json::from_str(
            r#"{"type":"error","error":{"message":"cannot commit an empty input audio buffer"}}"#,
        )
        .unwrap();
        assert!(matches!(error, OpenAiServerEvent::Error { .. }));
    }
}
