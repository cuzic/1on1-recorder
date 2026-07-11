# capture-api

OS-independent building blocks for a resilient audio capture engine.

## `rebinding`: Capture Rebinding State Machine

A pure decision function for safely rebinding an audio capture stream (microphone,
endpoint loopback, or process loopback) whenever:

- the device it's bound to disappears,
- the system's default device changes, or
- (for process loopback) the target process exits or restarts.

It follows an Observation -> Admission -> Decision -> Effect pattern: whatever
actually talks to the OS (WASAPI, PipeWire, ScreenCaptureKit, ...) reports raw facts as
[`Observation`](src/rebinding.rs)s, and [`decide`](src/rebinding.rs) — the only place
state is written — turns them into a list of [`Effect`](src/rebinding.rs)s for the
caller to execute. `decide` takes no current time, no randomness, does no I/O, and
spawns no threads, which makes the whole rebinding policy testable without any real
hardware (see `tests/scenarios.rs`) and reusable across completely different capture
backends.

Guarantees this state machine provides:

- A pinned device that disappears waits for that exact device to come back; it never
  silently falls back to a different (e.g. the system default) device.
- A `FollowDefault` binding switches to the new default only after the old stream's
  stop has actually completed — never starts a new worker while one is still stopping.
- Every (re)bind gets a fresh, monotonically-increasing epoch, so a consumer can always
  tell a stale frame from a previous binding generation apart from a current one.
- Stale observations (an old operation/epoch, a duplicate `WorkerStarted`, a delayed
  retry timer) are rejected rather than corrupting the current state.
- Retries are capped ([`MAX_RETRY_ATTEMPTS`](src/rebinding.rs)); an unrecoverable
  failure moves to `Failed` instead of retrying forever.
- Process loopback ignores endpoint-side events entirely and only reacts to the target
  process itself exiting or restarting.

## Status

Only the rebinding policy is published so far. A `CaptureAdapter` trait connecting this
policy layer to real OS backends (WASAPI/PipeWire/ScreenCaptureKit) is intentionally
not defined yet — it will be added once a Windows backend is implemented and its shape
is grounded in an actual implementation, rather than designed speculatively ahead of
any real backend using it.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
