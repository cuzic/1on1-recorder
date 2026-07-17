# stt-deepgram

[`stt-api`](../stt-api) adapter for Deepgram's Nova-3 streaming speech-to-text API.

See [`stt-transcription-architecture.md`](../../stt-transcription-architecture.md) §6
at the repository root for the protocol writeup this implementation follows
(connection URL/query params, `Authorization: Token` auth, binary PCM16 frames,
`is_final`/`speech_final`, `SpeechStarted`/`UtteranceEnd`, `CloseStream` ->
`Metadata` drain).

## Usage

```rust
use stt_api::{SttProvider, SttSessionConfig};
use stt_deepgram::DeepgramProvider;

let provider = DeepgramProvider::new(std::env::var("DEEPGRAM_API_KEY")?);
let config = SttSessionConfig::new(16_000)
    .with_language("ja")
    .with_interim_results(true);
let (mut session, mut events) = provider.start_session(config).await?;
```

`language` defaults to `"ja"` when not set on the config — this project's PoC target
is Japanese meetings, and Deepgram's Nova-3 supports Japanese at the same price as
English in both streaming and batch (no separate/pricier tier, unlike some other
providers).

## Status

PoC adapter. Not yet verified against a live connection — protocol details are from
Deepgram's official docs only (see the design doc's source links). The first live
spike against a real API key is tracked as a follow-up.

An `examples/deepgram_poc.rs` spike is available for that follow-up: it streams a
synthesized sine-wave tone (no real meeting audio available in this dev environment)
through a live session and prints every `SttEvent` as it arrives. Run it with a real
key via `DEEPGRAM_API_KEY=xxx cargo run --example deepgram_poc -p stt-deepgram`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
