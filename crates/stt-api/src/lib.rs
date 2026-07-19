//! Provider-agnostic streaming speech-to-text abstraction.
//!
//! See `stt-transcription-architecture.md` (repository root) for the full design
//! rationale. In short: a session is opened with [`SttProvider::start_session`], audio
//! is pushed in via [`SttSession::send_audio`], and results stream back as
//! [`SttEvent`]s on the paired channel until [`SttSession::finalize`] drains the last
//! ones. Fields every provider can produce live directly on [`SttEvent`]; fields only
//! some providers support live in [`SttExtraRequest`]/[`SttExtraResult`], named once
//! here so two providers implementing the same capability share one name instead of
//! inventing provider-prefixed duplicates.
//!
//! This crate has no dependency on any one provider's SDK, and no dependency on this
//! project's own session/track types — "audio in, text out" is the entire contract.
//! Correlating a session with a recording session/track is the caller's job.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// A single STT provider. Returns `Box<dyn SttSession>` (not an associated type) so
/// callers can hold `Box<dyn SttProvider>` and swap providers at runtime.
#[async_trait]
pub trait SttProvider: Send + Sync {
    async fn start_session(
        &self,
        config: SttSessionConfig,
    ) -> Result<(Box<dyn SttSession>, mpsc::UnboundedReceiver<SttEvent>), SttError>;
}

/// One open streaming session with a provider. `async_trait`'s default `Send`-future
/// requirement applies to implementations: if a provider's transport (e.g. a WebSocket
/// write half) is `!Send`, isolate it in a dedicated task and talk to it over a
/// channel rather than holding it across an `.await` here.
#[async_trait]
pub trait SttSession: Send {
    /// Sends one chunk of mono PCM audio. Chunk size is the caller's choice; providers
    /// that require fixed framing buffer/split internally.
    async fn send_audio(&mut self, chunk: AudioChunk<'_>) -> Result<(), SttError>;

    /// Signals end of audio and flushes any results still in flight. Each adapter
    /// implements its own provider's shutdown handshake (e.g. Deepgram's
    /// `CloseStream` -> `Metadata` drain) — no provider has been found where simply
    /// dropping the connection is sufficient.
    async fn finalize(self: Box<Self>) -> Result<(), SttError>;

    /// Keeps the session alive during a stretch where the caller isn't sending real
    /// audio (e.g. it's skipping silence rather than paying to stream/transcribe it).
    /// Some streaming STT providers drop the connection on an idle timeout if no audio
    /// arrives for a while; `app-service` is expected to call this periodically during
    /// such gaps to prevent that.
    ///
    /// The default implementation is a no-op ([`KeepAliveEffect::Noop`]) — providers
    /// that need this (Deepgram, Google, OpenAI) override it individually. This method
    /// only adds the hook; provider-specific overrides are implemented separately.
    async fn keep_alive(&mut self) -> Result<KeepAliveEffect, SttError> {
        Ok(KeepAliveEffect::Noop)
    }
}

/// What, if anything, [`SttSession::keep_alive`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAliveEffect {
    /// The provider needs no keep-alive traffic; nothing was sent.
    Noop,
    /// A protocol-level control/keep-alive message was sent (e.g. a WebSocket ping or
    /// a provider-defined keep-alive frame). It carries no audio, so it does not
    /// advance the provider's audio timeline.
    ControlMessage,
    /// Artificial audio (e.g. silence) was sent to the provider to keep the stream
    /// open. Unlike `ControlMessage`, this *does* advance the provider's audio
    /// timeline by `samples` samples, which callers correlating timestamps back to
    /// the real recording need to account for.
    InjectedAudio { samples: u64 },
}

/// A chunk of audio plus its absolute position within the session, so result events
/// (`audio_start_ms`/`audio_end_ms`) can be correlated back to the caller's own
/// recording timeline.
pub struct AudioChunk<'a> {
    pub pcm: &'a [f32],
    /// Sample offset from the first `send_audio` call in this session, at
    /// `SttSessionConfig::sample_rate_hz`.
    pub start_sample: u64,
}

/// Per-session configuration. `#[non_exhaustive]` so new fields can be added without
/// breaking callers; construct via [`SttSessionConfig::new`] plus `with_*` builders
/// (a `#[non_exhaustive]` struct cannot be built with struct-literal syntax — including
/// `..Default::default()` — from outside this crate).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SttSessionConfig {
    pub sample_rate_hz: u32,
    /// `None` requests provider auto-detection where supported.
    pub language: Option<String>,
    /// Providers that don't support interim results always emit `FinalTranscript` only.
    pub interim_results: bool,
    /// Providers that don't support diarization always leave `Word::speaker` as `None`.
    pub diarization: bool,
    /// Providers that don't support VAD events never emit `SpeechStarted`/`SpeechEnded`.
    pub vad_events: bool,
    pub extra: SttExtraRequest,
}

impl SttSessionConfig {
    /// `sample_rate_hz` has no sensible default, so it's a required constructor
    /// argument rather than something left to `Default` (which would silently allow
    /// `0`). Each adapter's `start_session` must still validate `sample_rate_hz` is
    /// nonzero and a rate the provider actually supports, rejecting with
    /// [`SttError::PermanentError`] otherwise — this constructor only rules out
    /// forgetting to set it at all.
    pub fn new(sample_rate_hz: u32) -> Self {
        Self {
            sample_rate_hz,
            language: None,
            interim_results: false,
            diarization: false,
            vad_events: false,
            extra: SttExtraRequest::default(),
        }
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    pub fn with_interim_results(mut self, interim_results: bool) -> Self {
        self.interim_results = interim_results;
        self
    }

    pub fn with_diarization(mut self, diarization: bool) -> Self {
        self.diarization = diarization;
        self
    }

    pub fn with_vad_events(mut self, vad_events: bool) -> Self {
        self.vad_events = vad_events;
        self
    }

    pub fn with_extra(mut self, extra: SttExtraRequest) -> Self {
        self.extra = extra;
        self
    }
}

#[derive(Debug, Clone)]
pub enum SttEvent {
    SpeechStarted,
    SpeechEnded,
    PartialTranscript {
        text: String,
        /// Absolute position within the session, matching `AudioChunk::start_sample`.
        /// `None` if the provider doesn't report a range for this event.
        audio_start_ms: Option<u64>,
        audio_end_ms: Option<u64>,
        extra: SttExtraResult,
    },
    FinalTranscript {
        text: String,
        words: Option<Vec<Word>>,
        audio_start_ms: Option<u64>,
        audio_end_ms: Option<u64>,
        extra: SttExtraResult,
    },
    /// Wraps [`SttError`] directly rather than a separate `{message, recoverable}`
    /// shape, so retryability has exactly one source of truth: `SttError::is_retryable`.
    Error(SttError),
}

#[derive(Debug, Clone)]
pub struct Word {
    pub text: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub confidence: Option<f32>,
    pub speaker: Option<u32>,
}

/// Deliberately not a copy of `recorder_domain::UploadError`'s variant set — the two
/// error surfaces classify different failure modes. What's shared is the *pattern*:
/// a typed enum with an `is_retryable()` method, so callers don't have to re-derive
/// retry policy from a string or status code.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SttError {
    #[error("connection/transport error: {0}")]
    Transport(String),
    #[error("request timed out")]
    Timeout,
    #[error("rate limited")]
    RateLimited,
    #[error("authentication failed or expired: {0}")]
    AuthenticationFailed(String),
    #[error("provider rejected the request: {0}")]
    PermanentError(String),
    #[error("session already closed")]
    SessionClosed,
}

impl SttError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            SttError::Transport(_) | SttError::Timeout | SttError::RateLimited
        )
    }
}

/// A catalog of "known but not universally supported" request-side capabilities.
/// New capabilities get one field here, named after the *concept*, not any one
/// provider — when a second provider gains the same capability, it reuses the
/// existing field rather than inventing a new name. Every field is `Option`, so
/// leaving it unset (or targeting a provider that ignores it) is never an error.
///
/// `#[non_exhaustive]`, so external crates (including provider adapter crates in this
/// workspace, which are just as "external" to `stt-api` as anyone else) cannot use
/// struct-literal syntax to construct this — not even `..Default::default()`. Use
/// [`SttExtraRequest::default`] plus the `with_*` builders below instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct SttExtraRequest {
    /// Biases recognition toward specific words/phrases (proper nouns, jargon).
    /// Supported by: Deepgram (`keywords`), Google (`speech_contexts`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vocabulary_boost: Option<Vec<String>>,

    /// A "thinking" budget to spend before transcribing. Supported by: Gemini.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<u32>,

    /// Last-resort passthrough for anything not worth a dedicated field yet. Prefer
    /// adding a typed field above before reaching for this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_specific: Option<serde_json::Value>,
}

impl SttExtraRequest {
    pub fn with_vocabulary_boost(mut self, words: Vec<String>) -> Self {
        self.vocabulary_boost = Some(words);
        self
    }

    pub fn with_reasoning_budget(mut self, budget: u32) -> Self {
        self.reasoning_budget = Some(budget);
        self
    }

    pub fn with_provider_specific(mut self, value: serde_json::Value) -> Self {
        self.provider_specific = Some(value);
        self
    }
    // Add a matching `with_*` for every new field.
}

/// The result-side counterpart of [`SttExtraRequest`] — same naming and
/// `#[non_exhaustive]` + builder rules apply, including to adapter crates assembling
/// one from a provider's response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct SttExtraResult {
    /// Auto-detected language, when the provider supports detection and reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_language: Option<String>,

    /// Sentiment classification, when the provider supports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sentiment: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_specific: Option<serde_json::Value>,
}

impl SttExtraResult {
    pub fn with_detected_language(mut self, language: impl Into<String>) -> Self {
        self.detected_language = Some(language.into());
        self
    }

    pub fn with_sentiment(mut self, sentiment: impl Into<String>) -> Self {
        self.sentiment = Some(sentiment.into());
        self
    }

    pub fn with_provider_specific(mut self, value: serde_json::Value) -> Self {
        self.provider_specific = Some(value);
        self
    }
}
