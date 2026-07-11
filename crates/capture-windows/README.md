# capture-windows

WASAPI-backed audio capture for Windows: microphone and system-audio (endpoint
loopback) capture, with QPC-based timestamps precise enough to feed
[`audio-timeline`](../audio-timeline), and identifiers shared with
[`capture-api`](../capture-api)'s rebinding state machine (`BindingKind`, `DeviceRole`).

Ported from this project's `spikes/spike-01-wasapi-dual-capture` and
`spikes/spike-common`, which validated this against real Windows hardware.

## What's here

- `wasapi_common` / `mic_stream` / `loopback_stream`: resolve a target device (or
  "default"), activate + initialize a shared-mode `IAudioClient`, and hand off to...
- `capture_loop`: the event-driven capture loop shared by every stream kind —
  wait for a callback, drain packets via `GetBuffer`/`ReleaseBuffer`, detect
  `AUDCLNT_E_DEVICE_INVALIDATED` and `IAudioSessionEvents::OnSessionDisconnected` as
  device loss.
- `device_watch`: `IMMNotificationClient`-based observation of device add/remove,
  state changes, property changes, and default-device changes, forwarded as raw
  events (deliberately not interpreted on the callback thread — see the module docs).
- `device_select`: endpoint enumeration and resolution (`"default"` or a specific
  `IMMDevice::GetId()`).

## What's not here yet

Process loopback (capturing a specific application's audio only, e.g. just Zoom's
output) is not ported yet. Phase 1A only needs microphone + endpoint loopback capture;
process loopback is Phase 1B work.

Nothing here yet turns a `device_watch::DeviceWatchEvent` or a capture thread's exit
into a `capture_api::rebinding::Observation`/`Effect` — that adapter layer, and
whatever `CaptureAdapter`-style trait it ends up needing, is deliberately left for
once there's a second backend (or at least a supervisor loop) to inform its shape.
