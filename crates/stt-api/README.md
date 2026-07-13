# stt-api

Provider-agnostic streaming speech-to-text abstraction.

See [`stt-transcription-architecture.md`](../../stt-transcription-architecture.md) at
the repository root for the full design rationale (why a typed `extra` catalog instead
of a raw JSON bag, why `SttProvider`/`SttSession` return `Box<dyn ...>`, why the error
type isn't a copy of `recorder-domain::UploadError`).

## Shape

- [`SttProvider::start_session`] opens a session and returns an
  `mpsc::UnboundedReceiver<SttEvent>` alongside a `Box<dyn SttSession>` used to push
  audio in.
- [`SttSession::send_audio`] takes an [`AudioChunk`] (PCM plus its absolute sample
  offset in the session, so results can be correlated back to a recording timeline).
- [`SttSession::finalize`] drains and closes — each provider adapter implements its own
  shutdown handshake here; no provider has been found where dropping the connection is
  enough.
- Fields every provider can produce live directly on [`SttEvent`]. Fields only some
  providers support live in [`SttExtraRequest`]/[`SttExtraResult`] — named once here so
  a second provider implementing the same capability reuses the field instead of
  inventing a provider-prefixed duplicate. Both are `#[non_exhaustive]`; construct them
  via `Default::default()` plus the `with_*` builders (struct-literal construction,
  even with `..Default::default()`, isn't available to external crates for a
  `#[non_exhaustive]` struct — this includes provider adapter crates in this
  workspace).

## Status

Implemented alongside `stt-deepgram` (Deepgram Nova-3), the first (and currently only)
provider adapter. No dependency on any provider SDK or on this project's own
session/track types.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
