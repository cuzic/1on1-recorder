# session-store

SQLite-backed ledger of sessions, tracks, segments, upload status, and events for the
1on1-recorder application.

Like `recorder-domain`, this crate is **application-internal, not a publishing
candidate** — it exists to give `segment-store` (SPIKE-04's atomic Opus commit) and
`upload-client` (SPIKE-08's idempotent upload) one shared schema to register into,
instead of each keeping its own SQLite file (`SegmentDb` and `SpoolDb` respectively, in
the original spikes) that could drift apart. See the Codex review recorded on task #4
for the reasoning behind building this before `segment-store`/`upload-client`.

## Schema

- `sessions` — one row per recording session: the flattened `SessionManifest` plus a
  `capture_state_tag`/`capture_state_recoverable`/`capture_state_reason` mirroring
  `recorder_domain::CaptureState`, and a `remote_session_id` filled in once
  `UploadAdapter::create_session` succeeds.
- `tracks` — which `TrackKind`s a session declared (`self`/`remote`).
- `segments` — one row per committed `AudioSegment` (already fsynced/hashed on disk by
  `segment-store`; this table stores metadata and a path, not the audio bytes).
- `upload_status` — one row per segment's `UploadState`, plus `attempt_count` /
  `last_attempt_at` for `upload-client`'s backoff logic. `attempt_count` increments
  only on a transition *into* `Uploading` — its own later `Completed`/`Failed`
  outcome doesn't add a further increment, so `update_upload_state`'s caller can
  always go `Uploading -> {Completed, Failed}` for one real upload call without
  double-counting it (see `upload_attempt_count` and that method's doc comment).
- `events` — an append-only log of state transitions, for diagnostics.

`remote_session_id` (once set) and `segments_for_track` (every committed segment
for one track, regardless of upload status) are what `app-service`'s startup
crash-recovery (task #11) uses to decide whether a recovered session can resume
uploading without first needing to retry `UploadAdapter::create_session`.

`CaptureState`/`UploadState` are stored as a plain `_tag` column (plus the `Failed`
variant's `recoverable`/`retryable` + `reason` in their own columns) rather than a
serialized blob, so `WHERE state_tag NOT IN (...)`-style queries work without SQLite's
JSON1 extension.

## Concurrency

Opened with `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, and a 5s
`busy_timeout` — multiple connections (capture pipeline, upload worker) can hold the
same file open without hitting `SQLITE_BUSY` under normal load.

## Crash recovery

`reconcile_on_startup()` finds sessions a previous process instance left in a
non-terminal `CaptureState` (`preparing`/`recording`/`stopping`/`finalizing` — i.e. it
never reached `Finalized` or `Failed`) and marks them `Failed { recoverable: true }` so
`app-service` can drive them through finalization and upload instead of treating them
as still-active. See `tests/reconciliation.rs`.
