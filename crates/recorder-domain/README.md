# recorder-domain

Core domain types for the 1on1-recorder application, ported from `design.md` §9
(domain model), §10 (state model), and §13 (upload API boundary).

Unlike `audio-timeline`, `capture-api`, and `capture-windows`, this crate is
**application-internal, not a publishing candidate** — its types (`SessionManifest`,
`TrackKind`'s `"self"`/`"remote"` wire format, the `UploadAdapter` API contract) are
specific to this recorder's design, not a generic abstraction over a wider problem.

## Contents

- `TrackKind`, `CapturedFrame` — the two logical audio tracks and one raw captured
  frame (§9.1, §9.2).
- `AudioSegment`, `AudioCodec` — one committed, immutable 30-second segment on disk
  (§9.3).
- `SessionId`, `SessionManifest` and its nested `CaptureManifest` / `AudioManifest` /
  `ConsentManifest` — the manifest sent to `create_session` (§9.4). `RemoteSourceKind`
  distinguishes endpoint loopback (Phase 1A) from process loopback (Phase 1B) so a
  manifest always states which guarantee actually applied.
- `CaptureState`, `UploadState` — independent per-session/per-segment lifecycles
  (§10); a capture failure doesn't stop uploads of already-committed segments, and an
  upload failure doesn't stop recording.
- `UploadAdapter`, `RemoteSession`, `UploadReceipt`, `SessionSummary`, `UploadError` —
  the API boundary (§13). `UploadError` encodes §13.3's retry classification directly
  (`is_retryable`, `needs_token_refresh_before_retry`) instead of leaving callers to
  re-derive it from a raw status code.

Contains no OS-specific types — those live in `capture-windows` and future
`capture-linux`/`capture-macos` crates.
