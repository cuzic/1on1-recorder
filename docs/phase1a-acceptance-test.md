# Phase 1A acceptance test (task #9)

design.md §21's Phase 1A completion criterion:

> Zoom会議を30分録音し、Self / Remoteを別トラックでAPIへ送信できる。
> (Record a 30-minute Zoom meeting and ship Self/Remote as separate tracks to the API.)

This requires a real Windows machine with a real microphone and a real Zoom call —
nothing in this repository's dev/CI environment can execute it. Every crate this
test exercises (`capture-windows`, `app-service`'s `windows-supervisor` feature,
`apps/desktop`) has only ever been **cross-compile-checked** from Linux
(`cargo check/clippy --target x86_64-pc-windows-gnu`), never run. This is the
first time any of it touches real hardware. Expect to find bugs — that's what
this test is for.

## Prerequisites (on the Windows machine)

1. Rust (`rustup`, stable toolchain, MSVC target — not the `x86_64-pc-windows-gnu`
   target used for cross-compilation here) and a working WASAPI-capable audio
   setup (a real microphone, and speakers/headphones Zoom's audio will play
   through).
2. Node.js + `pnpm` (see this project's global rule: never `npm install`).
3. This repository, checked out at the commit this file ships in.
4. Zoom (or any app that plays audio through the system's default output — Phase
   1A's `EndpointLoopback` captures *all* system audio, not just Zoom's — see
   "Known limitation" below).

## 1. Start a mock upload API

No production API exists yet. `upload-client`'s mock server (design.md §13.1's
contract) is a legitimate stand-in for this test — the point is validating the
capture/align/segment/upload *pipeline*, not a specific backend:

```
cargo run -p upload-client --example mock_server_standalone --features mock-server
```

Leave it running (default `http://127.0.0.1:8787`). It logs every segment it
receives; watching that output during the test is useful.

## 2. Provision a bearer token

`apps/desktop` reads its token from `credential-store` (Windows Credential
Manager), not an in-app login screen (not part of Phase 1A's UI scope — see
`apps/desktop/README.md`). The mock server accepts any bearer token, so any
placeholder value works. Provisioning it directly via `credential-store`'s own
test surface, or via a short one-off Rust snippet calling
`credential_store::FallbackCredentialStore::save("1on1-recorder", "api-token",
"<any-value>")`, is the only way to set it right now — there is no CLI for this
yet (a known gap; `tools/recorderctl`, design.md §8, doesn't exist yet either).

## 3. Build and run the desktop app

```
cd apps/desktop
pnpm install
pnpm tauri dev
```

(`RECORDER_API_BASE_URL` defaults to `http://127.0.0.1:8787`, matching step 1 —
override it if you bound the mock server to a different port.)

## 4. Run the test

1. Confirm the recording-consent checkbox.
2. Start a Zoom call (or any call/audio source) and start recording in the app at
   roughly the same time.
3. Let it run for **30 minutes**. Periodically glance at the level meters (Self
   should move when you talk; Remote should move when the other party — or
   whatever's playing through your speakers — makes sound).
4. Stop the recording, then stop the Zoom call.

## 5. What "pass" looks like

- The mock server's own log shows segments arriving continuously (roughly one
  pair — Self + Remote — every 30 seconds) throughout the 30 minutes, not just
  at the very end.
- `get_status`'s `pending_segments` reaches 0 shortly after stopping (everything
  drained and finalized — see `app-service`'s `session_lifecycle::end_session`).
- The session directory under the app's data dir (`sessions/<session-id>/self/`
  and `.../remote/`) contains `.opus` files for the whole 30 minutes, playable
  and recognizable as your voice / the meeting's audio, respectively (`ffprobe`/
  any media player).
- No panics, hangs, or the app becoming unresponsive over the 30 minutes.

## 6. Known limitation to record, not fix, in this pass

`EndpointLoopback` (Phase 1A's Remote-track capture) captures the *entire*
system's audio output, not just Zoom's — if anything else makes sound during
the test (a notification, another app), it ends up in the Remote track too.
Isolating just Zoom's audio is `ProcessLoopback` (Phase 1B, `capture-windows`'s
own README already notes it's not implemented). This is an expected, already-
documented limitation, not a bug to chase during this test — see `app-service`'s
task #9 description and `capture-windows/README.md`.

## After the test

Whatever breaks is real, first-contact-with-hardware feedback on ~13 tasks worth
of code that has only ever been cross-compile-checked. Worth capturing, in order
of how likely to matter:

- Anything in `capture-windows`'s WASAPI init/capture-loop path (device
  resolution, format negotiation, the event-driven capture loop itself).
- `app-service::windows_supervisor`'s FSM wiring — does start/stop/shutdown
  actually behave as `capture-api::rebinding::decide()`'s tests assume.
- Timing/format assumptions in `windows_frame_collector`'s conversion layer
  (`nominal_frame_interval_ns` from `IAudioClient::GetDevicePeriod`, the QPC-to-ns
  conversion) — anything here being wrong would show up as audible drift/
  glitches once played back.
- `apps/desktop`'s actual UI behavior, entirely unverified before this (see that
  crate's README) — does the window render sensibly, do the meters update
  smoothly, does Stop actually feel responsive.

Once this test passes for real, Phase 1A (design.md §21) is complete.
