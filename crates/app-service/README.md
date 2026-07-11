# app-service

Orchestrates `capture -> align -> segment -> encode -> commit -> upload -> finalize`
for the meeting recorder. Application-internal, not a publishing candidate.

Per Codex's review of the original task list, this is being built in three stages
rather than as one large "wire everything to real Windows capture" task:

- **Stage 1 (task #7, this crate's current state)**: an OS-independent pipeline
  driven by `pseudo_source` (synthetic frames, no real audio hardware or OS capture
  API). Proves the wiring between `audio-timeline`, `segment-store`,
  `session-store`, and `upload-client` — including the conversion layers a real
  capture backend needs that a pipeline built only against `capture-windows` would
  have buried inside Windows-specific code:
  - `normalize.rs`: downmix-to-mono + resample-to-target-rate, since
    `audio-timeline` assumes mono at one fixed nominal rate but a capture backend
    can hand back stereo and/or a device's native sample rate.
  - `timeline_adapter.rs`: converts a track's `recorder_domain::CapturedFrame`
    sequence into `audio_timeline::AudioPacket`s and drives a `TimelineAligner`.
  - `segmenter.rs`: cuts aligned PCM into fixed-duration chunks, numbered so `Self`
    and `Remote` share the same `sequence` for the same `timeline_start_ms` window.
  - `pipeline.rs`: wires all of the above together with `segment-store` (encode +
    atomic commit), `session-store` (the ledger), and `upload-client` (HTTP upload),
    end to end. See `tests/e2e.rs`.
- **Stage 2 (task #10, this crate's current state)**: `windows_frame_collector`
  converts `windows_supervisor`'s captured frames into
  `recorder_domain::CapturedFrame`, and `windows_session::run_windows_capture_session`
  feeds them through the *exact same* stage 1 `run_pipeline` — no second,
  Windows-specific pipeline was built.
- **Stage 3 (task #11, this crate's current state)**: `upload_worker` and
  `session_lifecycle` drive design.md §10's `CaptureState`/`UploadState`
  transitions and force-quit crash recovery — see below.

## `windows_supervisor` (task #1)

Behind the `windows-supervisor` Cargo feature (off by default, so this crate's
default build stays OS-independent for stage 1's pipeline). Executes
`capture_api::rebinding::decide()`'s effects against real `capture-windows` capture
threads: `Effect::StartCapture` spawns a `MicCaptureStream`/`EndpointLoopbackStream`
via `spawn_capture_thread`; `Effect::StopCapture` signals the worker's `StopSignal`
and joins it on a dedicated thread, feeding the join result back through `decide()`
as `Observation::WorkerStopped`; `Effect::ScheduleRetry` sleeps on a thread before
firing `DecisionInput::RetryTimerFired`. `CaptureEvent`/`DeviceWatchEvent`s are
normalized into `Observation`s (Ctrl+C — or any other shutdown trigger the caller
wires up — becomes `DecisionInput::ShutdownRequested`, not a generic
`Observation`, per Codex's review).

This module can only be **type-checked** from this Linux dev environment (Windows
cross-compilation, the same way `capture-windows` itself has always been verified
here — see this project's earlier commits):

```
cargo check -p app-service --features windows-supervisor --target x86_64-pc-windows-gnu
cargo clippy -p app-service --features windows-supervisor --target x86_64-pc-windows-gnu --all-targets
```

Its actual runtime behavior — does the FSM really rebind correctly against a real
WASAPI device change, does shutdown really drain every worker — has **not** been
exercised on real Windows hardware yet. That's part of task #9's real-machine
30-minute Zoom test, once stage 2 (task #10) wires this into the full pipeline.

### Known limitations

- `decide()`'s `ShutdownRequested` handling only stops bindings currently
  `Running` (see `capture-api`'s rebinding module) — a binding still
  `Starting`/`Waiting` when shutdown arrives gets no `StopCapture` effect, and
  `run_until_shutdown`'s join-draining loop won't wait for it (it isn't tracked).
  In practice this only matters if shutdown races with a fresh start/rebind
  attempt.
- `DeviceWatch::start()` and `run_until_shutdown` must run on the *same* thread
  (per `DeviceWatch`'s own requirement that its creating thread stay alive) — the
  caller is responsible for that ordering; this module doesn't enforce it.
- Process loopback (`BindingKind::ProcessLoopback`) is not wired up — Phase 1A only
  manages Microphone and EndpointLoopback, matching `capture-windows`'s own scope.

## `windows_frame_collector` / `windows_session` (task #10)

Also behind `windows-supervisor`. `windows_supervisor` forwards `StreamStarted`/
`Frame` events to an optional sink (`WindowsSupervisor::set_frame_sink`) rather than
exposing its raw event channel for a second consumer to clone — cloning
`crossbeam_channel::Receiver<CaptureEvent>` would make the supervisor's own event
loop and an external consumer *competing* readers on the same channel, silently
dropping whichever `Frame` events the supervisor's own (frame-discarding) loop won
the race for. See `FrameSinkEvent`'s doc comment in `windows_supervisor.rs`.

`windows_frame_collector::collect_frames` drains that sink on its own thread,
converting each frame:

- `host_time_ns` is `capture_qpc_100ns * 100` — already a normalized, monotonic,
  machine-wide clock (`capture_windows::timestamp::QpcClock`), just converted from
  100ns units to nanoseconds.
- `source_time_ns` is derived from `device_position_frames` (the device's own
  cumulative sample counter) at the stream's sample rate — a second, independent
  "how much time has the device itself counted" value, for diagnostics only (not
  used for alignment).
- `nominal_frame_interval_ns` (`audio_timeline::AudioPacket`'s per-packet expected
  duration) comes from `IAudioClient::GetDevicePeriod`'s shared-mode default period
  — a fixed, engine-configured value queried once at stream start
  (`CaptureEvent::StreamStarted`), **not** derived from any one frame's
  `frame_count`. Using the device's own reported sample count as "nominal" would
  make clock drift undetectable by definition, since `TimelineAligner` works by
  comparing the nominal (drift-free) expectation against what the device actually
  delivered.

`windows_session::run_windows_capture_session` ties it together: run
`WindowsSupervisor` + the collector until `shutdown_rx` fires, then call stage 1's
`run_pipeline` with whatever was collected. Blocking calls
(`WindowsSupervisor::run_until_shutdown`, the collector thread's `join()`) run
inside `tokio::task::spawn_blocking`, since `DeviceWatch::start`/
`run_until_shutdown` must share one dedicated OS thread (see `DeviceWatch`'s own
requirement that its creating thread stay alive).

Buffers the entire session's audio in memory before running the pipeline —
correctly scoped for proving real capture flows end to end, not for how a
long-running recording should work. Incremental segmenting as capture progresses
is stage 3's job (task #11).

## `upload_worker` / `session_lifecycle` (task #11)

OS-independent (no `windows-supervisor` feature needed) — used identically by
stage 1's `pseudo_source` path and stage 2's real Windows path.

- `upload_worker::upload_pending_once` attempts every segment
  `SessionStore::pending_uploads` currently returns, once each, transitioning
  `Uploading -> {Completed, Failed}` per segment rather than aborting on the first
  failure. `run_until_drained` repeats this (with a sleep between passes) until
  nothing is pending or `max_passes` is exhausted — the backstop against a bug in
  error classification, not something expected to bind in normal operation.
- `session_lifecycle::begin_session`/`end_session` drive design.md §10's
  `CaptureState` diagram: `begin_session` registers the session (locally and with
  the API) and moves to `Recording`; `end_session` moves through
  `Stopping -> Finalizing`, drains any still-pending uploads via
  `run_until_drained`, calls `UploadAdapter::finalize_session`, and moves to
  `Finalized`. `pipeline::run_pipeline` now calls both instead of duplicating
  session bootstrapping inline, and `commit_and_upload_track`'s per-segment
  upload failures no longer abort the pipeline (`?`-propagate) — they're marked
  `Failed` and picked up by `end_session`'s drain pass instead, per design.md
  §13.4 ("an upload failure must not block recording").
- `session_lifecycle::recover_incomplete_sessions` is this task's other half —
  the wiring the task description asks for by name: at startup, before any new
  recording starts, it calls `SessionStore::reconcile_on_startup` (finds sessions
  a previous process instance left mid-flight), then `segment_store::
  scan_and_recover` for each of Phase 1A's two tracks (picks up any segment whose
  atomic commit was interrupted between rename and DB registration), then resumes
  uploading and finalizes each recovered session that already has a
  `remote_session_id`. See `tests/upload_failure_and_recovery.rs` for both
  behaviors end to end (including a real `commit_segment(..., CrashPoint::
  AfterRename)` simulating the crash).

**Known gap**: a session that crashed *before* ever getting a `remote_session_id`
(i.e. before `UploadAdapter::create_session`'s response was ever stored — a very
narrow window right at a session's start) isn't resumed automatically;
reconstructing its `SessionManifest` to retry `create_session` needs a getter
`session-store` doesn't have yet (its `sessions` table has every needed field,
just not a query that reassembles them into a `SessionManifest`). Such a session
is left `Failed` rather than silently dropped, but a human would currently need
to intervene to finish it.

## `tests/failure_injection.rs` (task #13)

Three scenarios, each exercising `session_lifecycle`/`upload_worker` the way a
real deployment would hit them:

- **Disk write failure during recording**: the session directory is made
  read-only (`chmod 0o555`) before `run_pipeline` runs, so `segment-store`'s
  `commit_segment` fails with a permission error (the same failure mode as
  running out of disk space — the OS refuses the write) instead of needing a
  real size-limited filesystem to reproduce that. `run_pipeline` errors out and
  the session is left at `CaptureState::Recording`; once the directory is
  writable again, `recover_incomplete_sessions` (the same startup path a real
  restart would run) drives it to `Finalized`.
- **Network outage from the very start of a session**: `begin_session`'s
  `UploadAdapter::create_session` call is pointed at an address nothing is
  listening on. The session exists locally (`CaptureState::Preparing`) but never
  gets a `remote_session_id` — and `recover_incomplete_sessions`, once the
  network is back, correctly does *not* try to resume it (the documented known
  gap two sections up), rather than silently discarding or mishandling it.
- **Network outage through the end of a session, then a restart**: one segment
  uploads successfully before the network drops; the next commits locally but
  every upload attempt against the (deliberately unreachable) endpoint fails.
  `end_session`'s finalize call fails too (the API's own segment-count check
  correctly refuses to finalize an incomplete session) and the session is left
  short of `Finalized`. A later `recover_incomplete_sessions` call (network
  restored) resumes and finishes it — and the mock server's Idempotency-Key
  dedup means the segment that failed once and succeeded later was still only
  ever *written* once server-side.

## Known scope limits (stage 1)

- The pseudo source generates a steady, zero-drift, zero-jitter frame stream.
  Real clock-drift/jitter handling is `audio-timeline`'s own concern and is already
  covered by that crate's simulation tests — stage 1 only needs to prove pipeline
  wiring, not re-prove alignment math.
- `run_pipeline` processes both tracks fully before returning; it isn't structured
  for a long-running session that segments incrementally as capture progresses.
  That structure belongs to a future incremental-segmenting redesign, not
  something this stage or task #13 covers.
