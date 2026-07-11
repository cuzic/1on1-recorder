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
- **Stage 2 (task #10, not yet built)**: feeds `windows_supervisor`'s captured
  frames into this stage's `timeline_adapter`/`segmenter`/`pipeline` — converting
  `capture-windows`'s QPC-based timestamps into the `host_time_ns`/
  `nominal_duration_ns` `timeline_adapter` expects is the remaining piece that
  actually makes the pipeline Windows-only.
- **Stage 3 (task #11, not yet built)**: a standing upload worker and richer
  recording-state management (pause/resume, disk-space handling, `CaptureState`
  transitions beyond what this stage exercises).

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

## Known scope limits (stage 1)

- The pseudo source generates a steady, zero-drift, zero-jitter frame stream.
  Real clock-drift/jitter handling is `audio-timeline`'s own concern and is already
  covered by that crate's simulation tests — stage 1 only needs to prove pipeline
  wiring, not re-prove alignment math.
- `run_pipeline` processes both tracks fully before returning; it isn't structured
  for a long-running session that segments incrementally as capture progresses.
  That structure belongs to stage 3's upload worker / recording-state management.
- No disk-space, network-outage, or crash-mid-session testing here — that's task
  #13 (failure-injection integration tests), which depends on this stage's pipeline
  existing first.
