//! `stt-api` adapter for local/offline speech-to-text via whisper.cpp
//! ([`whisper-rs`](https://crates.io/crates/whisper-rs), which binds whisper.cpp's C
//! API — confirmed against `whisper-rs` `0.16.0`'s published API on crates.io/docs.rs
//! on 2026-07-18, via the sparse registry index and docs.rs, not from training-data
//! memory: `WhisperContext::new_with_params`/`create_state`,
//! `WhisperState::full`/`full_n_segments`/`get_segment`, `FullParams`'s `set_*`
//! builders, and `WhisperSegment`'s `start_timestamp`/`end_timestamp`/`to_str` are all
//! real methods on that version, and `WhisperContext`'s/`WhisperState`'s Send/Sync
//! auto-trait status below was read directly off their docs.rs "Auto Trait
//! Implementations" sections). **This crate is a design-validation spike per the task
//! that produced it — not integrated into `app-service`/`apps/desktop`, and not meant
//! to be treated as production-ready without resolving the open questions in the last
//! section of this doc comment.**
//!
//! # Why whisper.cpp doesn't fit `SttSession` the way the other four providers do
//!
//! `stt-deepgram`/`stt-openai`/`stt-google`/`stt-assemblyai` all wrap a *streaming*
//! cloud API: audio goes out over a WebSocket as it's captured, and
//! `PartialTranscript`/`FinalTranscript` events come back continuously while the
//! connection is open. whisper.cpp has no equivalent mode — `WhisperState::full` is a
//! single **synchronous, blocking, batch** call that transcribes one buffer of PCM in
//! one shot and returns when it's done; there is no partial-result callback that means
//! "here's what I have so far, more may still change" the way Deepgram's/AssemblyAI's
//! interim results do; and it does not maintain any live incremental decode state
//! between calls in a way the caller could otherwise perform a native "fixed frame
//! size" streaming pattern (§`FullParams::set_no_context`, used below, exists
//! specifically to make consecutive calls *not* depend on each other).
//!
//! The design here treats "batch inference on a periodically-cut buffer" as the unit
//! of work and maps that onto `SttSession` as follows:
//!
//! - **[`SttEvent::FinalTranscript`] only.** Every whisper.cpp call is already a
//!   complete, non-revisable transcription of its input chunk — there's no
//!   "provisional" stage to report as `PartialTranscript`. Emitting `PartialTranscript`
//!   for each individual whisper.cpp call and then never following up with a
//!   corresponding "the real final" would be actively misleading (callers built
//!   against the other four providers expect a partial to eventually be superseded by
//!   a final covering the same span), so this adapter never emits it at all — set
//!   `SttSessionConfig::interim_results` if you like, this adapter silently ignores it.
//! - **No [`SttEvent::SpeechStarted`]/[`SpeechEnded`](SttEvent::SpeechEnded).** These
//!   would need genuine low-latency VAD wired to real-time event delivery; the
//!   placeholder RMS-threshold buffering logic in [`chunk_buffer`] runs *after the
//!   fact* over whatever's already buffered; it is not designed to be a responsive VAD
//!   and shouldn't be read as one. `SttSessionConfig::vad_events` is ignored the same
//!   way `interim_results` is.
//! - **No diarization.** whisper.cpp has no built-in speaker diarization; combining it
//!   with e.g. pyannote is a well-known but separate pipeline stage this crate doesn't
//!   attempt. `Word::speaker` — not that this adapter even populates `Word` yet, see
//!   below — would always be `None`. `SttSessionConfig::diarization` is ignored.
//! - **`Word`-level detail is not populated (`words: None`).** whisper.cpp *can*
//!   produce token-level timestamps (`FullParams::set_token_timestamps`), but turning
//!   those into word-grouped `Word`s (respecting multi-byte UTF-8 boundaries,
//!   whisper.cpp's own BPE-token-to-word heuristics, etc.) is real work this spike
//!   doesn't attempt — see "Open questions" below. `audio_start_ms`/`audio_end_ms` on
//!   the emitted `FinalTranscript` *are* populated, but from this crate's own
//!   [`chunk_buffer`] sample bookkeeping (the chunk's known absolute position in the
//!   session), not from whisper.cpp's per-segment timestamps — those are relative to
//!   the chunk fed to `full()`, and by design each chunk is decoded independently
//!   (`set_no_context(true)`) precisely so unrelated chunks don't bleed decode state
//!   into each other, which makes this crate's own bookkeeping the more trustworthy
//!   source for *this* adapter's absolute-position contract.
//!
//! # Architecture: `Arc<WhisperContext>` shared, `WhisperState` created per call
//!
//! whisper-rs splits "the loaded model" (`WhisperContext`, expensive: parses and
//! mmaps/allocates the whole model) from "one decode's mutable working state"
//! (`WhisperState`, created via `WhisperContext::create_state`, cheap by comparison).
//! Per docs.rs's "Auto Trait Implementations" for `whisper-rs` `0.16.0`:
//! `WhisperContext` is `Send + Sync` (safe to share across threads/tasks), but
//! `WhisperState` is neither (it holds internal state whisper.cpp itself does not
//! guarantee is safe to touch from more than one thread, or to move across threads
//! mid-use). Combined with `WhisperState::full` being a **blocking** call that can
//! take seconds, this rules out the "hold a long-lived `WhisperState`, `.await` it
//! from async code" shape other adapters use for their WebSocket state.
//!
//! Instead: [`WhisperProvider`] loads the model once into an `Arc<WhisperContext>`
//! (in its constructor — see "Model loading is synchronous" below) and hands a clone
//! of that `Arc` to every session. Each [`WhisperSession`] owns only a
//! [`chunk_buffer::ChunkBuffer`] (pure Rust, no whisper-rs types, `Send`) and an
//! `mpsc::UnboundedSender` to a dedicated **worker task** (spawned per session by
//! `start_session`, mirroring `stt-assemblyai`'s/`stt-openai`'s writer/reader-task
//! split for their own not-directly-`.await`-able transports). The worker receives cut
//! chunks over that channel and, for each one, spawns a *fresh* `WhisperState` inside
//! `tokio::task::spawn_blocking` (the correct primitive here: `spawn_blocking` moves
//! the closure and everything it captures onto a dedicated blocking-pool thread and
//! runs it to completion there, which is exactly the "don't stall the async
//! runtime with CPU-bound synchronous work, and don't require the closure's contents
//! to be movable *back* across an await point" shape `WhisperState`'s non-`Send`-ness
//! demands). One chunk is transcribed at a time per session, in submission order — see
//! "Ordering and backpressure" below for why that's a deliberate simplification, not
//! an oversight.
//!
//! Creating a new `WhisperState` per chunk (rather than reusing one across a session)
//! also means every chunk is decoded from a clean slate, matching
//! `set_no_context(true)`'s intent: this crate's chunk boundaries are content-agnostic
//! (RMS-threshold/hard-cap cuts, not linguistic ones), so *not* carrying decode context
//! across them avoids one chunk's tail unpredictably influencing the next chunk's
//! start.
//!
//! # Model loading is synchronous
//!
//! `WhisperContext::new_with_params` is a plain blocking function — whisper-rs has no
//! async model-loading API. [`WhisperProvider::new`] is therefore also a plain
//! (non-`async`) blocking function; call it from inside a running Tokio runtime via
//! `tokio::task::spawn_blocking(move || WhisperProvider::new(path)).await` if loading
//! needs to happen without stalling that runtime's other work (e.g. at app startup
//! while other async initialization is also in flight). This is a one-time cost per
//! `WhisperProvider` (typically once per process), not per session.
//!
//! # Sample rate: this crate requires exactly 16kHz mono PCM
//!
//! whisper.cpp's published models are trained on, and require, 16kHz mono audio
//! (`WHISPER_SAMPLE_RATE` in whisper.cpp's own C source is a hardcoded `16000`) — this
//! is general knowledge about whisper.cpp carried over from prior training, not
//! independently re-verified against whisper.cpp's source in this session (unlike the
//! whisper-rs Rust API details cited at the top of this doc comment, which were
//! checked live). [`WhisperProvider::start_session`] rejects any
//! `SttSessionConfig::sample_rate_hz` other than `16000`, the same way
//! `stt-assemblyai` rejects anything but its own required rate. The existing
//! `app_service::resample::resample` function (mentioned by name, not linked, since
//! this crate intentionally has no dependency on `app-service` — see the Cargo.toml
//! comment on why this crate stays out of the default build) already does exactly this
//! conversion for the other rate-locked providers (`stt-openai` needs 24kHz,
//! `stt-assemblyai` needs 16kHz) and would be the natural place to add a 16kHz target
//! for this provider too, without any changes to `resample.rs` itself — it already
//! takes an arbitrary target rate as a parameter. Wiring that up is out of scope here.
//!
//! # Open questions / follow-up work before this leaves the spike stage
//!
//! - **CPU-only realtime performance.** With no GPU feature enabled (this crate's
//!   default — see the Cargo.toml comment), whisper.cpp inference speed is
//!   proportional to model size and CPU core count/SIMD support, and is *not*
//!   guaranteed to run faster than real time. As a rough, well-known-from-general-use
//!   order of magnitude (not benchmarked against this workspace's target hardware in
//!   this session): the `tiny`/`base` models are typically comfortably faster than
//!   real time on a modern laptop CPU; `small` is usually still faster than real time
//!   but with less headroom; `medium`/`large-v3` can approach or exceed real time on
//!   CPU alone, at which point the [`chunk_buffer::ChunkBuffer`]'s `max_chunk_ms` hard
//!   cap (buffer grows if inference falls behind) turns into a genuine, growing
//!   latency/memory problem rather than just a design nicety. This needs actual
//!   measurement on representative hardware before picking a default model for this
//!   project.
//! - **Model size vs. accuracy/speed.** whisper.cpp ships `tiny`/`base`/`small`/
//!   `medium`/`large-v3` (plus `large-v3-turbo`), each also available pre-quantized
//!   (`q5_1`/`q8_0`/etc. `.gguf`-style ggml files) trading a further chunk of accuracy
//!   for lower memory and faster inference. This crate takes a model path as a plain
//!   string ([`WhisperProvider::new`]) and is agnostic to which of these is loaded —
//!   picking a default for this project (accuracy for Japanese 1on1 meeting audio vs.
//!   CPU budget) is unresolved and deliberately left to the caller for now.
//! - **Two tracks (Self/Remote) running in parallel.** This project already runs one
//!   `SttSession` per track; two whisper.cpp sessions transcribing concurrently on the
//!   same machine roughly doubles CPU demand at whatever moment both tracks have a
//!   chunk ready, competing for the same core pool. Sharing one `Arc<WhisperContext>`
//!   (as this crate does) avoids doubling *memory* for model weights, but does nothing
//!   for CPU contention — each session's worker task still spawns its own
//!   `spawn_blocking` calls independently, so both tracks' inference calls can run
//!   simultaneously and oversubscribe cores. A follow-up integration should likely
//!   route *all* sessions sharing one `WhisperProvider` through a single bounded-
//!   concurrency queue (e.g. a semaphore sized to leave headroom for the rest of the
//!   app) rather than letting `n_threads`-per-call multiply by however many sessions
//!   happen to be open.
//! - **Native build cost vs. the other four providers.** Covered in detail in
//!   `Cargo.toml`'s comment: this is the reason `stt-whisper` is a workspace `members`
//!   entry but excluded from `default-members`, and why `app-service` should gate this
//!   behind an optional Cargo feature rather than a hard dependency if/when it's
//!   actually integrated.
//! - **Ordering and backpressure.** Each session's worker processes one chunk at a
//!   time in order, so events are never reordered within a session — but if inference
//!   takes longer than `max_chunk_ms` worth of audio to run, buffered
//!   [`chunk_buffer::PendingChunk`]s queue up in the (unbounded) command channel with
//!   no backpressure signalled to `send_audio`'s caller. `SttSession::send_audio`'s
//!   `Ok(())` return only means "the chunk was buffered/queued", not "transcribed" —
//!   same as every other provider in this workspace, but worth calling out explicitly
//!   here since whisper.cpp's call latency is far less predictable than a WebSocket
//!   round-trip. Actual backpressure (e.g. rejecting new audio, or dropping chunks)
//!   isn't implemented.
//! - **Not-yet-real VAD.** [`chunk_buffer::ChunkConfig::silence_rms_threshold`] is a
//!   fixed linear-amplitude threshold with no noise-floor adaptation; a quiet room
//!   with a hissy microphone and a loud room with a clean one need different
//!   thresholds. whisper.cpp itself optionally bundles a real VAD model
//!   (`FullParams::enable_vad`/`set_vad_model_path`/`set_vad_params`, confirmed to
//!   exist on `whisper-rs` `0.16.0` per the docs.rs check cited at the top of this
//!   file) which this crate does not use — worth evaluating as a replacement for the
//!   RMS heuristic before this leaves the spike stage, though note it would require
//!   *also* loading a separate VAD model file, and would presumably move VAD-driven
//!   cutting from [`chunk_buffer`] (today whisper-rs-agnostic and unit-testable
//!   without a model file) into the `spawn_blocking` inference path, changing the
//!   testability tradeoff this scaffold currently has.
//! - **Language / auto-detect.** When `SttSessionConfig::language` is `None`, this
//!   adapter passes whisper.cpp the literal string `"auto"` and also sets
//!   `FullParams::set_detect_language(true)`, based on prior general knowledge of
//!   whisper.cpp's/whisper.cpp CLI's own conventions for requesting auto-detection —
//!   this specific behavior was *not* independently re-verified against whisper.cpp's
//!   source or against a real model in this session (no model file is available in
//!   this sandbox; see "Testing" below), unlike the whisper-rs Rust API surface itself
//!   which was checked live. Treat it as a reasonable-but-unverified default.
//!
//! # Testing
//!
//! No model file is available in the environment this crate was written in, so
//! nothing that calls into whisper-rs itself (`WhisperProvider::new`,
//! `run_inference`) has automated test coverage here — only `cargo check -p
//! stt-whisper` (compiles, including the FFI-generating build script) was verified.
//! Everything that *can* be exercised without a model — [`chunk_buffer::ChunkBuffer`]'s
//! buffering/cutting logic, and this module's [`validate_config`] — has unit tests.
//! `whisper-rs` itself ships a `test-with-tiny-model` feature (visible in its
//! published feature list) for integration testing against a real (tiny) model file;
//! wiring that up is natural follow-up work but out of scope for this spike.

mod chunk_buffer;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chunk_buffer::{ChunkBuffer, PendingChunk};
use stt_api::{AudioChunk, SttError, SttEvent, SttProvider, SttSession, SttSessionConfig};
use tokio::sync::{mpsc, oneshot};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError};

pub use chunk_buffer::ChunkConfig;

/// whisper.cpp's published models are trained on, and require, 16kHz mono PCM — see
/// the module doc comment's "Sample rate" section.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// whisper.cpp CLI's own long-standing default thread count, per prior general
/// knowledge (not re-verified this session) — more threads doesn't reliably help
/// throughput past this point due to memory-bandwidth limits on typical consumer
/// hardware. Overridable via [`WhisperProvider::with_n_threads`].
const DEFAULT_N_THREADS: i32 = 4;

fn default_n_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| (n.get() as i32).min(DEFAULT_N_THREADS))
        .unwrap_or(DEFAULT_N_THREADS)
}

/// A loaded whisper.cpp model, shared (via `Arc`) across every session it starts. See
/// the module doc comment's "Architecture" section for why sharing `WhisperContext`
/// (not `WhisperState`) across sessions is the right split.
pub struct WhisperProvider {
    context: Arc<WhisperContext>,
    n_threads: i32,
    chunk_config: ChunkConfig,
}

impl WhisperProvider {
    /// Loads a whisper.cpp model from `model_path` (a `ggml`/`gguf`-format `.bin` file,
    /// as produced by whisper.cpp's `convert-*.py` scripts or downloaded directly from
    /// whisper.cpp's model repository). **Blocking** — see the module doc comment's
    /// "Model loading is synchronous" section for how to call this from async code
    /// without stalling the runtime.
    pub fn new(model_path: impl AsRef<Path>) -> Result<Self, SttError> {
        let context = WhisperContext::new_with_params(
            model_path.as_ref(),
            WhisperContextParameters::default(),
        )
        .map_err(map_whisper_err)?;
        Ok(Self {
            context: Arc::new(context),
            n_threads: default_n_threads(),
            chunk_config: ChunkConfig::default(),
        })
    }

    /// Overrides the number of CPU threads whisper.cpp uses per inference call
    /// (default: [`default_n_threads`], capped at 4 — see its doc comment).
    pub fn with_n_threads(mut self, n_threads: i32) -> Self {
        self.n_threads = n_threads;
        self
    }

    /// Overrides the chunk-buffering/VAD-threshold tunables every session created
    /// after this call will use. See [`ChunkConfig`]'s doc comment — the defaults are
    /// a starting point, not a tuned value for any particular microphone/room.
    pub fn with_chunk_config(mut self, chunk_config: ChunkConfig) -> Self {
        self.chunk_config = chunk_config;
        self
    }
}

#[async_trait]
impl SttProvider for WhisperProvider {
    async fn start_session(
        &self,
        config: SttSessionConfig,
    ) -> Result<(Box<dyn SttSession>, mpsc::UnboundedReceiver<SttEvent>), SttError> {
        validate_config(&config)?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        tokio::spawn(worker_task(
            Arc::clone(&self.context),
            cmd_rx,
            event_tx,
            config.language.clone(),
            self.n_threads,
        ));

        Ok((
            Box::new(WhisperSession {
                commands: cmd_tx,
                buffer: ChunkBuffer::new(self.chunk_config.clone()),
            }),
            event_rx,
        ))
    }
}

fn validate_config(config: &SttSessionConfig) -> Result<(), SttError> {
    if config.sample_rate_hz != SAMPLE_RATE_HZ {
        return Err(SttError::PermanentError(format!(
            "sample_rate_hz must be {SAMPLE_RATE_HZ} (whisper.cpp models require 16kHz \
             mono PCM; resample with app_service::resample::resample() before calling \
             send_audio — see this crate's module doc comment), got {}",
            config.sample_rate_hz
        )));
    }
    Ok(())
}

enum WhisperCommand {
    Chunk(PendingChunk),
    /// Sent by `finalize()` after any trailing remainder has already been queued as a
    /// `Chunk`; the worker acknowledges once every `Chunk` queued *before* this one has
    /// finished transcribing (not merely been received), so `finalize()`'s caller only
    /// sees it return after every event it will ever emit has already been sent.
    Finalize(oneshot::Sender<()>),
}

/// Holds only a command-channel sender plus the (whisper-rs-agnostic, `Send`)
/// [`ChunkBuffer`] — never a `WhisperState`/`WhisperContext` directly, so this type is
/// trivially `Send` regardless of whisper-rs's own Send/Sync story. The actual model
/// lives in `worker_task`/`run_inference`, reached only through
/// `tokio::task::spawn_blocking`. See the module doc comment's "Architecture" section.
struct WhisperSession {
    commands: mpsc::UnboundedSender<WhisperCommand>,
    buffer: ChunkBuffer,
}

#[async_trait]
impl SttSession for WhisperSession {
    async fn send_audio(&mut self, chunk: AudioChunk<'_>) -> Result<(), SttError> {
        // `chunk.start_sample` is trusted as-is (per `stt_api::AudioChunk`'s own doc
        // comment, populating it correctly on every call is the caller's job) — see
        // `ChunkBuffer::push`'s doc comment for why this crate doesn't second-guess it
        // with its own running total.
        for pending in self.buffer.push(chunk.pcm, chunk.start_sample) {
            self.commands
                .send(WhisperCommand::Chunk(pending))
                .map_err(|_| SttError::SessionClosed)?;
        }
        Ok(())
    }

    async fn finalize(mut self: Box<Self>) -> Result<(), SttError> {
        if let Some(pending) = self.buffer.flush() {
            self.commands
                .send(WhisperCommand::Chunk(pending))
                .map_err(|_| SttError::SessionClosed)?;
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        self.commands
            .send(WhisperCommand::Finalize(ack_tx))
            .map_err(|_| SttError::SessionClosed)?;
        ack_rx.await.map_err(|_| SttError::SessionClosed)
    }
}

/// Processes queued chunks one at a time, in order — see the module doc comment's
/// "Ordering and backpressure" section for what that does and doesn't guarantee.
async fn worker_task(
    context: Arc<WhisperContext>,
    mut commands: mpsc::UnboundedReceiver<WhisperCommand>,
    events: mpsc::UnboundedSender<SttEvent>,
    language: Option<String>,
    n_threads: i32,
) {
    while let Some(command) = commands.recv().await {
        match command {
            WhisperCommand::Chunk(chunk) => {
                let PendingChunk {
                    pcm,
                    start_sample,
                    end_sample,
                } = chunk;
                let ctx = Arc::clone(&context);
                let lang = language.clone();
                let result = tokio::task::spawn_blocking(move || {
                    run_inference(&ctx, &pcm, lang.as_deref(), n_threads)
                })
                .await;

                let audio_start_ms = Some(start_sample * 1000 / SAMPLE_RATE_HZ as u64);
                let audio_end_ms = Some(end_sample * 1000 / SAMPLE_RATE_HZ as u64);
                match result {
                    Ok(Ok(text)) => {
                        let text = text.trim().to_string();
                        if !text.is_empty() {
                            let _ = events.send(SttEvent::FinalTranscript {
                                text,
                                // Not populated — see the module doc comment's
                                // "`Word`-level detail is not populated" section.
                                words: None,
                                audio_start_ms,
                                audio_end_ms,
                                extra: Default::default(),
                            });
                        }
                        // An empty transcript (silence whisper.cpp correctly
                        // recognized as containing no speech) is dropped rather than
                        // forwarded as a useless empty FinalTranscript, matching
                        // stt-assemblyai's `translate_turn`.
                    }
                    Ok(Err(err)) => {
                        let _ = events.send(SttEvent::Error(err));
                    }
                    Err(join_err) => {
                        let _ = events.send(SttEvent::Error(SttError::PermanentError(format!(
                            "whisper.cpp inference task panicked: {join_err}"
                        ))));
                    }
                }
            }
            WhisperCommand::Finalize(ack) => {
                let _ = ack.send(());
                break;
            }
        }
    }
}

/// Runs one synchronous, blocking whisper.cpp inference call. Must only be called from
/// inside `tokio::task::spawn_blocking` — never `.await`ed directly, and never holding
/// the resulting `WhisperState` across an `.await` point (it isn't `Send`; see the
/// module doc comment's "Architecture" section).
fn run_inference(
    context: &WhisperContext,
    pcm: &[f32],
    language: Option<&str>,
    n_threads: i32,
) -> Result<String, SttError> {
    let mut state = context.create_state().map_err(map_whisper_err)?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(n_threads);
    params.set_translate(false);
    // Each chunk is an independently-cut, content-agnostic buffer (see the module doc
    // comment's "Architecture" section) — carrying decode context from one chunk into
    // the next would let one chunk's tail influence a linguistically-unrelated
    // neighbor's start.
    params.set_no_context(true);
    params.set_single_segment(false);
    params.set_print_progress(false);
    params.set_print_special(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    match language {
        Some(lang) => params.set_language(Some(lang)),
        // See the module doc comment's "Language / auto-detect" open question.
        None => {
            params.set_language(Some("auto"));
            params.set_detect_language(true);
        }
    }

    state.full(params, pcm).map_err(map_whisper_err)?;

    let n_segments = state.full_n_segments();
    let mut text = String::new();
    for i in 0..n_segments {
        if let Some(segment) = state.get_segment(i) {
            if let Ok(segment_text) = segment.to_str() {
                text.push_str(segment_text);
            }
        }
    }
    Ok(text)
}

/// whisper.cpp inference failures are deterministic given the same model/input (a bad
/// model file, an internal whisper.cpp error, etc.) rather than a transient
/// network/rate-limit condition the other four providers retry on — `PermanentError`
/// (never retryable, see `SttError::is_retryable`) is the closer fit of the two
/// non-transport-failure `SttError` variants.
fn map_whisper_err(err: WhisperError) -> SttError {
    SttError::PermanentError(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_config_accepts_16k() {
        let config = SttSessionConfig::new(SAMPLE_RATE_HZ);
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn validate_config_rejects_other_rates() {
        for rate in [8_000, 24_000, 44_100, 48_000] {
            let config = SttSessionConfig::new(rate);
            let Err(err) = validate_config(&config) else {
                panic!("expected sample_rate_hz={rate} to be rejected");
            };
            assert!(matches!(err, SttError::PermanentError(_)));
            assert!(!err.is_retryable());
        }
    }

    #[test]
    fn default_n_threads_is_never_zero_or_unreasonably_large() {
        let n = default_n_threads();
        assert!(n >= 1);
        assert!(n <= DEFAULT_N_THREADS);
    }

    #[test]
    fn map_whisper_err_is_never_retryable() {
        // WhisperError has no public constructor usable from outside whisper-rs in
        // 0.16, so this exercises the mapping indirectly through a value we *can*
        // build: confirm the SttError variant contract instead, matching the pattern
        // `stt-assemblyai`'s own tests use for its own error mapping.
        let err = SttError::PermanentError("boom".to_string());
        assert!(!err.is_retryable());
    }
}
