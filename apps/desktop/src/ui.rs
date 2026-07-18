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

    // #33/#34's live transcript panel and #38's "load the last summary on screen
    // open" both key off which session is current, not off the 250ms tick itself —
    // tracked as a plain loop-local `Option<String>` (not a signal) since nothing
    // outside this future reads it.
    let mut transcript_segments = use_signal(Vec::<TranscriptSegment>::new);
    let mut summary_text = use_signal(|| None::<String>);
    let mut summary_message = use_signal(|| None::<String>);
    let mut summary_busy = use_signal(|| false);

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
            let mut last_session_id: Option<String> = None;
            loop {
                let new_status = actions::get_status(&state);

                if new_status.last_session_id != last_session_id {
                    last_session_id = new_status.last_session_id.clone();
                    summary_message.set(None);
                    let latest = last_session_id
                        .as_ref()
                        .and_then(|s| s.parse::<SessionId>().ok())
                        .and_then(|id| state.store.get_latest_summary(id).ok().flatten());
                    summary_text.set(latest.map(|s| s.text));
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
            Ok(s) => status.set(s),
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
                Ok(s) => status.set(s),
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

            let Some(session_id) = status().last_session_id.as_deref().and_then(|s| s.parse::<SessionId>().ok()) else {
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

            let stored_credential = state.credential_store.load(summarize::CREDENTIAL_SERVICE, provider.api_key_account());
            if stored_credential.is_err() {
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

            // `ClaudeVertex`/`GeminiVertex` authenticate via a GCP project/location/
            // service-account bundle (`summarize::VertexCredentials`), not a bare API
            // key — `build_vertex_client` wires up its own resolvers instead of the
            // `credential_store_auth_resolver` + plain `Client::builder()` path below.
            let client = if provider.is_vertex() {
                // `stored_credential.is_err()` already returned above, so this is `Ok`.
                let raw = stored_credential.unwrap_or_default();
                match serde_json::from_str::<summarize::VertexCredentials>(&raw) {
                    Ok(credentials) => summarize::build_vertex_client(credentials),
                    Err(e) => {
                        summary_message.set(Some(format!("認証情報の読み込みに失敗しました: {e}")));
                        summary_busy.set(false);
                        return;
                    }
                }
            } else {
                let resolver = summarize::credential_store_auth_resolver(state.credential_store.clone(), provider.api_key_account());
                genai::Client::builder().with_auth_resolver(resolver).build()
            };
            let options = summarize::SummarizeOptions::new(model.clone());

            match summarize::summarize(&client, &turns, &options).await {
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
    let has_session = current.last_session_id.is_some();
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

    rsx! {
        style { "{STYLE}" }
        main { class: "container",
            div { class: "header-row",
                h1 { "1on1 Recorder" }
                button { class: "gear", onclick: move |_| screen.set(Screen::Settings), "⚙" }
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
