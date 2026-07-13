# capture-macos

ScreenCaptureKit-backed audio capture for macOS 15+: microphone and system-audio
output capture from a single `SCStream`, with host-clock-anchored timestamps precise
enough to feed [`audio-timeline`](../audio-timeline), and identifiers shared with
[`capture-api`](../capture-api)'s rebinding state machine (`BindingKind`, `DeviceRole`).

Mirrors [`capture-windows`](../capture-windows)'s shape (`CaptureEvent`/
`CaptureStream`/`StopSignal`/`spawn_capture_thread`), adapted for ScreenCaptureKit's
callback-driven model instead of WASAPI's poll loop — see `lib.rs`'s and
`sc_stream.rs`'s module doc comments for the structural differences (one shared
stream instead of two independent ones, no "nominal frame interval" concept the way
WASAPI's `GetDevicePeriod` provides one).

**Status: not yet verified against a real build.** This dev environment has no
macOS host, so nothing in this crate has been compiled or run — see the caveats in
each module's doc comment. The first real build happens in CI
(`.github/workflows/macos-build.yml`) or on a real Mac, not here.

## What's here

- `sc_stream`: the `CaptureStream` implementation. Registers one `SCStream` with up
  to two output handlers (`SCStreamOutputType::Microphone` /
  `SCStreamOutputType::Audio`), converting each `CMSampleBuffer` into a
  `CapturedFrameRecord` + `Vec<f32>` and forwarding it as `CaptureEvent::Frame`.
- `app_filter`: builds `SCContentFilter`s — unfiltered (`BindingKind::EndpointLoopback`)
  or scoped to one running application (`BindingKind::ProcessLoopback`).
- `device_select`: CoreAudio device enumeration (`enumerate_capture_devices`/
  `enumerate_render_devices`, mirroring `capture-windows`'s functions of the same
  name) plus running-application enumeration via ScreenCaptureKit's own
  `SCShareableContent` (no separate process-enumeration dependency needed).
- `device_watch`: CoreAudio `AudioObjectPropertyListenerBlock`-based device-change
  notifications, forwarded as raw `DeviceWatchEvent`s (deliberately not resolved on
  the listener callback — same "notification is a trigger to re-enumerate, not a
  payload to trust" pattern `capture-windows::device_watch` uses).
- `permissions`: TCC status checks. Screen & System Audio Recording is preflighted
  via `SCShareableContent::get()`; Microphone has no ScreenCaptureKit-level
  preflight, so it's discovered lazily from a failed stream start instead (see the
  module doc comment for why this doesn't need a second Objective-C binding
  dependency).
- `timestamp`: `CMTime` (rational time value) → nanoseconds conversion. Pure
  arithmetic, unit-tested directly against fixture values — no TCC grant, real
  ScreenCaptureKit/CoreAudio access, or real audio hardware needed for these tests
  to pass (though, like the rest of this crate, a macOS host/CI runner is still
  needed to *compile* it at all) — see the module doc comment for why this does
  *not* need a
  `mach_timebase_info` step, refining what the original design plan assumed.

## `BindingKind` mapping

| ScreenCaptureKit output | `BindingKind` |
|---|---|
| `SCStreamOutputType::Microphone` | `Microphone` |
| `SCStreamOutputType::Audio` (unfiltered) | `EndpointLoopback` |
| `SCStreamOutputType::Audio` (app-filtered via `app_filter::scoped_to_application`) | `ProcessLoopback` |

`ProcessLoopback` was never implemented on Windows (WASAPI process-loopback wasn't
ported to `capture-windows`) — ScreenCaptureKit's per-app `SCContentFilter` fits this
binding more naturally, so it's lit up here first.

## What's not here yet

- `CaptureEvent::StreamStarted::nominal_frame_interval_ns` is currently always `0`.
  ScreenCaptureKit has no WASAPI-`GetDevicePeriod` equivalent (it's callback-driven
  at whatever cadence the OS delivers buffers, not a fixed queried period) — a real
  nominal value needs either an empirical measurement approach or confirming
  whether `SCStreamConfiguration` exposes a fixed delivery interval on a real build.
  Needed before `audio_timeline::TimelineAligner`-based drift detection is
  meaningful for this backend.
- `sc_stream::FrameForwarder`'s `discontinuity`/`silent` flags are always `false` —
  ScreenCaptureKit's dropped-buffer signaling (if any, via `SCStreamFrameInfo`'s
  status field) hasn't been confirmed against a real build yet.
- The adapter layer that turns a `device_watch::DeviceWatchEvent`/capture thread
  exit into a `capture_api::rebinding::Observation`/`Effect` lives in `app-service`'s
  `macos_supervisor` module, not here — same layering `capture-windows`'s README
  documents for `windows_supervisor`.
- `device_watch::ApplicationWatch` (meeting-app relaunch detection via `NSWorkspace`
  notifications) is stubbed — needs an `objc2-app-kit`-style binding this crate
  doesn't pull in yet, deferred until `macos_supervisor` actually wires it up.
