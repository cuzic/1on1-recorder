//! `stt-api` adapter for Google Cloud Speech-to-Text v2's `StreamingRecognize` RPC.
//!
//! Protocol summary (verified 2026-07-17 against
//! <https://docs.cloud.google.com/speech-to-text/docs/reference/rpc/google.cloud.speech.v2>
//! and <https://docs.cloud.google.com/speech-to-text/docs/streaming-recognize>, and
//! against the real `google/cloud/speech/v2/cloud_speech.proto` — see
//! `proto/google/cloud/speech/v2/cloud_speech.proto`'s header comment for exactly
//! what was vendored from it): `StreamingRecognize` is a bidirectional-streaming gRPC
//! method, no REST/WebSocket equivalent exists. The first
//! `StreamingRecognizeRequest` on the stream carries `recognizer` (a resource path
//! `projects/{project}/locations/{location}/recognizers/_` — `_` selects the
//! implicit default recognizer, no `CreateRecognizer` call needed) plus
//! `streaming_config`; every request after that carries only raw PCM16LE `audio`
//! bytes, split into <=15KB messages per the proto's own field comment on
//! `StreamingRecognizeRequest.audio`. Ending the request stream (this crate does so
//! by returning from the `async_stream::stream!` generator on `finalize`) is what
//! signals end-of-audio; there is no separate close handshake message like
//! Deepgram's `CloseStream`.
//!
//! **Model**: defaults to `chirp_3`, Google's latest multilingual model, GA for
//! `ja-JP` streaming transcription (this project's PoC target is Japanese meetings).
//! Its one relevant gap: per
//! <https://docs.cloud.google.com/speech-to-text/docs/models/chirp-3>, Chirp 3
//! speaker diarization is only available through `BatchRecognize`/`Recognize`, not
//! `StreamingRecognize`. This crate still forwards `SttSessionConfig::diarization`
//! as `RecognitionFeatures.diarization_config` when requested (harmless — the server
//! just won't populate `WordInfo.speaker_label` for a model/method combination that
//! doesn't support it), and leaves `Word::speaker` as `None` whenever
//! `speaker_label` comes back empty, per `stt-api`'s "supported only if the provider
//! actually supports it, never a hard error" contract.
//!
//! **Auth**: Application Default Credentials by default (`gcp_auth::provider()`,
//! which checks `GOOGLE_APPLICATION_CREDENTIALS`, `gcloud auth
//! application-default login` state, and the GCE/Cloud Run metadata server, in that
//! order), or an explicit service-account key (inline JSON or a file path) via
//! [`ServiceAccountSource`]. A single Deepgram-style API-key string doesn't fit this
//! provider: every request also needs a project and a location (endpoint routing —
//! see [`endpoint_uri`]), so [`GoogleSttCredentials`] bundles all three for
//! `credential-store` to persist as one JSON blob under one account.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use stt_api::{
    AudioChunk, KeepAliveEffect, SttError, SttEvent, SttExtraResult, SttProvider, SttSession,
    SttSessionConfig, Word,
};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

mod pb {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/google.cloud.speech.v2.rs"));
}

/// `credential-store` service/account this crate's credentials are expected under
/// (design.md §12.4), matching `stt_deepgram::CREDENTIAL_SERVICE`/
/// `DEEPGRAM_API_KEY_ACCOUNT`'s pattern. The stored secret is
/// [`GoogleSttCredentials`] serialized as JSON (see its doc comment for why a plain
/// string isn't enough here).
pub const CREDENTIAL_SERVICE: &str = "1on1-recorder";
pub const GOOGLE_STT_CREDENTIALS_ACCOUNT: &str = "google-stt-credentials";

const DEFAULT_MODEL: &str = "chirp_3";
const OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// Stays comfortably under the proto's documented 15KB-per-message limit on
/// `StreamingRecognizeRequest.audio`.
const MAX_AUDIO_CHUNK_BYTES: usize = 12_000;
/// Mid-range of the proto's documented 0-20 valid `Phrase.boost` values — enough to
/// bias recognition toward `vocabulary_boost` words without the false-positive risk
/// the proto comment attributes to values near the top of that range.
const VOCABULARY_BOOST_VALUE: f32 = 10.0;
/// Duration of the silent PCM heartbeat `GoogleSession::keep_alive` sends. Well
/// under the ~10s `StreamingRecognize` idle timeout this crate's doc comment
/// describes, so a caller polling this on any reasonable cadence keeps the stream
/// alive with room to spare.
const KEEP_ALIVE_SILENCE_MS: u32 = 100;
/// Bounds how many `Command`s (audio or keep-alive) can queue in
/// `GoogleSession::commands` ahead of `build_outbound_stream` actually writing
/// them to the gRPC connection. This project's only real producer,
/// `app-service`'s WASAPI capture pipeline, calls `send_audio` roughly once per
/// ~10ms capture callback, so 500 slots hold a few seconds of audio — enough to
/// absorb a brief stall in the TCP write path, but small and finite enough that a
/// genuinely stuck connection (`try_send` returning `Full`) is reported back as a
/// retryable error well before Google's own ~10s `StreamingRecognize` idle
/// timeout would have killed the session anyway, instead of growing this queue
/// without limit.
const COMMAND_CHANNEL_CAPACITY: usize = 500;

/// Where to find the service-account key used to mint OAuth tokens, if not relying
/// on Application Default Credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAccountSource {
    /// Resolve credentials the standard ADC way: `GOOGLE_APPLICATION_CREDENTIALS`,
    /// `gcloud`'s user credentials, or the GCE/Cloud Run metadata server.
    ApplicationDefault,
    /// Inline service-account key JSON (the file content, not a path).
    Json(String),
    /// Path to a service-account key JSON file on disk.
    Path(String),
}

/// Everything one `credential-store` entry needs to bundle for this provider: a
/// single API-key string (Deepgram's shape) isn't enough for Google, since every
/// request is also scoped to a project and a location.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleSttCredentials {
    pub project_id: String,
    /// GCP location, e.g. `"global"` (default endpoint, works for most callers) or
    /// a region like `"asia-northeast1"` (data-residency-restricted endpoints; see
    /// [`endpoint_uri`]).
    pub location: String,
    pub service_account: ServiceAccountSource,
}

impl GoogleSttCredentials {
    pub fn new(project_id: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            location: location.into(),
            service_account: ServiceAccountSource::ApplicationDefault,
        }
    }

    pub fn with_service_account_json(mut self, json: impl Into<String>) -> Self {
        self.service_account = ServiceAccountSource::Json(json.into());
        self
    }

    pub fn with_service_account_path(mut self, path: impl Into<String>) -> Self {
        self.service_account = ServiceAccountSource::Path(path.into());
        self
    }
}

/// A configured Google provider. One instance can open many sessions.
pub struct GoogleProvider {
    credentials: GoogleSttCredentials,
    model: String,
}

impl GoogleProvider {
    pub fn new(credentials: GoogleSttCredentials) -> Self {
        Self {
            credentials,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[async_trait]
impl SttProvider for GoogleProvider {
    async fn start_session(
        &self,
        config: SttSessionConfig,
    ) -> Result<(Box<dyn SttSession>, mpsc::UnboundedReceiver<SttEvent>), SttError> {
        if config.sample_rate_hz == 0 {
            return Err(SttError::PermanentError(
                "sample_rate_hz must be nonzero".to_string(),
            ));
        }

        let token = resolve_token(&self.credentials).await?;
        let channel = connect_channel(&self.credentials.location).await?;
        let mut client = pb::speech_client::SpeechClient::new(channel);

        let recognizer = recognizer_path(&self.credentials.project_id, &self.credentials.location);
        let streaming_config = build_streaming_config(&self.model, &config);

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
        let outbound = build_outbound_stream(recognizer, streaming_config, cmd_rx);

        let mut request = tonic::Request::new(outbound);
        let auth_value: tonic::metadata::MetadataValue<_> = format!("Bearer {token}")
            .try_into()
            .map_err(|_| SttError::AuthenticationFailed("invalid bearer token".to_string()))?;
        request.metadata_mut().insert("authorization", auth_value);

        let response = client
            .streaming_recognize(request)
            .await
            .map_err(map_status_error)?;
        let inbound = response.into_inner();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (drained_tx, drained_rx) = oneshot::channel();
        tokio::spawn(reader_task(
            inbound,
            event_tx,
            drained_tx,
            config.vad_events,
        ));

        Ok((
            Box::new(GoogleSession {
                commands: cmd_tx,
                drained: Some(drained_rx),
                sample_rate_hz: config.sample_rate_hz,
            }),
            event_rx,
        ))
    }
}

enum Command {
    Audio(Vec<u8>),
    Close,
}

/// Holds only a command-channel sender, never the gRPC stream itself, so this type
/// stays trivially `Send` regardless of the transport — mirrors
/// `stt_deepgram::DeepgramSession`. The actual request stream lives in the
/// `async_stream::stream!` generator built by `build_outbound_stream`, driven by
/// tonic; the response stream lives in `reader_task`.
struct GoogleSession {
    commands: mpsc::Sender<Command>,
    drained: Option<oneshot::Receiver<Result<(), SttError>>>,
    /// Needed to size the silent PCM16LE buffer `keep_alive` sends (see its doc
    /// comment on why Google's `StreamingRecognize` needs a real audio heartbeat
    /// rather than a protocol-level ping).
    sample_rate_hz: u32,
}

#[async_trait]
impl SttSession for GoogleSession {
    async fn send_audio(&mut self, chunk: AudioChunk<'_>) -> Result<(), SttError> {
        let mut bytes = Vec::with_capacity(chunk.pcm.len() * 2);
        for &sample in chunk.pcm {
            let clamped = sample.clamp(-1.0, 1.0);
            let pcm16 = (clamped * i16::MAX as f32).round() as i16;
            bytes.extend_from_slice(&pcm16.to_le_bytes());
        }
        send_command(&self.commands, Command::Audio(bytes))
    }

    async fn finalize(mut self: Box<Self>) -> Result<(), SttError> {
        // Unlike `send_audio`/`keep_alive`, this can afford to await instead of
        // `try_send`ing: it runs once, after the caller is done producing audio,
        // so blocking here until `build_outbound_stream` drains a full queue
        // (rather than failing) is the correct way to make sure `Close` is seen.
        self.commands
            .send(Command::Close)
            .await
            .map_err(|_| SttError::SessionClosed)?;
        match self.drained.take() {
            Some(rx) => rx.await.map_err(|_| SttError::SessionClosed)?,
            None => Ok(()),
        }
    }

    /// `StreamingRecognize` has no protocol-level keep-alive (see crate root doc
    /// comment) — the only way to stop the ~10s idle timeout from firing during a
    /// silence-skipping gap is to actually advance the audio timeline. This sends
    /// `KEEP_ALIVE_SILENCE_MS` of all-zero PCM16LE through the same
    /// `Command::Audio` path `send_audio` uses, so it's indistinguishable to the
    /// gRPC stream from real (silent) audio.
    async fn keep_alive(&mut self) -> Result<KeepAliveEffect, SttError> {
        let samples = keep_alive_silence_samples(self.sample_rate_hz);
        let bytes = vec![0u8; samples as usize * 2];
        send_command(&self.commands, Command::Audio(bytes))?;
        Ok(KeepAliveEffect::InjectedAudio { samples })
    }
}

/// `send_audio`/`keep_alive`'s shared enqueue path: `try_send` rather than the
/// awaiting `send` so a stalled outbound gRPC write (TCP backpressure) can't make
/// either method hang the caller — instead `Full` surfaces immediately as a
/// retryable `SttError::Transport` once `COMMAND_CHANNEL_CAPACITY` worth of audio
/// is already queued.
fn send_command(commands: &mpsc::Sender<Command>, command: Command) -> Result<(), SttError> {
    commands.try_send(command).map_err(|err| match err {
        TrySendError::Full(_) => {
            SttError::Transport("stt-google: outbound audio queue is full".to_string())
        }
        TrySendError::Closed(_) => SttError::SessionClosed,
    })
}

/// Number of mono samples in `KEEP_ALIVE_SILENCE_MS` at `sample_rate_hz`, i.e. how
/// many silent samples `GoogleSession::keep_alive` sends and reports back via
/// `KeepAliveEffect::InjectedAudio`.
fn keep_alive_silence_samples(sample_rate_hz: u32) -> u64 {
    (sample_rate_hz as u64 * KEEP_ALIVE_SILENCE_MS as u64) / 1000
}

/// Builds the outbound request stream: one `streaming_config` message, then
/// audio-only messages (<=`MAX_AUDIO_CHUNK_BYTES` each) until `Command::Close`
/// (or the sender is dropped) ends the generator — which half-closes the gRPC
/// stream, `StreamingRecognize`'s only end-of-audio signal.
fn build_outbound_stream(
    recognizer: String,
    streaming_config: pb::StreamingRecognitionConfig,
    mut commands: mpsc::Receiver<Command>,
) -> impl futures_core::Stream<Item = pb::StreamingRecognizeRequest> {
    async_stream::stream! {
        yield pb::StreamingRecognizeRequest {
            recognizer,
            streaming_request: Some(
                pb::streaming_recognize_request::StreamingRequest::StreamingConfig(streaming_config),
            ),
        };
        while let Some(cmd) = commands.recv().await {
            match cmd {
                Command::Audio(bytes) => {
                    for piece in bytes.chunks(MAX_AUDIO_CHUNK_BYTES) {
                        yield pb::StreamingRecognizeRequest {
                            recognizer: String::new(),
                            streaming_request: Some(
                                pb::streaming_recognize_request::StreamingRequest::Audio(piece.to_vec()),
                            ),
                        };
                    }
                }
                Command::Close => break,
            }
        }
    }
}

async fn reader_task(
    mut inbound: tonic::codec::Streaming<pb::StreamingRecognizeResponse>,
    events: mpsc::UnboundedSender<SttEvent>,
    drained: oneshot::Sender<Result<(), SttError>>,
    vad_events: bool,
) {
    let mut drained = Some(drained);

    loop {
        match inbound.message().await {
            Ok(Some(response)) => {
                for event in translate_response(response, vad_events) {
                    let _ = events.send(event);
                }
            }
            Ok(None) => break,
            Err(status) => {
                let stt_err = map_status_error(status);
                let _ = events.send(SttEvent::Error(stt_err.clone()));
                if let Some(tx) = drained.take() {
                    let _ = tx.send(Err(stt_err));
                }
                return;
            }
        }
    }

    if let Some(tx) = drained.take() {
        let _ = tx.send(Ok(()));
    }
}

/// `_` selects Speech-to-Text v2's implicit default recognizer, so no
/// `CreateRecognizer` call is needed up front.
fn recognizer_path(project_id: &str, location: &str) -> String {
    format!("projects/{project_id}/locations/{location}/recognizers/_")
}

/// Speech-to-Text v2 requires a location-specific endpoint for every location other
/// than `"global"` (verified 2026-07-17 via
/// <https://docs.cloud.google.com/speech-to-text/docs/samples/speech-multi-region-client>
/// and the Node.js v2 client's endpoint resolution) — calling the global endpoint
/// with a regional resource name returns `NOT_FOUND`.
fn endpoint_uri(location: &str) -> String {
    if location == "global" || location.is_empty() {
        "https://speech.googleapis.com".to_string()
    } else {
        format!("https://{location}-speech.googleapis.com")
    }
}

async fn connect_channel(location: &str) -> Result<Channel, SttError> {
    let endpoint = Endpoint::from_shared(endpoint_uri(location))
        .map_err(|err| SttError::Transport(err.to_string()))?
        .tls_config(ClientTlsConfig::new().with_webpki_roots())
        .map_err(|err| SttError::Transport(err.to_string()))?;
    endpoint
        .connect()
        .await
        .map_err(|err| SttError::Transport(err.to_string()))
}

/// `SttSessionConfig::language`, mapped per this crate's doc comment item 4: `None`
/// or `Some("ja")` both mean "this project's default", `ja-JP`; anything else is
/// passed through as-is (the caller is expected to supply a full BCP-47 tag, as
/// `RecognitionConfig.language_codes` requires).
fn language_code_for(config: &SttSessionConfig) -> String {
    match config.language.as_deref() {
        None | Some("ja") => "ja-JP".to_string(),
        Some(other) => other.to_string(),
    }
}

fn build_streaming_config(
    model: &str,
    config: &SttSessionConfig,
) -> pb::StreamingRecognitionConfig {
    let diarization_config = config
        .diarization
        .then(pb::SpeakerDiarizationConfig::default);

    let adaptation = config.extra.vocabulary_boost.as_ref().map(|words| {
        let phrases = words
            .iter()
            .map(|word| pb::phrase_set::Phrase {
                value: word.clone(),
                boost: VOCABULARY_BOOST_VALUE,
            })
            .collect();
        pb::SpeechAdaptation {
            phrase_sets: vec![pb::speech_adaptation::AdaptationPhraseSet {
                value: Some(
                    pb::speech_adaptation::adaptation_phrase_set::Value::InlinePhraseSet(
                        pb::PhraseSet { phrases },
                    ),
                ),
            }],
        }
    });

    let recognition_config = pb::RecognitionConfig {
        model: model.to_string(),
        language_codes: vec![language_code_for(config)],
        features: Some(pb::RecognitionFeatures {
            enable_word_time_offsets: true,
            enable_word_confidence: true,
            enable_automatic_punctuation: true,
            diarization_config,
        }),
        adaptation,
        // Headerless PCM16LE always needs explicit decoding params (proto's own
        // doc comment on `RecognitionConfig.decoding_config`).
        decoding_config: Some(
            pb::recognition_config::DecodingConfig::ExplicitDecodingConfig(
                pb::ExplicitDecodingConfig {
                    encoding: pb::explicit_decoding_config::AudioEncoding::Linear16 as i32,
                    sample_rate_hertz: config.sample_rate_hz as i32,
                    audio_channel_count: 1,
                },
            ),
        ),
    };

    pb::StreamingRecognitionConfig {
        config: Some(recognition_config),
        streaming_features: Some(pb::StreamingRecognitionFeatures {
            enable_voice_activity_events: config.vad_events,
            interim_results: config.interim_results,
        }),
    }
}

fn translate_response(response: pb::StreamingRecognizeResponse, vad_events: bool) -> Vec<SttEvent> {
    use pb::streaming_recognize_response::SpeechEventType;

    let mut events = Vec::new();

    if vad_events {
        match SpeechEventType::try_from(response.speech_event_type)
            .unwrap_or(SpeechEventType::Unspecified)
        {
            SpeechEventType::SpeechActivityBegin => events.push(SttEvent::SpeechStarted),
            SpeechEventType::SpeechActivityEnd => events.push(SttEvent::SpeechEnded),
            SpeechEventType::Unspecified | SpeechEventType::EndOfSingleUtterance => {}
        }
    }

    events.extend(response.results.into_iter().filter_map(translate_result));
    events
}

fn translate_result(result: pb::StreamingRecognitionResult) -> Option<SttEvent> {
    let is_final = result.is_final;
    let audio_end_ms = result.result_end_offset.as_ref().map(duration_to_ms);
    let language_code = result.language_code;

    let alternative = result.alternatives.into_iter().next()?;
    if alternative.transcript.is_empty() {
        return None;
    }

    // Unlike Deepgram, Google reports only the *end* of a result's audio range
    // (`result_end_offset`); the closest available start is the first word's own
    // start offset, when word-level timing is present.
    let audio_start_ms = alternative
        .words
        .first()
        .and_then(|w| w.start_offset.as_ref())
        .map(duration_to_ms);

    let extra = if language_code.is_empty() {
        SttExtraResult::default()
    } else {
        SttExtraResult::default().with_detected_language(language_code)
    };

    if is_final {
        let words = if alternative.words.is_empty() {
            None
        } else {
            Some(
                alternative
                    .words
                    .into_iter()
                    .map(|w| Word {
                        text: w.word,
                        start_ms: w.start_offset.as_ref().map(duration_to_ms),
                        end_ms: w.end_offset.as_ref().map(duration_to_ms),
                        // 0.0 is the proto's own documented sentinel for "not set".
                        confidence: (w.confidence != 0.0).then_some(w.confidence),
                        speaker: parse_speaker_label(&w.speaker_label),
                    })
                    .collect(),
            )
        };
        Some(SttEvent::FinalTranscript {
            text: alternative.transcript,
            words,
            audio_start_ms,
            audio_end_ms,
            extra,
        })
    } else {
        Some(SttEvent::PartialTranscript {
            text: alternative.transcript,
            audio_start_ms,
            audio_end_ms,
            extra,
        })
    }
}

/// `WordInfo.speaker_label` is only set when diarization is both requested *and*
/// supported for the active model/language/method (see crate root doc comment); an
/// empty label means "not supported here", which this crate reports as `None`
/// rather than an error.
///
/// Google's `speaker_label` is 1-based (confirmed against Google's own diarization
/// sample output and the v1 API's `speaker_tag` doc, "ranges from '1' to
/// diarization_speaker_count"), but `Word::speaker` is documented and consumed
/// elsewhere (`apps/desktop/src/transcript.rs::speaker_label`) as 0-based, matching
/// Deepgram's convention — that shared renderer adds 1 back for display. Without
/// subtracting 1 here, every Google-diarized transcript would display one speaker
/// number higher than the actual speaker. `checked_sub` guards the
/// not-expected-per-Google's-own-docs case of a `"0"` label turning into a
/// nonsensical negative index; that's reported as `None` rather than underflowing.
fn parse_speaker_label(label: &str) -> Option<u32> {
    if label.is_empty() {
        None
    } else {
        label.parse::<u32>().ok().and_then(|n| n.checked_sub(1))
    }
}

fn duration_to_ms(duration: &prost_types::Duration) -> u64 {
    let seconds = duration.seconds.max(0) as u64;
    let nanos = duration.nanos.max(0) as u64;
    seconds * 1000 + nanos / 1_000_000
}

fn map_status_error(status: tonic::Status) -> SttError {
    use tonic::Code;
    match status.code() {
        Code::Unauthenticated | Code::PermissionDenied => {
            SttError::AuthenticationFailed(status.message().to_string())
        }
        Code::ResourceExhausted => SttError::RateLimited,
        Code::DeadlineExceeded => SttError::Timeout,
        Code::Unavailable | Code::Aborted | Code::Internal | Code::Unknown | Code::Cancelled => {
            SttError::Transport(status.message().to_string())
        }
        _ => SttError::PermanentError(status.message().to_string()),
    }
}

async fn resolve_token(credentials: &GoogleSttCredentials) -> Result<String, SttError> {
    use gcp_auth::TokenProvider;

    let scopes = &[OAUTH_SCOPE];
    let token = match &credentials.service_account {
        ServiceAccountSource::ApplicationDefault => {
            let provider = gcp_auth::provider()
                .await
                .map_err(|err| SttError::AuthenticationFailed(err.to_string()))?;
            provider
                .token(scopes)
                .await
                .map_err(|err| SttError::AuthenticationFailed(err.to_string()))?
        }
        ServiceAccountSource::Json(json) => {
            let account = gcp_auth::CustomServiceAccount::from_json(json)
                .map_err(|err| SttError::AuthenticationFailed(err.to_string()))?;
            account
                .token(scopes)
                .await
                .map_err(|err| SttError::AuthenticationFailed(err.to_string()))?
        }
        ServiceAccountSource::Path(path) => {
            let account = gcp_auth::CustomServiceAccount::from_file(path)
                .map_err(|err| SttError::AuthenticationFailed(err.to_string()))?;
            account
                .token(scopes)
                .await
                .map_err(|err| SttError::AuthenticationFailed(err.to_string()))?
        }
    };
    Ok(token.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizer_path_uses_implicit_default_recognizer() {
        assert_eq!(
            recognizer_path("my-project", "global"),
            "projects/my-project/locations/global/recognizers/_"
        );
    }

    #[test]
    fn endpoint_uri_uses_global_host_for_global_location() {
        assert_eq!(endpoint_uri("global"), "https://speech.googleapis.com");
    }

    #[test]
    fn endpoint_uri_uses_regional_host_for_other_locations() {
        assert_eq!(
            endpoint_uri("asia-northeast1"),
            "https://asia-northeast1-speech.googleapis.com"
        );
    }

    #[test]
    fn language_code_defaults_and_maps_ja_to_ja_jp() {
        let default_config = SttSessionConfig::new(16_000);
        assert_eq!(language_code_for(&default_config), "ja-JP");

        let ja_config = SttSessionConfig::new(16_000).with_language("ja");
        assert_eq!(language_code_for(&ja_config), "ja-JP");

        let en_config = SttSessionConfig::new(16_000).with_language("en-US");
        assert_eq!(language_code_for(&en_config), "en-US");
    }

    #[test]
    fn build_streaming_config_sets_explicit_pcm_decoding() {
        let config = SttSessionConfig::new(16_000).with_interim_results(true);
        let streaming_config = build_streaming_config(DEFAULT_MODEL, &config);

        let recognition_config = streaming_config.config.expect("config present");
        assert_eq!(recognition_config.model, DEFAULT_MODEL);
        assert_eq!(recognition_config.language_codes, vec!["ja-JP"]);
        assert!(matches!(
            recognition_config.decoding_config,
            Some(pb::recognition_config::DecodingConfig::ExplicitDecodingConfig(
                pb::ExplicitDecodingConfig {
                    encoding,
                    sample_rate_hertz: 16_000,
                    audio_channel_count: 1,
                }
            )) if encoding == pb::explicit_decoding_config::AudioEncoding::Linear16 as i32
        ));

        let features = recognition_config.features.expect("features present");
        assert!(features.diarization_config.is_none());

        let streaming_features = streaming_config
            .streaming_features
            .expect("streaming_features present");
        assert!(streaming_features.interim_results);
        assert!(!streaming_features.enable_voice_activity_events);
    }

    #[test]
    fn build_streaming_config_enables_diarization_with_empty_message() {
        let config = SttSessionConfig::new(16_000).with_diarization(true);
        let streaming_config = build_streaming_config(DEFAULT_MODEL, &config);
        let features = streaming_config
            .config
            .and_then(|c| c.features)
            .expect("features present");
        assert!(features.diarization_config.is_some());
    }

    #[test]
    fn build_streaming_config_maps_vocabulary_boost_to_inline_phrase_set() {
        let config = SttSessionConfig::new(16_000).with_extra(
            stt_api::SttExtraRequest::default()
                .with_vocabulary_boost(vec!["1on1".to_string(), "Kubernetes".to_string()]),
        );
        let streaming_config = build_streaming_config(DEFAULT_MODEL, &config);
        let adaptation = streaming_config
            .config
            .and_then(|c| c.adaptation)
            .expect("adaptation present");
        assert_eq!(adaptation.phrase_sets.len(), 1);
        let phrase_set = match &adaptation.phrase_sets[0].value {
            Some(pb::speech_adaptation::adaptation_phrase_set::Value::InlinePhraseSet(set)) => set,
            other => panic!("expected inline phrase set, got {other:?}"),
        };
        assert_eq!(phrase_set.phrases.len(), 2);
        assert_eq!(phrase_set.phrases[0].value, "1on1");
    }

    #[test]
    fn translate_result_drops_empty_transcript() {
        let result = pb::StreamingRecognitionResult {
            alternatives: vec![pb::SpeechRecognitionAlternative {
                transcript: String::new(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(translate_result(result).is_none());
    }

    #[test]
    fn translate_result_maps_partial_result() {
        let result = pb::StreamingRecognitionResult {
            alternatives: vec![pb::SpeechRecognitionAlternative {
                transcript: "こんにちは".to_string(),
                ..Default::default()
            }],
            is_final: false,
            result_end_offset: Some(prost_types::Duration {
                seconds: 1,
                nanos: 500_000_000,
            }),
            ..Default::default()
        };
        let event = translate_result(result).expect("event");
        match event {
            SttEvent::PartialTranscript {
                text, audio_end_ms, ..
            } => {
                assert_eq!(text, "こんにちは");
                assert_eq!(audio_end_ms, Some(1_500));
            }
            other => panic!("expected PartialTranscript, got {other:?}"),
        }
    }

    #[test]
    fn translate_result_maps_final_result_with_words_and_speaker() {
        let result = pb::StreamingRecognitionResult {
            alternatives: vec![pb::SpeechRecognitionAlternative {
                transcript: "こんにちは".to_string(),
                words: vec![pb::WordInfo {
                    word: "こんにちは".to_string(),
                    start_offset: Some(prost_types::Duration {
                        seconds: 1,
                        nanos: 0,
                    }),
                    end_offset: Some(prost_types::Duration {
                        seconds: 1,
                        nanos: 500_000_000,
                    }),
                    confidence: 0.97,
                    speaker_label: "2".to_string(),
                }],
                ..Default::default()
            }],
            is_final: true,
            ..Default::default()
        };
        let event = translate_result(result).expect("event");
        match event {
            SttEvent::FinalTranscript {
                text,
                words,
                audio_start_ms,
                ..
            } => {
                assert_eq!(text, "こんにちは");
                assert_eq!(audio_start_ms, Some(1_000));
                let words = words.expect("words present");
                assert_eq!(words.len(), 1);
                // Google's speaker_label "2" is 1-based (second speaker); stored
                // 0-based (see `parse_speaker_label`'s doc comment) as Some(1).
                assert_eq!(words[0].speaker, Some(1));
                assert_eq!(words[0].confidence, Some(0.97));
            }
            other => panic!("expected FinalTranscript, got {other:?}"),
        }
    }

    #[test]
    fn translate_result_leaves_speaker_none_when_label_unsupported() {
        let result = pb::StreamingRecognitionResult {
            alternatives: vec![pb::SpeechRecognitionAlternative {
                transcript: "hello".to_string(),
                words: vec![pb::WordInfo {
                    word: "hello".to_string(),
                    speaker_label: String::new(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            is_final: true,
            ..Default::default()
        };
        let event = translate_result(result).expect("event");
        match event {
            SttEvent::FinalTranscript { words, .. } => {
                assert_eq!(words.expect("words present")[0].speaker, None);
            }
            other => panic!("expected FinalTranscript, got {other:?}"),
        }
    }

    #[test]
    fn translate_response_maps_voice_activity_events_only_when_enabled() {
        let response = pb::StreamingRecognizeResponse {
            speech_event_type:
                pb::streaming_recognize_response::SpeechEventType::SpeechActivityBegin as i32,
            ..Default::default()
        };
        assert!(matches!(
            translate_response(response.clone(), true).as_slice(),
            [SttEvent::SpeechStarted]
        ));
        assert!(translate_response(response, false).is_empty());
    }

    #[test]
    fn parse_speaker_label_handles_empty_and_numeric() {
        assert_eq!(parse_speaker_label(""), None);
        // Google's speaker_label is 1-based; parse_speaker_label converts it to the
        // 0-based convention `Word::speaker` uses everywhere else (Deepgram's own
        // index is already 0-based) so the shared `transcript::speaker_label`
        // renderer's `+1` produces the right display number for both providers.
        assert_eq!(parse_speaker_label("1"), Some(0));
        assert_eq!(parse_speaker_label("3"), Some(2));
        assert_eq!(parse_speaker_label("not-a-number"), None);
        // Not expected per Google's own docs (labels start at "1"), but guard
        // against underflow rather than panicking if it ever occurs.
        assert_eq!(parse_speaker_label("0"), None);
    }

    #[test]
    fn keep_alive_silence_samples_computes_100ms_at_sample_rate() {
        assert_eq!(keep_alive_silence_samples(16_000), 1_600);
        assert_eq!(keep_alive_silence_samples(8_000), 800);
        assert_eq!(keep_alive_silence_samples(48_000), 4_800);
    }

    #[tokio::test]
    async fn keep_alive_sends_silent_pcm_audio_command_and_reports_samples() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
        let mut session = GoogleSession {
            commands: cmd_tx,
            drained: None,
            sample_rate_hz: 16_000,
        };

        let effect = session.keep_alive().await.expect("keep_alive succeeds");
        assert_eq!(effect, KeepAliveEffect::InjectedAudio { samples: 1_600 });

        match cmd_rx.try_recv().expect("a command was sent") {
            Command::Audio(bytes) => {
                // PCM16LE, mono: 2 bytes per sample, all zero (silence).
                assert_eq!(bytes.len(), 1_600 * 2);
                assert!(bytes.iter().all(|&b| b == 0));
            }
            Command::Close => panic!("expected Command::Audio, got Command::Close"),
        }
    }

    #[tokio::test]
    async fn send_audio_returns_retryable_transport_error_when_queue_is_full() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(1);
        let mut session = GoogleSession {
            commands: cmd_tx,
            drained: None,
            sample_rate_hz: 16_000,
        };

        // Fills the one available slot without anything draining it, mirroring a
        // stalled `build_outbound_stream`/TCP write.
        session
            .send_audio(AudioChunk {
                pcm: &[0.0],
                start_sample: 0,
            })
            .await
            .expect("first send_audio fills the only slot");

        let err = session
            .send_audio(AudioChunk {
                pcm: &[0.0],
                start_sample: 1,
            })
            .await
            .expect_err("second send_audio must fail once the queue is full");
        assert!(matches!(err, SttError::Transport(_)));
        assert!(
            err.is_retryable(),
            "a full outbound queue must be retryable, not permanent"
        );

        drop(cmd_rx);
    }

    #[tokio::test]
    async fn send_audio_returns_session_closed_when_receiver_dropped() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
        drop(cmd_rx);
        let mut session = GoogleSession {
            commands: cmd_tx,
            drained: None,
            sample_rate_hz: 16_000,
        };

        let err = session
            .send_audio(AudioChunk {
                pcm: &[0.0],
                start_sample: 0,
            })
            .await
            .expect_err("send_audio must fail once the receiver is gone");
        assert!(matches!(err, SttError::SessionClosed));
    }

    #[test]
    fn map_status_error_classifies_common_codes() {
        assert!(matches!(
            map_status_error(tonic::Status::unauthenticated("bad token")),
            SttError::AuthenticationFailed(_)
        ));
        assert!(matches!(
            map_status_error(tonic::Status::resource_exhausted("quota")),
            SttError::RateLimited
        ));
        assert!(matches!(
            map_status_error(tonic::Status::deadline_exceeded("slow")),
            SttError::Timeout
        ));
    }
}
