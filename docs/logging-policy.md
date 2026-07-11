# Logging / metrics / diagnostics policy

Formalizes design.md §17 (可観測性と診断) into rules a PR reviewer or a future
contributor can check code against, per Codex's review of the original task list
(task #12): the risk this exists to prevent is a `tracing::instrument` (or a
one-off `tracing::debug!(?args, ...)`) that logs an entire struct — including a
bearer token, a keyring secret, or a buffer of raw PCM samples — without anyone
having deliberately decided that was safe.

## The rule

**Never let these three things reach a log, metric label, or diagnostic export:**

1. Audio content (raw PCM samples, encoded segment bytes, anything derived from
   what was actually said in a recording).
2. Authentication material: bearer tokens, the credential-store's secrets,
   anything `credential_store::CredentialStore::save`/`load` handles.
3. Unhashed device identifiers, when the destination is a *diagnostic export a user
   might share with someone else* (design.md §17.3's "sanitized log" ZIP) — as
   opposed to the local, non-exported session manifest/ledger, where a raw device
   ID is required and expected (see "What's NOT a violation" below).

## What's allowed (design.md §17.2's allowlist)

`session_id`, platform, app version, a *hashed* device ID, state transitions, OS
error codes/messages, segment sequence numbers, and timing statistics
(§17.1: capture callback interval, ring buffer usage, dropped-frame count,
inserted-silence duration, resample correction amount, segment encode time,
segment size, upload throughput, retry count, source reconnect count).

## What's NOT a violation

- `session-store`'s `sessions`/`segments` tables storing `microphone_device_id`,
  `remote_source_id`, `local_path`, `sha256`, etc. — this is the functional local
  ledger (and mirrors `SessionManifest`'s own JSON shape from design.md §9.4), not
  a log or a diagnostic export. §17.2's "hash the device ID" requirement applies to
  the *optional diagnostic ZIP* (§17.3), which doesn't exist yet — build it as a
  dedicated sanitization step (a function whose whole job is "take a session's data
  and redact/hash it for export"), not by loosening what the ledger itself stores.
- Error message *text* from an external library (`reqwest::Error`, `rusqlite::Error`,
  `keyring::Error`, a device-invalidation `windows::core::Error`) — these describe
  what the OS/library call failed to do, not our own secrets or audio content.
  Still worth a second look if a new dependency's error type ever embeds request
  bodies or credentials verbatim (some HTTP client libraries do, on request-trace
  logging feature flags — don't enable those without checking).

## `tracing::instrument` guidance

No crate in this workspace uses `#[tracing::instrument]` yet (checked as of this
policy's writing — see the audit below). Before adding it to a function whose
arguments include a token, a segment's PCM/encoded bytes, or anything from
`credential-store`:

- Default to `#[instrument(skip_all)]`, then explicitly re-add only the specific
  fields from the allowlist above (`session_id`, `sequence`, ...) via
  `fields(...)`.
- Never `#[instrument]` bare (no `skip`/`skip_all`) on a function whose signature
  includes `&[f32]`/`Vec<f32>` (audio samples), a `token`/`secret`/`password`-named
  parameter, or an `AudioSegment`/`CapturedFrame`/`SessionManifest` passed by value
  — instrument specific scalar fields via `fields(...)` instead of the whole
  struct via `Debug`.
- The same applies to ad hoc `tracing::debug!(?some_struct, ...)` calls: prefer
  naming the specific safe fields over `?`-debug-formatting an entire struct that
  might grow a sensitive field later without the log call being revisited.

## Audit (current state, this policy's writing)

Every `tracing::`/`println!`/`eprintln!` call in `crates/` as of this task:

| Crate | Calls | Sensitive content? |
|---|---|---|
| `capture-windows` | `mmcss.rs`, `capture_loop.rs` (MMCSS registration, session disconnect, device invalidation, unsupported PCM format, `WaitForMultipleObjects` failure) | No — `stream_id` (a `BindingKind`), OS error text, format flags. No PCM samples, no device IDs, no tokens. |
| `upload-client` | `client.rs`'s retry log (`attempt`, `delay`, the classified `UploadError`) | No — never logs the bearer token or segment bytes; `UploadError`'s `Display` never includes either (see `recorder-domain::UploadError`). |
| `credential-store` | `lib.rs`'s keyring-unavailable fallback warning (`error = %e`) | No — logs the *error message* from the `keyring` crate (a backend failure description), never the secret being saved/loaded. |
| `app-service` (`windows_supervisor`) | worker stopped/errored logs (`binding`, `exit`, `mmcss_applied`, `error`) | No — no `CaptureEvent::Frame` (which carries samples) is ever logged; only lifecycle variants are. |
| `recorder-domain`, `session-store`, `segment-store`, `credential-store` (crypto), `audio-timeline`, `capture-api` | none | N/A |

No violations found. Re-run this grep when reviewing a PR that adds logging:

```
grep -rn "tracing::\|eprintln!\|println!" crates --include=*.rs
```

## Still open (not this task's scope, but downstream of it)

- The §17.3 diagnostic-export ZIP itself doesn't exist yet — building it (likely
  as part of `apps/desktop`, task #8, or a dedicated module) needs its own explicit
  device-ID-hashing step, sanitized separately from whatever the ledger stores raw.
- design.md §16.5 mentions recording a `device_switch` event when the bound device
  changes — once that's implemented (likely inside the future Windows supervisor's
  observation-normalization layer or `session-store`'s `events` table), make sure
  its payload follows this same allowlist (device ID hashed if it's ever exported).
- §17.1's metrics themselves aren't wired up to anything yet (no metrics-collection
  crate is in the workspace) — when one is added, this policy applies to metric
  labels/dimensions exactly as it does to log fields.
