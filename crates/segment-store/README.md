# segment-store

Opus/Ogg encoding and atomic segment commit for the meeting recorder, ported from
`spikes/spike-04-opus-atomic-commit`.

Application-internal, not a publishing candidate (like `recorder-domain` and
`session-store`): the commit protocol is generic, but the crate is wired directly to
`session-store`'s schema.

## Commit protocol (design.md §12.2)

1. Write `{sequence:06}.partial` under `session_dir/{track}/` and flush.
2. `fsync` the file.
3. SHA-256 the encoded bytes.
4. Atomically rename `.partial` -> `{sequence:06}.opus`.
5. Register the segment with `session-store` (`SessionStore::register_segment`).

`CrashPoint` lets tests cut this short after any step, leaving exactly what a real
crash at that point would leave on disk, for `scan_and_recover` to reconcile.

## Changes from the original spike

- Segments are keyed by `(session_id, track, sequence)`, not just
  `(session_id, sequence)` — `Self` and `Remote` now commit under separate
  subdirectories (`session_dir/self/`, `session_dir/remote/`) and never collide on the
  same sequence number.
- Registration goes through `session_store::SessionStore` instead of a standalone
  `SegmentDb`, so segments and their upload status live in the one schema
  `upload-client` will also register into.
- `duration_ms` is derived from the committed Ogg file's own granule position (see
  `granule::read_total_samples`) instead of being trusted from the caller — the
  encoder already tracks total encoded samples via the granule position on the last
  packet, so recovery can reconstruct exact durations without decoding audio.
- `timeline_start_ms` for a segment recovered after a crash (rename succeeded, DB
  registration didn't) is reconstructed as `sequence * nominal_segment_duration_ms`.
  This is exact for Phase 1A's fixed-cadence, gap-free segmenter, but would be wrong
  for a future variable-length segmenter — see `recovery::scan_and_recover`'s doc
  comment.

## Known constraint: `rename` onto an existing file

`std::fs::rename` on Windows fails if the destination already exists (POSIX silently
replaces it). `commit_segment` never calls it that way in the intended flow — each
`(track, sequence)` is committed exactly once, and after a crash the caller is expected
to run `scan_and_recover` (which registers an already-renamed `.opus` file rather than
re-encoding it) before ever calling `commit_segment` again for that sequence. This
crate does not currently guard against a caller violating that and re-committing an
already-`.opus`'d sequence — worth revisiting once `app-service`'s restart flow is
wired up (task #7), since a `rename` error on that path would currently surface as a
generic `SegmentStoreError::Io` rather than a clear "already committed" error.
