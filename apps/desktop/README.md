# apps/desktop

The Tauri 2 + Vue 3 desktop shell (design.md §6/§8), ported from
`spikes/spike-05-tauri-tray` and wired to the real recording pipeline
(`app-service`, `session-store`, `upload-client`, `credential-store`) — task #8.

Per that task's scope note (Codex's review of the original task list), this
covers exactly design.md §21 Phase 1A's UI list and nothing more:

- Recording start/stop
- Self/Remote level meters
- Elapsed time
- Uploaded/pending segment counts
- Error display
- Recording consent confirmation

Not built (explicitly out of Phase 1A's scope per that same review): device
selection UI (mic/remote-source pickers — `recording.rs` currently hardcodes
`"default"` for both), the history screen (design.md §14.3), and OS-specific
permission-error redirect flows (§14.4). Tray hide/show (ported from spike-05, see
below) is kept working but is **not** required for Phase 1A's completion
criterion.

## Rust is the single source of truth

Per design.md §6.1, capture/upload state lives entirely in the Tauri backend
(`src-tauri/src/state.rs`'s `AppState`) — the Vue frontend only polls
`get_status` (`src/App.vue`, every 250ms) and calls `confirm_consent`/
`start_recording`/`stop_recording`. It never tracks its own copy of recording
state.

## Real capture vs. dev-mode fallback

`capture-windows` (and so `app-service`'s `windows-supervisor` feature) is
Windows-only. `src-tauri/src/recording.rs` is split accordingly:

- **`#[cfg(windows)]`**: `start` spawns `app_service::run_windows_capture_session`
  in the background (real WASAPI capture via `WindowsSupervisor`); `stop` signals
  its shutdown channel and awaits the result. The level meter is real — updated
  live by `app_service::windows_frame_collector::collect_frames`'s optional
  level-sink parameter as frames actually arrive.
- **`#[cfg(not(windows))]`** (this Linux dev environment, and any other non-Windows
  build): `start` just records a start time; nothing runs in the background.
  `stop` generates exactly enough `pseudo_source` audio for however much real
  time elapsed and runs it through the *same* `run_pipeline` a real session
  uses — so session-store/segment-store/upload-client are genuinely exercised
  locally, but no actual microphone input is ever captured. The level meter is a
  deterministic placeholder (`level::dev_placeholder_level`, ported from
  spike-05's `synthesize_level`) — not real audio, and never used when
  `cfg(windows)`.

## What's verified vs. not

- **Rust backend**: `cargo check -p desktop` / `cargo clippy -p desktop` (this
  Linux environment, dev-mode fallback path) and `cargo check -p desktop --target
  x86_64-pc-windows-gnu` (Windows cross-compile, real-capture path) both pass —
  see the same cross-compilation approach used throughout `capture-windows`/
  `app-service`'s `windows-supervisor` feature.
- **Frontend**: `pnpm install && pnpm exec vue-tsc --noEmit && pnpm run build`
  all succeed (type-checks and bundles cleanly).
- **Actual UI behavior — NOT verified.** This dev container's WebKitGTK cannot
  initialize a real WebView window (a pre-existing, documented limitation — see
  `spikes/spike-05-tauri-tray/soak_test.sh`'s own note: WebKitGTK hangs here even
  running its own `MiniBrowser` directly, independent of Tauri). `tauri dev`/
  `tauri build`'s actual window has not been launched, clicked through, or
  visually inspected. Whether the meters render sensibly, whether `start`/`stop`
  actually work end to end from a real button click, and whether the tray
  hide/show behavior still works are all unverified here — they need a real
  Windows/macOS/Linux desktop (or a Linux VM with working 3D acceleration) to
  check.

## Configuration (Phase 1A: fixed endpoint, no settings UI)

`src-tauri/src/config.rs`: the API base URL comes from `RECORDER_API_BASE_URL`
(placeholder default, `http://127.0.0.1:8787`, if unset) — design.md §21's "固定
APIへのアップロード". The bearer token is expected to already be provisioned in
`credential-store` under service `"1on1-recorder"` / account `"api-token"` before
recording is attempted; there is no in-app "log in" flow (also not in Phase 1A's
UI scope list).
