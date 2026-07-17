# stt-assemblyai

[`stt-api`](../stt-api) adapter for AssemblyAI's Streaming v3 speech-to-text API.

Protocol details (connection URL/query params, `Authorization` header format, binary
PCM16 frames, `Turn`/`Begin`/`Termination` messages, `Terminate` -> `Termination`
drain) were verified directly against AssemblyAI's docs on 2026-07-17 — see the
module-level doc comment in [`src/lib.rs`](src/lib.rs) for source links and the full
writeup, including why diarization and VAD "speech ended" events work differently
here than in `stt-deepgram`.

## Usage

```rust
use stt_api::{SttProvider, SttSessionConfig};
use stt_assemblyai::AssemblyAIProvider;

let provider = AssemblyAIProvider::new(std::env::var("ASSEMBLYAI_API_KEY")?);
let config = SttSessionConfig::new(16_000)
    .with_language("ja")
    .with_interim_results(true);
let (mut session, mut events) = provider.start_session(config).await?;
```

`language` defaults to `"ja"` when not set on the config, matching this project's PoC
target of Japanese meetings. AssemblyAI's streaming API added Japanese support in
2026.

## Known limitations

- **Sample rate**: only 16 kHz PCM16 is supported. `start_session` rejects any other
  `sample_rate_hz` with `SttError::PermanentError`. AssemblyAI also supports Opus
  encodings (where `sample_rate` is ignored entirely), but that doesn't fit
  `stt-api`'s "caller picks the rate" contract, so this crate doesn't wire it up.
- **Diarization**: AssemblyAI does not support speaker diarization on a single live
  audio stream — their documented approach is one streaming session per speaker,
  merged into one transcript downstream. This project already captures Self/Remote
  audio as separate tracks (and therefore separate `SttSession`s), which is that same
  shape, so `Word::speaker` is always `None` here regardless of
  `SttSessionConfig::diarization`; track identity is the speaker signal instead.
- **VAD events**: v3 has a `SpeechStarted` message but no matching "speech ended"
  message. This adapter emits `SttEvent::SpeechStarted` (when `vad_events` is set) but
  never `SttEvent::SpeechEnded` — silence is instead implied by the next `Turn`
  carrying `end_of_turn: true`.

## Status

PoC adapter. Not yet verified against a live connection — protocol details are from
AssemblyAI's official docs only. The first live spike against a real API key is
tracked as a follow-up.

An `examples/assemblyai_poc.rs` spike is available for that follow-up: it streams a
synthesized sine-wave tone (no real meeting audio available in this dev environment)
through a live session and prints every `SttEvent` as it arrives. Run it with a real
key via `ASSEMBLYAI_API_KEY=xxx cargo run --example assemblyai_poc -p stt-assemblyai`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
