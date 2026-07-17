# apps/desktop

A single Rust binary crate on `dioxus-desktop` (task #30), wired to the real
recording pipeline (`app-service`, `session-store`, `upload-client`,
`credential-store`) — originally task #8's Tauri 2 + Vue 3 shell (ported from
`spikes/spike-05-tauri-tray`), migrated off Tauri so the whole app — UI included
— is Rust, and so `commands.rs`'s IPC layer could go away: `src/ui.rs` calls
`src/actions.rs`/`app_service` in-process, not over `invoke()`.

Per design.md §21 Phase 1A's UI list, plus what later tasks added on top:

- Recording start/stop
- Self/Remote level meters
- Elapsed time
- Uploaded/pending segment counts
- Error display
- Recording consent confirmation
- Settings screen (`src/settings.rs`, tasks #31/#37): Deepgram API key, and the
  summarization provider/model selection + its API key

Not built (explicitly out of Phase 1A's scope per the original task list's
review): device selection UI (mic/remote-source pickers — `recording.rs`
currently hardcodes `"default"` for both), the history screen (design.md
§14.3), and OS-specific permission-error redirect flows (§14.4). Tray hide/show
(ported from spike-05 via `dioxus-desktop`'s `trayicon` module, see below) is
kept working but is **not** required for Phase 1A's completion criterion.

## Rust is the single source of truth

Per design.md §6.1, capture/upload state lives entirely in `src/app_state.rs`'s
`AppState` — `src/ui.rs`'s `App` component only polls `actions::get_status`
(every 250ms, via a `use_future` loop) and calls `actions::confirm_consent`/
`start_recording`/`stop_recording` directly. There is no separate frontend
process and no IPC boundary to keep in sync — the UI component and the
recording logic run in the same process, so there is only ever one copy of
recording state to begin with.

## Real capture vs. dev-mode fallback

`capture-windows` (and so `app-service`'s `windows-supervisor` feature) is
Windows-only. `src/recording.rs` is split accordingly:

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

- **Rust**: `cargo check -p desktop` / `cargo clippy -p desktop` (this Linux
  environment, dev-mode fallback path, WebKitGTK renderer via `dioxus-desktop`)
  and `cargo check -p desktop --target x86_64-pc-windows-gnu` (Windows
  cross-compile, real-capture + `live-transcription` path) both pass — see the
  same cross-compilation approach used throughout `capture-windows`/
  `app-service`'s `windows-supervisor` feature. There is no separate frontend
  build step (no `pnpm`/`vite`/`vue-tsc`) — the whole app is this one crate.
- **Actual UI behavior — NOT verified.** This dev container's WebKitGTK cannot
  initialize a real WebView window (a pre-existing, documented limitation — see
  `spikes/spike-05-tauri-tray/soak_test.sh`'s own note: WebKitGTK hangs here even
  running its own `MiniBrowser` directly, independent of Tauri/Dioxus). The
  actual window has not been launched, clicked through, or visually inspected.
  Whether the meters render sensibly, whether `start`/`stop`/the settings screen
  actually work end to end from real clicks, and whether the tray hide/show
  behavior still works are all unverified here — they need a real
  Windows/macOS/Linux desktop (or a Linux VM with working 3D acceleration) to
  check.

## Configuration (Phase 1A: fixed endpoint, settings screen for API keys only)

`src/config.rs`: the API base URL comes from `RECORDER_API_BASE_URL`
(placeholder default, `http://127.0.0.1:8787`, if unset) — design.md §21's "固定
APIへのアップロード". The bearer token is expected to already be provisioned in
`credential-store` under service `"1on1-recorder"` / account `"api-token"` before
recording is attempted; there is no in-app "log in" flow (also not in Phase 1A's
UI scope list).

The Deepgram and summarization-provider API keys, unlike the bearer token above,
*do* have an in-app entry point: the settings screen (`src/settings.rs`, gear
button on the main screen), saved via the same `credential-store` under service
`"1on1-recorder"` / accounts `stt_deepgram::DEEPGRAM_API_KEY_ACCOUNT` and
`summarize::{CLAUDE,OPENAI}_API_KEY_ACCOUNT` — never displayed back once saved,
only a "configured/not configured" status.
