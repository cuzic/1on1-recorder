# stt-openai

[`stt-api`](../stt-api) adapter for OpenAI's Realtime transcription API.

See the module doc comment in [`src/lib.rs`](src/lib.rs) for the protocol writeup
this implementation follows (connection URL, `Authorization: Bearer` auth,
`session.update` framing, `input_audio_buffer.append`/`.commit`,
`conversation.item.input_audio_transcription.delta`/`.completed`/`.failed`), confirmed
2026-07-17 against
<https://developers.openai.com/api/docs/guides/realtime-transcription>,
<https://developers.openai.com/api/docs/guides/realtime-websocket>, and the
`realtime-server-events`/`realtime-client-events` API reference pages.

## Usage

```rust
use stt_api::{SttProvider, SttSessionConfig};
use stt_openai::OpenAiProvider;

let provider = OpenAiProvider::new(std::env::var("OPENAI_STT_API_KEY")?);
let config = SttSessionConfig::new(24_000) // must be exactly 24000
    .with_language("ja")
    .with_interim_results(true)
    .with_vad_events(true);
let (mut session, mut events) = provider.start_session(config).await?;
```

`language` defaults to `"ja"` when not set on the config, matching `stt-deepgram`'s
default (this project's PoC target is Japanese meetings).

## Constraints (different from `stt-deepgram`)

- **Sample rate is fixed at 24kHz mono PCM16.** `SttSessionConfig::sample_rate_hz` is
  validated against exactly `24000` in `start_session`, rejecting anything else with
  `SttError::PermanentError`. Unlike `stt-deepgram` (which forwards whatever rate the
  caller passes to Deepgram's `sample_rate` query param), this crate does **not**
  resample — resampling to 24kHz is the caller's responsibility before calling
  `send_audio`.
- **No diarization.** `SttSessionConfig::diarization` is accepted but ignored, and
  `Word::speaker` is always `None`. OpenAI's realtime transcription `completed` event
  returns a flat `transcript` string with no per-word data at all (there's a separate
  `gpt-4o-transcribe-diarize` model for speaker-labeled *batch* transcription, but
  that's a different API surface this crate doesn't implement).
- **`vad_events` also controls turn segmentation, not just the `SpeechStarted`/
  `SpeechEnded` events.** OpenAI's server-side VAD (`turn_detection`) is what
  auto-segments audio into successive turns, each producing its own
  `FinalTranscript`. With `vad_events: false`, `turn_detection` is left `null` and
  only one `FinalTranscript` is produced for the whole session, at `finalize()`'s
  manual commit. Set `vad_events: true` for continuous per-utterance finals like
  `stt-deepgram` gives by default.

## Model

Uses `gpt-realtime-whisper` by default (override via `OpenAiProvider::with_model`) —
the model OpenAI's docs (as of 2026-07-17) describe as the low-latency streaming
choice for realtime transcription. Deliberately not `whisper-1`,
`gpt-4o-transcribe`, or `gpt-4o-mini-transcribe`: task #41 called those out as
approaching a 2026-06 deprecation window at the time this crate was written.

## Status

PoC adapter. Not yet verified against a live connection — protocol details are from
OpenAI's official docs only. The first live spike against a real API key is tracked
as a follow-up.

An `examples/openai_poc.rs` spike is available for that follow-up: it streams a
synthesized sine-wave tone (no real meeting audio available in this dev environment)
through a live session and prints every `SttEvent` as it arrives. Run it with a real
key via `OPENAI_STT_API_KEY=xxx cargo run --example openai_poc -p stt-openai`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
