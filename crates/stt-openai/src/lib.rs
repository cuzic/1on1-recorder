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
use stt_api::{
    AudioChunk, KeepAliveEffect, SttError, SttEvent, SttProvider, SttSession, SttSessionConfig,
};
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
/// Duration of the silent PCM buffer [`OpenAiSession::keep_alive`] injects per call.
/// OpenAI's realtime idle-timeout behavior isn't documented precisely enough to rely
/// on a no-cost keep-alive frame existing, so — same policy as `stt-google` — this
/// sends a short burst of real (silent) audio often enough to keep the connection
/// from going idle, rather than risking the session getting dropped.
const KEEP_ALIVE_SILENCE_MS: u32 = 100;
/// [`KEEP_ALIVE_SILENCE_MS`] of silence at [`REQUIRED_SAMPLE_RATE_HZ`], in samples.
const KEEP_ALIVE_SILENCE_SAMPLES: u32 = REQUIRED_SAMPLE_RATE_HZ / 1000 * KEEP_ALIVE_SILENCE_MS;

/// Capacity of the `WsCommand` channel `send_audio`/`keep_alive` push onto and
/// `writer_task` drains. Bounded (not `mpsc::unbounded_channel`) so that a TCP
/// write that stalls without erroring — e.g. the peer stops reading but doesn't
/// reset the connection — can't grow this queue's PCM backlog without limit; once
/// `writer_task` falls this far behind, `send_audio`/`keep_alive` start returning
/// `SttError::Transport` (see their `try_send` calls below) instead of continuing
/// to buffer. Sized in *messages*, not bytes, since `WsCommand::Audio` chunks are
/// caller-sized (capture-side chunks are on the order of a 10ms device period —
/// see the capture crates); at that cadence 256 queued messages is on the order
/// of a couple of seconds of audio, enough slack to absorb an ordinary scheduling
/// stall without either running away in memory or rejecting audio on routine
/// jitter.
const AUDIO_COMMAND_CHANNEL_CAPACITY: usize = 256;

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
        let (cmd_tx, cmd_rx) = mpsc::channel(AUDIO_COMMAND_CHANNEL_CAPACITY);
        let (drained_tx, drained_rx) = oneshot::channel();
        let draining = Arc::new(AtomicBool::new(false));

        // Queued before returning the session to the caller, so `session.update`
        // is guaranteed to reach the server before any `send_audio` call's
        // `input_audio_buffer.append`. `.await` rather than `try_send`: the channel
        // was just created and nothing else has queued onto it yet, so this never
        // actually blocks.
        let _ = cmd_tx
            .send(WsCommand::Json(build_session_update(&self.model, &config)))
            .await;

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

#[derive(Debug)]
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
    commands: mpsc::Sender<WsCommand>,
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
            .try_send(WsCommand::Audio(bytes))
            .map_err(send_error_to_stt_error)
    }

    async fn finalize(mut self: Box<Self>) -> Result<(), SttError> {
        // `.send(...).await` rather than `try_send`: a commit must reach
        // `writer_task` strictly after every `WsCommand::Audio` already queued
        // ahead of it (the reader task matches the commit's `completed` event by
        // `item_id`, so if audio the commit was meant to flush got dropped instead
        // the drain would hang or resolve on the wrong turn — see the module doc
        // comment). Waiting for queue space here preserves that ordering instead
        // of racing a retry loop against `writer_task`'s drain rate; `finalize()`
        // is called at most once per session and #81 is adding a timeout around
        // it, which bounds how long this can block.
        self.commands
            .send(WsCommand::Commit)
            .await
            .map_err(|_| SttError::SessionClosed)?;
        match self.drained.take() {
            Some(rx) => rx.await.map_err(|_| SttError::SessionClosed)?,
            None => Ok(()),
        }
    }

    /// OpenAI's realtime idle-timeout behavior isn't documented precisely enough to
    /// trust an unpublished no-cost keep-alive frame, so — matching `stt-google`'s
    /// policy — this sends a short burst of genuinely silent PCM16 audio through the
    /// same `input_audio_buffer.append` path `send_audio` uses, rather than a
    /// protocol-level ping. This does advance OpenAI's audio timeline, hence
    /// `InjectedAudio` (not `ControlMessage`).
    async fn keep_alive(&mut self) -> Result<KeepAliveEffect, SttError> {
        let silence = vec![0u8; KEEP_ALIVE_SILENCE_SAMPLES as usize * 2];
        self.commands
            .try_send(WsCommand::Audio(silence))
            .map_err(send_error_to_stt_error)?;
        Ok(KeepAliveEffect::InjectedAudio {
            samples: KEEP_ALIVE_SILENCE_SAMPLES as u64,
        })
    }
}

/// Shared by `send_audio`/`keep_alive`'s `try_send` calls: `Full` means
/// `writer_task` is backlogged (see [`AUDIO_COMMAND_CHANNEL_CAPACITY`]'s doc
/// comment) rather than gone, so it's `Transport` — retryable via
/// `SttError::is_retryable` — and not the permanent `SessionClosed`.
fn send_error_to_stt_error(err: mpsc::error::TrySendError<WsCommand>) -> SttError {
    match err {
        mpsc::error::TrySendError::Full(_) => SttError::Transport(
            "openai realtime writer task is backlogged: audio command queue is full"
                .to_string(),
        ),
        mpsc::error::TrySendError::Closed(_) => SttError::SessionClosed,
    }
}

async fn writer_task(
    mut write: SplitSink<WsStream, Message>,
    mut commands: mpsc::Receiver<WsCommand>,
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
    fn keep_alive_sends_100ms_of_silence_and_reports_samples_sent() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(AUDIO_COMMAND_CHANNEL_CAPACITY);
        let mut session = OpenAiSession {
            commands: cmd_tx,
            drained: None,
        };

        let effect = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(session.keep_alive())
            .unwrap();

        assert_eq!(
            effect,
            KeepAliveEffect::InjectedAudio {
                samples: KEEP_ALIVE_SILENCE_SAMPLES as u64,
            }
        );

        match cmd_rx.try_recv() {
            Ok(WsCommand::Audio(bytes)) => {
                // PCM16LE, all-zero bytes: 2 bytes/sample, every byte silent.
                assert_eq!(bytes.len(), KEEP_ALIVE_SILENCE_SAMPLES as usize * 2);
                assert!(bytes.iter().all(|&b| b == 0));
            }
            other => panic!("expected WsCommand::Audio, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_audio_reports_retryable_transport_error_when_channel_is_full() {
        // Capacity 1: the one slot is filled by the Json session-update queued at
        // `start_session` time in real usage; here we fill it manually so the very
        // next `send_audio` call observes `TrySendError::Full`.
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        cmd_tx.try_send(WsCommand::Json(json!({}))).unwrap();
        let mut session = OpenAiSession {
            commands: cmd_tx,
            drained: None,
        };

        let pcm = [0.0f32; 4];
        let err = session
            .send_audio(AudioChunk {
                pcm: &pcm,
                start_sample: 0,
            })
            .await
            .expect_err("try_send should fail once the channel is full");

        assert!(matches!(err, SttError::Transport(_)));
        assert!(err.is_retryable(), "a full channel should be retryable");

        // The one slot still holds the original Json command; the Audio command was
        // rejected outright rather than silently queued or dropped-and-lost.
        assert!(matches!(cmd_rx.try_recv(), Ok(WsCommand::Json(_))));
    }

    #[tokio::test]
    async fn send_audio_reports_session_closed_when_receiver_dropped() {
        let (cmd_tx, cmd_rx) = mpsc::channel(AUDIO_COMMAND_CHANNEL_CAPACITY);
        drop(cmd_rx);
        let mut session = OpenAiSession {
            commands: cmd_tx,
            drained: None,
        };

        let pcm = [0.0f32; 4];
        let err = session
            .send_audio(AudioChunk {
                pcm: &pcm,
                start_sample: 0,
            })
            .await
            .expect_err("send_audio should fail once the receiver is gone");

        assert!(matches!(err, SttError::SessionClosed));
    }

    #[tokio::test]
    async fn keep_alive_reports_retryable_transport_error_when_channel_is_full() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        cmd_tx.try_send(WsCommand::Commit).unwrap();
        let mut session = OpenAiSession {
            commands: cmd_tx,
            drained: None,
        };

        let err = session
            .keep_alive()
            .await
            .expect_err("try_send should fail once the channel is full");

        assert!(matches!(err, SttError::Transport(_)));
        assert!(err.is_retryable(), "a full channel should be retryable");
        assert!(matches!(cmd_rx.try_recv(), Ok(WsCommand::Commit)));
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
