# stt-google

[`stt-api`](../stt-api) adapter for Google Cloud Speech-to-Text v2's
`StreamingRecognize` API (gRPC).

See this crate's `src/lib.rs` module doc comment for the protocol writeup this
implementation follows (resource-based `recognizer` path, `streaming_config` ->
audio-only message framing, model/diarization choice and its streaming caveat, ADC
auth). `proto/google/cloud/speech/v2/cloud_speech.proto` is a trimmed, wire-
compatible copy of Google's own `cloud_speech.proto` — see that file's header
comment for exactly what was kept vs dropped.

## Usage

```rust
use stt_api::{SttProvider, SttSessionConfig};
use stt_google::{GoogleProvider, GoogleSttCredentials};

// Application Default Credentials by default: `gcloud auth application-default
// login`, or GOOGLE_APPLICATION_CREDENTIALS pointing at a service-account key.
let credentials = GoogleSttCredentials::new("my-gcp-project", "global");
let provider = GoogleProvider::new(credentials);

let config = SttSessionConfig::new(16_000)
    .with_language("ja")
    .with_interim_results(true);
let (mut session, mut events) = provider.start_session(config).await?;
```

To use an explicit service-account key instead of ADC:

```rust
let credentials = GoogleSttCredentials::new("my-gcp-project", "global")
    .with_service_account_path("/path/to/key.json");
// or: .with_service_account_json(key_json_contents)
```

`GoogleSttCredentials` bundles project id, location, and service-account source into
one struct so `credential-store` can persist it as a single JSON blob under
`stt_google::CREDENTIAL_SERVICE` / `GOOGLE_STT_CREDENTIALS_ACCOUNT` — a plain API-key
string (Deepgram's shape) isn't enough here, since every request is also scoped to a
project and a location.

`language` defaults to `ja-JP` when not set (or set to `"ja"`) on the config — this
project's PoC target is Japanese meetings, and Chirp 3 (this crate's default model)
is GA for `ja-JP` streaming transcription.

## Diarization caveat

Chirp 3 speaker diarization is only available through `BatchRecognize`/`Recognize`,
not `StreamingRecognize` (see `src/lib.rs` for the checked source). This crate still
requests diarization when `SttSessionConfig::diarization` is set — harmless when
unsupported, since the server just won't populate word-level speaker labels — and
reports `Word::speaker` as `None` whenever that happens, rather than erroring.

## Requirements

Building this crate compiles `proto/google/cloud/speech/v2/cloud_speech.proto` via
`tonic-prost-build`, which needs a `protoc` binary on `PATH` (or pointed to via the
`PROTOC` env var) — e.g. `brew install protobuf` or `apt install protobuf-compiler`.

## Status

PoC adapter. Not yet verified against a live connection — protocol details are from
Google's own proto/docs only (see the module doc comment's source links), and there
are no live Google Cloud credentials available in this dev environment. The first
live spike against a real project is tracked as a follow-up.

An `examples/google_poc.rs` spike is available for that follow-up: it streams a
synthesized sine-wave tone (no real meeting audio available in this dev environment)
through a live session and prints every `SttEvent` as it arrives. Run it with
`GOOGLE_STT_PROJECT_ID=xxx cargo run --example google_poc -p stt-google` (plus
Application Default Credentials configured).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
