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
- **Stage 2 (task #10, not yet built)**: a Windows supervisor that runs
  `capture-api`'s FSM against real `capture-windows` output and converts its
  QPC-based timestamps into the `host_time_ns`/`nominal_duration_ns` this stage's
  `timeline_adapter` expects — this is the piece that makes the pipeline
  Windows-only. Depends on task #1 (the supervisor loop) as well.
- **Stage 3 (task #11, not yet built)**: a standing upload worker and richer
  recording-state management (pause/resume, disk-space handling, `CaptureState`
  transitions beyond what this stage exercises).

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
