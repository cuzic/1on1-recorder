# stt-whisper

[`stt-api`](../stt-api) adapter for local/offline speech-to-text via whisper.cpp, via
the [`whisper-rs`](https://crates.io/crates/whisper-rs) bindings.

**Status: design-validation spike.** This crate exists to answer the question "how
would a non-streaming, batch-inference engine map onto `stt-api`'s streaming
`SttSession` trait?" — it is not wired into `app-service`/`apps/desktop`, and the design
decisions and open questions below should be read before treating it as
production-ready. See the module doc comment in [`src/lib.rs`](src/lib.rs) for the full
writeup; this file is a shorter pointer to the same material.

## Before you build this crate

Unlike `stt-deepgram`/`stt-openai`/`stt-google`/`stt-assemblyai` (pure-Rust WebSocket
clients), this crate depends on `whisper-rs`, which compiles whisper.cpp (C/C++) from
source via `cmake` at build time. You need a working `cmake` + C/C++ toolchain
installed. Because of this cost, `stt-whisper` is a workspace `members` entry but is
excluded from the root `Cargo.toml`'s `default-members` — a plain `cargo build`/`cargo
check` at the repo root will *not* build it; use `cargo build -p stt-whisper` (or
`--workspace`) explicitly.

## Design summary

- One `WhisperProvider` loads one whisper.cpp model (`Arc<WhisperContext>`, shared —
  it's `Send + Sync` per whisper-rs's own docs) and can start many sessions.
- Each `WhisperSession` buffers incoming PCM in a whisper-rs-agnostic `ChunkBuffer`
  (energy-threshold "VAD" plus a hard time cap) and, whenever it cuts a chunk, hands it
  to a dedicated per-session worker task over a channel.
- The worker transcribes one chunk at a time via `tokio::task::spawn_blocking`, creating
  a fresh `WhisperState` per call (`WhisperState` is *not* `Send`/`Sync`, so it can never
  cross an `.await` or be shared — this is why a new one is created per blocking call
  instead of being held across the session's lifetime).
- Only `SttEvent::FinalTranscript` is ever emitted — no `PartialTranscript`,
  `SpeechStarted`/`SpeechEnded`, or diarization. `Word`-level detail isn't populated
  (`words: None`); `audio_start_ms`/`audio_end_ms` come from this crate's own sample
  bookkeeping, not whisper.cpp's per-segment timestamps.
- Requires exactly 16kHz mono PCM (`stt_whisper::SAMPLE_RATE_HZ`), matching whisper.cpp
  model requirements. `app_service::resample::resample()` (unchanged by this crate —
  see its own doc comment) already supports converting to an arbitrary target rate and
  would be the natural place to add a 16kHz target when this is actually integrated.

## Open questions (see `src/lib.rs` for the full detail on each)

- CPU-only inference speed relative to real time is unmeasured on this project's target
  hardware — model choice (`tiny` through `large-v3`, quantized or not) is unresolved.
- Two tracks (Self/Remote) running whisper.cpp concurrently roughly doubles CPU demand;
  sharing `WhisperContext` avoids doubling model-weight memory but does nothing for CPU
  contention across sessions.
- The RMS-threshold "VAD" is a placeholder with no noise-floor adaptation; whisper.cpp's
  own bundled VAD model (`FullParams::enable_vad`) is a candidate replacement, at the
  cost of moving cutting logic out of the currently model-free-testable `chunk_buffer`
  module.
- `send_audio`'s `Ok(())` only means "queued", not "transcribed" — there's no
  backpressure if inference falls behind the audio being captured.
- Auto-detect language behavior (`"auto"` + `set_detect_language(true)`) is based on
  prior general knowledge of whisper.cpp conventions, not verified against a live model
  in this environment (no model file was available).

## Testing

No model file was available in the environment this crate was written in.
`cargo check -p stt-whisper` was verified to compile (including whisper-rs-sys's native
build). `cargo test -p stt-whisper` covers the model-free logic: `chunk_buffer`'s
buffering/cutting behavior, and `lib.rs`'s config validation. Nothing that calls into
whisper-rs itself (`WhisperProvider::new`, `run_inference`) has automated coverage here;
`whisper-rs` ships a `test-with-tiny-model` feature for that, which would be natural
follow-up work.
