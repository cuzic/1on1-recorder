use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use credential_store::CredentialStore;
use dioxus::desktop::trayicon::{self, DioxusTrayIcon, DioxusTrayMenu};
use dioxus::desktop::{use_tray_icon_event_handler, use_tray_menu_event_handler, use_window};
use dioxus::html::geometry::PixelsVector2D;
use dioxus::prelude::*;
use recorder_domain::{SessionId, TrackKind};
use session_store::{Summary, TranscriptSegment};

use crate::actions;
use crate::app_state::AppState;
use crate::export;
use crate::history;
use crate::settings::{self, Screen, SummaryProvider};
use crate::status::Status;
use crate::transcript;
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

                // Only the recording_active view renders the panel (#33's scope), so
                // there's no need to poll `list_transcript_segments` once recording
                // stops.
                if new_status.recording {
                    if let Some(id) = last_session_id.as_ref().and_then(|s| s.parse::<SessionId>().ok()) {
                        if let Ok(segments) = state.store.list_transcript_segments(id) {
                            transcript_segments.set(segments);
                        }
                    }
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
    let summary_reload_state = state.clone();
    use_effect(move || {
        let session_id = selected_session_id();
        summary_message.set(None);
        export_message.set(None);
        let latest = session_id.and_then(|id| summary_reload_state.store.get_latest_summary(id).ok().flatten());
        summary_text.set(latest.map(|s| s.text));
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
                // Task #69: sync `selected_session_id` immediately rather than
                // waiting for the next 250ms poll tick — otherwise a stale
                // selection (e.g. picked from `history::History` before this
                // click) could still be the target of a summary/export click
                // during that window (Codex review finding).
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
    // isn't gated on `recording`.
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

            let provider = state
                .credential_store
                .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT)
                .ok()
                .map(|key| SummaryProvider::from_key(&key))
                .unwrap_or(SummaryProvider::Claude);
            let model = state
                .credential_store
                .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_MODEL_ACCOUNT)
                .unwrap_or_else(|_| provider.default_model().to_string());

            // CLI-based providers (#59) authenticate as the `claude`/`codex` CLI's
            // own OAuth/subscription login, not a stored API key — this check only
            // applies to the genai/Vertex paths below (`api_key_account()` is
            // `None` for them, so `stored_credential` stays `None` and the check
            // is skipped). A future API-key-free genai provider (e.g. Ollama)
            // would also have `api_key_account() == None` and be skipped here in
            // the same way — see the `None` arm of the `summarizer` match below.
            let stored_credential = provider.api_key_account().map(|account| state.credential_store.load(summarize::CREDENTIAL_SERVICE, account));
            if matches!(stored_credential, Some(Err(_))) {
                let message = if provider.is_vertex() { "設定画面でGoogle Vertex AIの認証情報を設定してください" } else { "設定画面でAPIキーを設定してください" };
                summary_message.set(Some(message.to_string()));
                summary_busy.set(false);
                return;
            }

            let segments = match state.store.list_transcript_segments(session_id) {
                Ok(segments) => segments,
                Err(e) => {
                    summary_message.set(Some(format!("文字起こしの取得に失敗しました: {e}")));
                    summary_busy.set(false);
                    return;
                }
            };
            let turns = transcript::to_turns(&segments);
            if turns.is_empty() {
                summary_message.set(Some("要約対象の文字起こしがありません".to_string()));
                summary_busy.set(false);
                return;
            }
            let options = summarize::SummarizeOptions::new(model.clone());

            // Three independent ways to build a `Summarizer`: a `claude`/`codex` CLI
            // subprocess (#59, no API key needed), Google Vertex AI (a GCP project/
            // location/service-account bundle), and plain `genai` (with or without
            // an API key) — see `SummaryProvider::uses_cli`/`is_vertex`. Once built,
            // all three are invoked identically below.
            let summarizer: Result<Box<dyn summarize::Summarizer>, String> = if let Some(backend) = provider.cli_backend() {
                Ok(Box::new(summarize::CliSummarizer(backend)))
            } else if provider.is_vertex() {
                // `matches!(stored_credential, Some(Err(_)))` already returned above,
                // so this is `Some(Ok(_))`.
                let raw = stored_credential.and_then(Result::ok).unwrap_or_default();
                match serde_json::from_str::<summarize::VertexCredentials>(&raw) {
                    Ok(credentials) => Ok(Box::new(summarize::GenaiSummarizer(summarize::build_vertex_client(credentials)))),
                    Err(e) => Err(format!("認証情報の読み込みに失敗しました: {e}")),
                }
            } else if let Some(account) = provider.api_key_account() {
                // Today every non-CLI, non-Vertex provider has an `api_key_account()`,
                // so this is the only reachable arm here.
                let resolver = summarize::credential_store_auth_resolver(state.credential_store.clone(), account);
                let client = genai::Client::builder().with_auth_resolver(resolver).build();
                Ok(Box::new(summarize::GenaiSummarizer(client)))
            } else {
                // No stored provider currently has `api_key_account() == None`
                // outside the CLI/Vertex cases handled above, so this arm is
                // unreachable today. It exists for a future API-key-free genai
                // provider (e.g. Ollama), which would build a plain client with
                // no auth resolver.
                let client = genai::Client::builder().build();
                Ok(Box::new(summarize::GenaiSummarizer(client)))
            };

            let result: Result<String, String> = match summarizer {
                Ok(summarizer) => summarizer.summarize(&turns, &options).await.map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };

            match result {
                Ok(text) => {
                    let summary = Summary {
                        session_id,
                        text: text.clone(),
                        provider_model: format!("{}/{}", provider.key(), model),
                        generated_at: chrono::Utc::now(),
                    };
                    if let Err(e) = state.store.insert_summary(&summary) {
                        tracing::warn!(error = %e, "failed to persist summary");
                    }
                    summary_text.set(Some(text));
                }
                Err(e) => summary_message.set(Some(format!("要約に失敗しました: {e}"))),
            }
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

    // #51: collapse Deepgram's Partial/Final row stream into one bubble per
    // in-flight utterance — see `transcript::visible_segments`'s doc comment.
    let raw_segments = transcript_segments();
    let visible_segments = transcript::visible_segments(&raw_segments);

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

                    p { class: "stats", "Uploaded segments: {uploaded_segments} · Pending: {pending_segments}" }

                    button { class: "stop", disabled: is_busy, onclick: on_stop_recording, "Stop" }

                    if let Some(msg) = transcription_status_line {
                        p { class: "hint", "{msg}" }
                    }

                    // #33/#34: live transcript, grouped into chat-style bubbles by
                    // track (Self/Remote — this app's primary speaker axis; see
                    // `transcript::track_label`'s doc comment) with an optional
                    // Deepgram diarization index appended.
                    div {
                        class: "transcript-panel",
                        onmounted: move |e| transcript_panel_mounted.set(Some(e.data())),
                        for seg in visible_segments {
                            div {
                                class: if seg.track == Some(TrackKind::SelfMic) { "bubble-row bubble-self" } else if seg.track == Some(TrackKind::RemoteAudio) { "bubble-row bubble-remote" } else { "bubble-row bubble-unknown" },
                                div {
                                    class: if seg.is_final { "bubble bubble-final" } else { "bubble bubble-interim" },
                                    span { class: "bubble-label", "{transcript::speaker_label(seg.track, seg.speaker)}" }
                                    p { class: "bubble-text", "{seg.text}" }
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
