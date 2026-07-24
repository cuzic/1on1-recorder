use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use dioxus::desktop::trayicon::{self, DioxusTrayIcon, DioxusTrayMenu};
use dioxus::desktop::{use_tray_icon_event_handler, use_tray_menu_event_handler, use_window};
use dioxus::html::geometry::PixelsVector2D;
use dioxus::prelude::*;
use recorder_domain::{SessionId, TrackKind};
use session_store::{TranscriptSegment, TranscriptionGap};

use crate::actions;
use crate::app_state::AppState;
use crate::capture_health;
use crate::export;
use crate::gap_retranscription::{self, GapRetranscribeState};
use crate::hint_consumer::HintState;
use crate::history;
use crate::settings::{self, Screen};
use crate::status::Status;
use crate::transcript::{self, TimelineItem};
use crate::transcription_status;

const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.png");

const STYLE: &str = r#"
.container {
  margin: 0;
  padding: 6vh 2rem;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 1.25em;
  font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
}
.panel {
  width: 100%;
  max-width: 360px;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 0.9em;
}
.hint {
  font-size: 0.85em;
  opacity: 0.7;
  margin: 0;
}
.consent {
  display: flex;
  align-items: center;
  gap: 0.5em;
  font-size: 0.9em;
  text-align: left;
}
button {
  padding: 0.6em 1.4em;
  border-radius: 6px;
  border: none;
  font-size: 1em;
  cursor: pointer;
}
button.primary {
  background: #2ecc71;
  color: #04210f;
}
button.stop {
  background: #e74c3c;
  color: #2a0703;
}
button:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.elapsed {
  font-size: 2em;
  font-variant-numeric: tabular-nums;
  margin: 0;
  display: flex;
  align-items: center;
  gap: 0.4em;
}
.dot {
  width: 12px;
  height: 12px;
  border-radius: 50%;
  background: #e74c3c;
}
.meter-row {
  width: 100%;
  display: flex;
  align-items: center;
  gap: 0.6em;
}
.meter-label {
  width: 4.5em;
  font-size: 0.85em;
  opacity: 0.8;
}
.meter {
  position: relative;
  flex: 1;
  height: 18px;
  background: #333;
  border-radius: 4px;
  overflow: visible;
}
.meter-fill {
  height: 100%;
  background: linear-gradient(90deg, #2ecc71, #f1c40f, #e74c3c);
  border-radius: 4px;
}
.meter-peak {
  position: absolute;
  top: -3px;
  width: 2px;
  height: 24px;
  background: white;
}
.stats {
  font-family: monospace;
  font-size: 0.85em;
  opacity: 0.7;
  margin: 0;
}
.error {
  color: #e74c3c;
  font-size: 0.9em;
  max-width: 360px;
  text-align: center;
}
.warning {
  color: #d97706;
  font-size: 0.85em;
  max-width: 360px;
  text-align: center;
  margin: 0;
}
.header-row {
  width: 100%;
  max-width: 360px;
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.header-row h1 {
  margin: 0;
}
.header-actions {
  display: flex;
  align-items: center;
  gap: 0.3em;
}
button.gear {
  padding: 0.4em 0.6em;
  background: transparent;
  color: inherit;
  font-size: 1.1em;
}
.transcript-panel {
  width: 100%;
  max-height: 240px;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 0.5em;
  padding: 0.5em;
  border: 1px solid #444;
  border-radius: 8px;
}
.hint-panel {
  width: 100%;
  padding: 0.5em 0.75em;
  border: 1px solid #7a6a2a;
  border-radius: 8px;
  background: rgba(255, 200, 50, 0.08);
  display: flex;
  flex-direction: column;
  gap: 0.2em;
}
.hint-panel-text {
  margin: 0;
}
.hint-panel-meta {
  font-size: 0.8em;
  opacity: 0.6;
}
.bubble-row {
  display: flex;
}
.bubble-row.bubble-self {
  justify-content: flex-end;
}
.bubble-row.bubble-remote {
  justify-content: flex-start;
}
.bubble-row.bubble-unknown {
  justify-content: center;
}
.bubble {
  max-width: 80%;
  padding: 0.5em 0.8em;
  border-radius: 10px;
  text-align: left;
}
.bubble-row.bubble-self .bubble {
  background: #2ecc7133;
}
.bubble-row.bubble-remote .bubble {
  background: #3498db33;
}
.bubble-row.bubble-unknown .bubble {
  background: #55555533;
}
.bubble.bubble-interim {
  opacity: 0.6;
  font-style: italic;
}
.bubble-label {
  display: block;
  font-size: 0.75em;
  opacity: 0.7;
  margin-bottom: 0.2em;
}
.bubble-text {
  margin: 0;
  white-space: pre-wrap;
}
.gap-row {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 0.3em;
  padding: 0.5em 0.8em;
  border: 1px dashed #f1c40f88;
  border-radius: 8px;
  background: #f1c40f11;
  text-align: center;
}
.gap-label {
  margin: 0;
  font-size: 0.8em;
  opacity: 0.85;
}
.gap-hint {
  margin: 0;
  font-size: 0.75em;
  opacity: 0.6;
}
.gap-error {
  margin: 0;
  font-size: 0.75em;
  color: #e74c3c;
}
button.gap-retranscribe {
  padding: 0.35em 0.9em;
  font-size: 0.85em;
  background: #f1c40f;
  color: #2a2000;
}
.summary-text {
  width: 100%;
  box-sizing: border-box;
  max-height: 240px;
  overflow-y: auto;
  white-space: pre-wrap;
  font-size: 0.9em;
  text-align: left;
  padding: 0.6em;
  border: 1px solid #444;
  border-radius: 8px;
}
"#;

// #53: how close (in px) to the panel's bottom counts as "already at the
// bottom" for auto-scroll purposes — small enough to not trigger while
// reading older messages, large enough to absorb sub-pixel rounding.
const AUTO_SCROLL_NEAR_BOTTOM_PX: f64 = 24.0;

fn format_elapsed(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

/// Ported from `spikes/spike-05-tauri-tray`'s tray icon (kept per task #8's own
/// scope note, not part of Phase 1A's completion criteria) — "Show" brings the
/// main window back after `WindowCloseBehaviour::WindowHides` hid it, "Quit"
/// exits the whole app.
fn setup_tray() {
    let menu = use_hook(|| {
        let tray_menu = DioxusTrayMenu::new();
        let show_item = trayicon::menu::MenuItem::with_id("show", "Show", true, None);
        let quit_item = trayicon::menu::MenuItem::with_id("quit", "Quit", true, None);
        tray_menu.append_items(&[&show_item, &quit_item]).ok();
        tray_menu
    });

    let icon = use_hook(|| dioxus::desktop::icon_from_memory::<DioxusTrayIcon>(ICON_BYTES).ok());

    use_hook(|| trayicon::init_tray_icon(menu.clone(), icon.clone()));

    let window = use_window();
    use_tray_menu_event_handler(move |event| match event.id.0.as_str() {
        "show" => {
            window.set_visible(true);
            window.set_focus();
        }
        "quit" => std::process::exit(0),
        _ => {}
    });

    let window = use_window();
    use_tray_icon_event_handler(move |event| {
        if let trayicon::TrayIconEvent::Click { button: trayicon::MouseButton::Left, button_state: trayicon::MouseButtonState::Up, .. } = event {
            window.set_visible(true);
            window.set_focus();
        }
    });
}

/// Task #92: one `transcription_gaps` (#90) marker in the transcript panel's
/// timeline (`transcript::timeline_items`) — "この区間は文字起こしできません
/// でした", plus a "この区間を再文字起こしする" button (#91) when both
/// `can_retranscribe` (the selected STT provider has a `BatchSttProvider`
/// adapter — see `gap_retranscription::supports_batch_retranscription`) and the
/// gap is closed (`gap.end_ms.is_some()`) hold; an explanatory line instead of
/// a button otherwise (requirement #2: no dead button, just a reason).
///
/// A standalone `#[component]` (rather than a plain fn spliced into `App`'s
/// `for` loop) so its own `use_context::<Arc<AppState>>()` and the click
/// handler's `spawn`ed future are scoped to just this one gap — clicking one
/// gap's button doesn't need to know or care about any other gap on screen.
#[component]
fn GapMarker(
    gap: TranscriptionGap,
    can_retranscribe: bool,
    provider_kind: app_service::SttProviderKind,
    selected_session_id: Signal<Option<SessionId>>,
    mut transcript_segments: Signal<Vec<TranscriptSegment>>,
    mut gaps: Signal<Vec<TranscriptionGap>>,
    mut gap_retranscribe_state: Signal<HashMap<i64, GapRetranscribeState>>,
) -> Element {
    let state = use_context::<Arc<AppState>>();
    let gap_id = gap.id;
    let gap_closed = gap.end_ms.is_some();
    let track_label = transcript::track_label(Some(gap.track));
    let label = match gap.end_ms {
        Some(end_ms) => format!("⚠ {track_label}: この区間({})は文字起こしできませんでした", format_elapsed(end_ms.saturating_sub(gap.start_ms))),
        None => format!("⚠ {track_label}: この区間は文字起こしできませんでした"),
    };

    let current_state = gap_retranscribe_state().get(&gap_id).cloned();
    let is_loading = matches!(current_state, Some(GapRetranscribeState::Loading));
    let error_text = match current_state {
        Some(GapRetranscribeState::Error(msg)) => Some(msg),
        _ => None,
    };

    let on_retranscribe = move |_| {
        let state = state.clone();
        async move {
            gap_retranscribe_state.with_mut(|m| {
                m.insert(gap_id, GapRetranscribeState::Loading);
            });
            match gap_retranscription::retranscribe(gap, provider_kind, &state.store, state.credential_store.as_ref()).await {
                Ok(new_segments) => {
                    // The gap is resolved server-side too (`retranscribe_gap`
                    // already called `SessionStore::discard_gap` — see task #91's
                    // doc comment), so drop it from both the local `gaps` list and
                    // any leftover loading/error entry, and fold the newly
                    // persisted rows straight into `transcript_segments` —
                    // `transcript::timeline_items` re-sorts by `start_ms` on every
                    // render, so appending here still lands them in the right
                    // chronological spot without waiting for the next poll tick.
                    gap_retranscribe_state.with_mut(|m| {
                        m.remove(&gap_id);
                    });
                    // Guard against a session switch that happened while this
                    // request was in flight: `transcript_segments`/`gaps` are
                    // shared, session-agnostic signals (`ui::App` reloads them
                    // wholesale on every `selected_session_id` change), not scoped
                    // to `gap.session_id`. Without this check, retranscribing a gap
                    // in session A and then picking a different session B from
                    // `history::History` before the request finishes would splice
                    // session A's newly persisted rows into session B's on-screen
                    // transcript. The DB write already happened either way, so
                    // skipping the local update here just means session A's panel
                    // picks it up from `SessionStore` the next time it's selected
                    // (selection-reload effect / poll loop), rather than seeing it
                    // update live right now.
                    if selected_session_id() == Some(gap.session_id) {
                        transcript_segments.with_mut(|segs| segs.extend(new_segments));
                        gaps.with_mut(|g| g.retain(|existing| existing.id != gap_id));
                    }
                }
                Err(err) => {
                    gap_retranscribe_state.with_mut(|m| {
                        m.insert(gap_id, GapRetranscribeState::Error(err));
                    });
                }
            }
        }
    };

    rsx! {
        div { class: "gap-row",
            p { class: "gap-label", "{label}" }
            if !gap_closed {
                p { class: "gap-hint", "(まだ接続が回復していません)" }
            } else if can_retranscribe {
                button {
                    class: "gap-retranscribe",
                    disabled: is_loading,
                    onclick: on_retranscribe,
                    if is_loading { "再文字起こし中..." } else { "この区間を再文字起こしする" }
                }
            } else {
                p { class: "gap-hint", "選択中のSTTプロバイダは再文字起こしに未対応です" }
            }
            if let Some(msg) = error_text {
                p { class: "gap-error", "{msg}" }
            }
        }
    }
}

#[component]
pub fn App() -> Element {
    setup_tray();

    let state = use_context::<Arc<AppState>>();
    let mut status = use_signal(Status::default);
    let mut busy = use_signal(|| false);
    let mut action_error = use_signal(|| None::<String>);
    let mut screen = use_signal(|| Screen::Main);
    // Task #69: which session summary generation (`on_generate_summary`) and
    // export (`on_export`) act on. Auto-follows the live/last recording (see the
    // polling future below, which sets this whenever `status().last_session_id`
    // changes — that covers both "recording just started" and "recording just
    // stopped", preserving the pre-#69 behavior of always targeting the most
    // recent session by default) but can be overridden by picking an older
    // session in `history::History`, which persists across app restarts (unlike
    // `AppState::last_summary`, in-memory only) since it's read from
    // `SessionStore::list_sessions`.
    let mut selected_session_id = use_signal(|| None::<SessionId>);

    // #33/#34's live transcript panel and #38's "load the last summary on screen
    // open" both key off which session is current, not off the 250ms tick itself —
    // tracked as a plain loop-local `Option<String>` (not a signal) since nothing
    // outside this future reads it.
    let mut transcript_segments = use_signal(Vec::<TranscriptSegment>::new);
    // `hint.rhai`'s live "what to talk about now" suggestion for whichever
    // session is currently *recording* — unlike `transcript_segments`, hints
    // are ephemeral (never persisted to `SessionStore`), so this only ever
    // has a value while `selected_session_id` is the actively-recording
    // session, and clears otherwise (see the poll loop below).
    let mut current_hint = use_signal(|| None::<HintState>);
    // Task #92: gaps (task #90) for whichever session `transcript_segments` above
    // is currently showing, and per-gap client-side state for the "この区間を
    // 再文字起こしする" button (#91) each renders — a gap missing from the map
    // reads as idle (see `gap_retranscription::GapRetranscribeState`'s doc
    // comment). Loaded/cleared alongside `transcript_segments` throughout this
    // component (poll loop and the selection-change effect below), so the two
    // never point at different sessions.
    let mut gaps = use_signal(Vec::<TranscriptionGap>::new);
    let mut gap_retranscribe_state = use_signal(HashMap::<i64, GapRetranscribeState>::new);
    let mut summary_text = use_signal(|| None::<String>);
    let mut summary_message = use_signal(|| None::<String>);
    let mut summary_busy = use_signal(|| false);
    // Task #71's manual "export to Markdown" button — its own message signal
    // (separate from `summary_message`) since export and summary generation are
    // independent actions that can each fail/succeed without clearing the other's
    // status line.
    let mut export_message = use_signal(|| None::<String>);

    // #53: auto-scroll the transcript panel to the bottom as new bubbles arrive,
    // but only when the user hasn't scrolled up to read older messages.
    let mut transcript_panel_mounted = use_signal(|| None::<Rc<MountedData>>);
    let visible_transcript_key = use_memo(move || {
        let segments = transcript_segments();
        let visible = transcript::visible_segments(&segments);
        (visible.len(), visible.last().map(|s| s.text.clone()))
    });
    use_effect(move || {
        // Subscribe to changes in what's actually rendered (count + last row's
        // text), not the raw poll tick — an unchanged view shouldn't re-check
        // scroll position every 250ms.
        let _ = visible_transcript_key();
        let Some(el) = transcript_panel_mounted() else { return };
        spawn(async move {
            let (Ok(offset), Ok(scroll_size), Ok(client_rect)) =
                (el.get_scroll_offset().await, el.get_scroll_size().await, el.get_client_rect().await)
            else {
                return;
            };
            let distance_from_bottom = scroll_size.height - offset.y - client_rect.size.height;
            if distance_from_bottom <= AUTO_SCROLL_NEAR_BOTTOM_PX {
                let _ = el.scroll(PixelsVector2D::new(0.0, scroll_size.height), ScrollBehavior::Smooth).await;
            }
        });
    });

    let poll_state = state.clone();
    use_future(move || {
        let state = poll_state.clone();
        async move {
            let mut last_session_id: Option<String>;
            let mut last_recording = false;
            loop {
                let new_status = actions::get_status(&state);

                // Auto-select the live/last recording as the "選択中セッション"
                // whenever recording starts or stops — edge-detected on
                // `recording`'s value, not on whether `last_session_id` differs
                // from this loop's own previous reading (Codex review finding):
                // comparing against `last_session_id` breaks once a manual
                // `history::History` pick has moved `selected_session_id` off of
                // what this loop last saw. E.g. session B is recording (this loop
                // already observed `last_session_id == B`), the user picks older
                // session A from history, then B stops — `status::current` still
                // reports `last_session_id == B` (same session, now finalized), so
                // the old value-diff check would see "unchanged" and never
                // re-select B, leaving the view stuck on A. Edge-detecting the
                // recording→not-recording (and not-recording→recording)
                // transition instead fires exactly at those two moments
                // regardless of what was manually selected in between, and also
                // covers the crash-recovery resume path (task #11) that doesn't
                // go through `on_start_recording`/`on_stop_recording` (which
                // handle the common case synchronously — see those handlers).
                let just_started = new_status.recording && !last_recording;
                let just_stopped = !new_status.recording && last_recording;
                last_recording = new_status.recording;
                last_session_id = new_status.last_session_id.clone();
                if just_started || just_stopped {
                    if let Some(parsed) = last_session_id.as_ref().and_then(|s| s.parse::<SessionId>().ok()) {
                        selected_session_id.set(Some(parsed));
                    }
                }

                // Task #92: the transcript panel (with gap markers) is now shown
                // for whichever session is *selected*, not just a live recording
                // (#33's original scope) — a `history::History` pick can point
                // `selected_session_id` at a session other than the one
                // `last_session_id`/`new_status.recording` describes, so this polls
                // off `selected_session_id` directly rather than `last_session_id`.
                // The selection-change effect below also loads both once
                // immediately on every selection change, so a picked-but-not-yet-
                // polled session doesn't show a stale flash of the previous one.
                if let Some(id) = selected_session_id() {
                    // During live recording, read from the broker-backed
                    // TranscriptBuffer for real-time updates. Otherwise, read
                    // from SessionStore for historical sessions.
                    let (segments, hint) = {
                        let current = state.current.lock().unwrap();
                        if current.as_ref().is_some_and(|a| a.session_id == id) {
                            let active = current.as_ref().unwrap();
                            (active.transcript_buffer.take(), active.hint_buffer.take())
                        } else {
                            (state.store.list_transcript_segments(id).unwrap_or_default(), None)
                        }
                    };
                    transcript_segments.set(segments);
                    current_hint.set(hint);
                    if let Ok(session_gaps) = state.store.gaps_for_session(id) {
                        gaps.set(session_gaps);
                    }
                } else {
                    current_hint.set(None);
                }

                status.set(new_status);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });

    // Task #69: whenever the "選択中セッション" changes (auto-follow above, or a
    // manual pick in `history::History`), reload that session's latest summary
    // and clear any stale success/error messages from a previous selection —
    // mirrors what the polling future used to do inline for `last_session_id`
    // before `selected_session_id` existed.
    //
    // Task #92: also reloads the transcript + gaps immediately here, rather than
    // waiting up to 250ms for the poll loop's next tick — otherwise switching to
    // a different session in `history::History` would briefly show whichever
    // session's bubbles/gap markers were on screen before, since the poll loop
    // only overwrites `transcript_segments`/`gaps` once its own tick fires. Also
    // clears `gap_retranscribe_state`: a gap id is only unique within its own
    // session's rows, so a stale loading/error entry from a previous selection
    // could otherwise mislabel an unrelated gap that happens to reuse the id.
    let selection_reload_state = state.clone();
    use_effect(move || {
        let session_id = selected_session_id();
        summary_message.set(None);
        export_message.set(None);
        let latest = session_id.and_then(|id| selection_reload_state.store.get_latest_summary(id).ok().flatten());
        summary_text.set(latest.map(|s| s.text));

        let segments = session_id.and_then(|id| selection_reload_state.store.list_transcript_segments(id).ok()).unwrap_or_default();
        transcript_segments.set(segments);
        let session_gaps = session_id.and_then(|id| selection_reload_state.store.gaps_for_session(id).ok()).unwrap_or_default();
        gaps.set(session_gaps);
        gap_retranscribe_state.set(HashMap::new());
        // Hints are ephemeral (see `current_hint`'s doc comment) — a picked
        // historical session never has one, and switching selection shouldn't
        // show the previous live session's stale hint even for a moment.
        current_hint.set(None);
    });

    // design.md's force-quit recovery (task #11): resume any session a previous
    // process instance left mid-flight, before any new recording can start. Runs
    // once in the background (`use_hook` so it isn't re-spawned on re-render) — it
    // must not block startup, and a failure here is logged, not fatal (the app
    // should still open so the user isn't stuck if e.g. the API is unreachable at
    // launch).
    use_hook(|| {
        let state = state.clone();
        tokio::spawn(async move {
            let sessions_root = state.config.sessions_root.clone();
            match app_service::recover_incomplete_sessions(&state.store, state.adapter.as_ref(), &sessions_root, 30_000, 48_000, 1, Duration::from_secs(2), 10).await {
                Ok(recovered) if !recovered.is_empty() => {
                    tracing::info!(count = recovered.len(), "recovered incomplete sessions from a previous run");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "startup session recovery failed"),
            }
        });
    });

    let confirm_state = state.clone();
    let on_confirm_consent = move |_| {
        action_error.set(None);
        status.set(actions::confirm_consent(&confirm_state));
    };

    let start_state = state.clone();
    let on_start_recording = move |_| {
        action_error.set(None);
        busy.set(true);
        match actions::start_recording(&start_state) {
            Ok(s) => {
                // ui_consumer/auto-summary/Rhai plugin dispatch are spawned by
                // `actions::start_recording` itself now, so both this button and
                // the control-server-backed CLI get identical wiring.
                if let Some(id) = s.last_session_id.as_deref().and_then(|id| id.parse::<SessionId>().ok()) {
                    selected_session_id.set(Some(id));
                }
                status.set(s);
            }
            Err(e) => action_error.set(Some(e)),
        }
        busy.set(false);
    };

    let stop_state = state.clone();
    let on_stop_recording = move |_| {
        let state = stop_state.clone();
        async move {
            action_error.set(None);
            busy.set(true);
            match actions::stop_recording(&state).await {
                Ok(s) => {
                    // Task #69: same reasoning as `on_start_recording` — re-assert
                    // the just-finished session as selected immediately, so it
                    // overrides whatever was picked from `history::History` while
                    // this session was still recording (matches this module's
                    // documented "a history pick overrides auto-follow until the
                    // next start/stop" behavior, without waiting on the next poll).
                    if let Some(id) = s.last_session_id.as_deref().and_then(|id| id.parse::<SessionId>().ok()) {
                        selected_session_id.set(Some(id));
                    }
                    status.set(s);
                }
                Err(e) => action_error.set(Some(e)),
            }
            busy.set(false);
        }
    };

    // #38: user-triggered summary. Works both mid-recording and after stop (any
    // time `last_session_id` is set) — unlike the transcript panel above, this
    // isn't gated on `recording`. Delegates to SummaryConsumer for the actual
    // summarization logic; this handler only manages UI state.
    let summary_state = state.clone();
    let on_generate_summary = move |_| {
        let state = summary_state.clone();
        async move {
            summary_message.set(None);
            summary_busy.set(true);

            let Some(session_id) = selected_session_id() else {
                summary_message.set(Some("記録されたセッションがありません".to_string()));
                summary_busy.set(false);
                return;
            };

            match state.summary_consumer.generate_summary_now(session_id).await {
                Ok(text) => {
                    summary_text.set(Some(text));
                }
                Err(e) => summary_message.set(Some(format!("要約に失敗しました: {e}"))),
            }
            // Also trigger via Rhai plugins
            state.rhai_engine.trigger_manual_summary(&state.broker, session_id);
            summary_busy.set(false);
        }
    };

    // Task #71: manual export of the latest session's finalized transcript (plus
    // its latest summary, if generated) to a local Markdown file. Synchronous —
    // `export::export_session` only does `SessionStore` reads and a local file
    // write, no network/await, so this mirrors `on_start_recording` rather than
    // the async `on_generate_summary`/`on_stop_recording` handlers above.
    let export_state = state.clone();
    let on_export = move |_| {
        export_message.set(None);

        let Some(session_id) = selected_session_id() else {
            export_message.set(Some("記録されたセッションがありません".to_string()));
            return;
        };

        match export::export_session(&export_state, session_id) {
            Ok(path) => export_message.set(Some(format!("エクスポートしました: {}", path.display()))),
            Err(e) => export_message.set(Some(format!("エクスポートに失敗しました: {e}"))),
        }
    };

    let current = status();
    let recording_active = current.recording;
    let consent_confirmed = current.consent_confirmed;
    let elapsed_str = format_elapsed(current.elapsed_ms);
    let self_rms_pct = (current.self_rms * 100.0).min(100.0);
    let self_peak_pct = (current.self_peak * 100.0).min(100.0);
    let remote_rms_pct = (current.remote_rms * 100.0).min(100.0);
    let remote_peak_pct = (current.remote_peak * 100.0).min(100.0);
    let uploaded_segments = current.uploaded_segments;
    let pending_segments = current.pending_segments;
    let last_error = current.last_error.clone();
    // #52: STT connection visibility, so an empty transcript panel doesn't read as
    // ambiguous "nobody has spoken yet" vs. "STT is broken" — see
    // `transcription_status::describe`'s doc comment.
    let transcription_status_line = transcription_status::describe(&current.transcription_status);
    // See `capture_health::describe`'s doc comment — a mic/system-audio track
    // stuck `Waiting`/`Failed` mid-session otherwise only shows as a silently
    // flatlined level meter.
    let capture_health_line = capture_health::describe(&current.capture_health);
    let last_session_line = current.last_session_id.clone().map(|session_id| match current.last_total_duration_ms {
        Some(duration_ms) => format!("Last session: {session_id} ({})", format_elapsed(duration_ms)),
        None => format!("Last session: {session_id}"),
    });
    // Task #69: gates the summary/export buttons on the *selected* session, not
    // merely "a session exists somewhere" — a session picked from
    // `history::History` still enables these even with no session recorded in
    // the current app run (`current.last_session_id` would be `None` then).
    let selected_session_id_value = selected_session_id();
    let has_session = selected_session_id_value.is_some();
    let action_error_text = action_error();
    let is_busy = busy();
    let is_summary_busy = summary_busy();

    // #51/#92: collapse Deepgram's Partial/Final row stream into one bubble per
    // in-flight utterance, then interleave `transcription_gaps` markers into
    // their correct chronological spot — see `transcript::timeline_items`'s doc
    // comment.
    let raw_segments = transcript_segments();
    let gap_list = gaps();
    let timeline = transcript::timeline_items(&raw_segments, &gap_list);
    // Task #92: whether to show a "この区間を再文字起こしする" button at all
    // (vs. an explanatory line) — the currently *selected* STT provider (not
    // necessarily the one that was connected when the gap itself was recorded;
    // see `retranscribe_gap`'s own doc comment) needs a `BatchSttProvider`
    // adapter (#91) for that to make sense.
    let selected_provider_kind = gap_retranscription::selected_provider_kind(state.credential_store.as_ref());
    let can_retranscribe = gap_retranscription::supports_batch_retranscription(selected_provider_kind);

    if screen() == Screen::Settings {
        return rsx! {
            settings::Settings { screen }
        };
    }

    if screen() == Screen::History {
        return rsx! {
            history::History { screen, selected_session_id }
        };
    }

    rsx! {
        style { "{STYLE}" }
        main { class: "container",
            div { class: "header-row",
                h1 { "1on1 Recorder" }
                div { class: "header-actions",
                    button { class: "gear", onclick: move |_| screen.set(Screen::History), title: "過去のセッション", "🕘" }
                    button { class: "gear", onclick: move |_| screen.set(Screen::Settings), title: "設定", "⚙" }
                }
            }

            if !recording_active {
                section { class: "panel",
                    h2 { "Ready to record" }
                    p { class: "hint", "Mic: default · Remote source: default" }

                    label { class: "consent",
                        input {
                            r#type: "checkbox",
                            checked: consent_confirmed,
                            onchange: on_confirm_consent,
                        }
                        "I consent to this meeting being recorded and uploaded."
                    }

                    button {
                        class: "primary",
                        disabled: is_busy || !consent_confirmed,
                        onclick: on_start_recording,
                        "Start recording"
                    }

                    if let Some(line) = last_session_line {
                        p { class: "hint", "{line}" }
                    }
                }
            } else {
                section { class: "panel recording",
                    p { class: "elapsed", span { class: "dot" } "{elapsed_str}" }

                    div { class: "meter-row",
                        span { class: "meter-label", "Self" }
                        div { class: "meter",
                            div { class: "meter-fill", style: "width: {self_rms_pct}%;" }
                            div { class: "meter-peak", style: "left: {self_peak_pct}%;" }
                        }
                    }
                    div { class: "meter-row",
                        span { class: "meter-label", "Remote" }
                        div { class: "meter",
                            div { class: "meter-fill", style: "width: {remote_rms_pct}%;" }
                            div { class: "meter-peak", style: "left: {remote_peak_pct}%;" }
                        }
                    }

                    if let Some(msg) = capture_health_line {
                        p { class: "warning", "⚠ {msg}" }
                    }

                    p { class: "stats", "Uploaded segments: {uploaded_segments} · Pending: {pending_segments}" }

                    button { class: "stop", disabled: is_busy, onclick: on_stop_recording, "Stop" }

                    if let Some(msg) = transcription_status_line {
                        p { class: "hint", "{msg}" }
                    }
                }
            }

            // Live conversation hint from `plugins/default/hint.rhai` — only
            // ever present while `selected_session_id` is the actively
            // recording session (see `current_hint`'s doc comment), so this
            // naturally disappears once recording stops or a different
            // (historical) session is selected.
            if let Some(hint) = current_hint() {
                section { class: "panel hint-panel",
                    p { class: "hint-panel-text", "💡 {hint.text}" }
                    p { class: "hint-panel-meta", "{hint.provider} · {hint.updated_at.elapsed().as_secs()}秒前" }
                }
            }

            // #33/#34/#92: transcript panel — bubbles grouped by track
            // (Self/Remote — this app's primary speaker axis; see
            // `transcript::track_label`'s doc comment) with an optional Deepgram
            // diarization index appended, interleaved with `transcription_gaps`
            // markers (#90) in their chronological spot. Shown whenever a session
            // is selected (not just while recording, unlike #33's original
            // scope): a gap is most often actionable right after the session
            // that had it stops (see `close_open_gap`), so restricting this to
            // `recording_active` would hide it right when it becomes useful.
            if has_session {
                section { class: "panel transcript-section",
                    div {
                        class: "transcript-panel",
                        onmounted: move |e| transcript_panel_mounted.set(Some(e.data())),
                        for item in timeline {
                            if let TimelineItem::Segment(seg) = item {
                                div {
                                    class: if seg.track == Some(TrackKind::SelfMic) { "bubble-row bubble-self" } else if seg.track == Some(TrackKind::RemoteAudio) { "bubble-row bubble-remote" } else { "bubble-row bubble-unknown" },
                                    div {
                                        class: if seg.is_final { "bubble bubble-final" } else { "bubble bubble-interim" },
                                        span { class: "bubble-label", "{transcript::speaker_label(seg.track, seg.speaker)}" }
                                        p { class: "bubble-text", "{seg.text}" }
                                    }
                                }
                            }
                            if let TimelineItem::Gap(gap) = item {
                                GapMarker {
                                    gap: *gap,
                                    can_retranscribe,
                                    provider_kind: selected_provider_kind,
                                    selected_session_id,
                                    transcript_segments,
                                    gaps,
                                    gap_retranscribe_state,
                                }
                            }
                        }
                    }
                }
            }

            // #38: user-triggered summary — enabled whenever a session exists
            // (mid-recording or after stop), not just while recording.
            section { class: "panel summary",
                // Task #69: makes explicit which session "要約を生成"/"エクスポート"
                // below will act on — important once a past session can be
                // selected from `history::History`, since it may not be the one
                // that's currently recording/just stopped.
                if let Some(session_id) = selected_session_id_value {
                    p { class: "hint", "対象セッション: {session_id}" }
                }
                button {
                    class: "primary",
                    disabled: !has_session || is_summary_busy,
                    onclick: on_generate_summary,
                    if is_summary_busy { "要約を生成中..." } else { "要約を生成" }
                }
                if let Some(msg) = summary_message() {
                    p { class: "hint", "{msg}" }
                }
                if let Some(text) = summary_text() {
                    div { class: "summary-text", "{text}" }
                }
                // #71: manual export of the latest session's transcript/summary to a
                // local Markdown file — enabled whenever a session exists, same as
                // the summary button above; a session with no summary yet still
                // exports its transcript (see `export::render_markdown`).
                button {
                    class: "primary",
                    disabled: !has_session,
                    onclick: on_export,
                    "エクスポート"
                }
                if let Some(msg) = export_message() {
                    p { class: "hint", "{msg}" }
                }
            }

            if let Some(err) = last_error {
                p { class: "error", "{err}" }
            }
            if let Some(err) = action_error_text {
                p { class: "error", "{err}" }
            }
        }
    }
}
