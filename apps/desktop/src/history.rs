//! Past-sessions list screen (task #69) — reachable from `ui::App` via a
//! `Signal<Screen>` swap, the same pattern `settings::Settings` uses for
//! `Screen::Settings`. Exists because `status::current`'s `last_session_id`
//! comes from `AppState::last_summary`, an in-memory `Mutex` that's empty again
//! after every restart — without this screen, a session recorded in an earlier
//! app run could never be summarized or exported again, even though its
//! transcript/summary rows are still sitting in `session-store`.
//!
//! Clicking a row sets it as the "選択中セッション" (`selected_session_id`,
//! owned by `ui::App`) and returns to the main screen, where the summary
//! generation/export buttons act on whatever session is currently selected —
//! see `ui.rs`'s `on_generate_summary`/`on_export`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use recorder_domain::{CaptureState, SessionId};

use crate::app_state::AppState;
use crate::settings::Screen;

const STYLE: &str = r#"
.history-container {
  margin: 0;
  padding: 5vh 2rem;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 1.25em;
  font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
}
.history-header {
  width: 100%;
  max-width: 420px;
  display: flex;
  align-items: center;
  gap: 0.8em;
}
.history-header h1 {
  margin: 0;
  font-size: 1.2em;
}
.history-list {
  width: 100%;
  max-width: 420px;
  display: flex;
  flex-direction: column;
  gap: 0.6em;
}
.history-row {
  display: flex;
  flex-direction: column;
  gap: 0.2em;
  padding: 0.7em 0.9em;
  border: 1px solid #444;
  border-radius: 8px;
  text-align: left;
  cursor: pointer;
  background: transparent;
}
.history-row:hover {
  background: #ffffff10;
}
.history-started {
  margin: 0;
  font-size: 0.95em;
}
.history-duration,
.history-state {
  margin: 0;
  font-size: 0.8em;
  opacity: 0.75;
}
.history-state.history-state-failed {
  color: #e74c3c;
  opacity: 1;
}
"#;

/// `YYYY-MM-DD HH:MM:SS` in the user's local timezone — `started_at`/`ended_at`
/// are stored as UTC (`schema.rs`'s `TEXT` columns hold RFC 3339), but a wall
/// clock time is more useful to scan in a list than UTC.
fn format_datetime(dt: DateTime<Utc>) -> String {
    dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()
}

/// `mm:ss` recording duration from `started_at`/`ended_at`, or a "still in
/// progress" label when `ended_at` isn't set yet — either because the session
/// is genuinely still recording, or because it crashed before reaching
/// `Finalized` (see `describe_capture_state` for how that's distinguished).
fn format_duration(started_at: DateTime<Utc>, ended_at: Option<DateTime<Utc>>) -> String {
    match ended_at {
        Some(ended_at) => {
            let total_seconds = (ended_at - started_at).num_seconds().max(0) as u64;
            format!("{:02}:{:02}", total_seconds / 60, total_seconds % 60)
        }
        None => "recording中".to_string(),
    }
}

/// Human-readable `CaptureState` label. `Failed { recoverable: true, .. }` is
/// called out separately from a plain `failed` — `SessionStore::reconcile_on_startup`
/// marks a session left mid-flight by a crashed process this way, and that's a
/// materially different situation from a session that failed for a normal
/// reason (e.g. a device permission error) while the app was still running.
fn describe_capture_state(state: &CaptureState) -> &'static str {
    match state {
        CaptureState::Idle => "idle",
        CaptureState::Preparing => "準備中",
        CaptureState::Recording => "recording中",
        CaptureState::Stopping => "停止処理中",
        CaptureState::Finalizing => "確定処理中",
        CaptureState::Finalized => "完了",
        CaptureState::Failed { recoverable: true, .. } => "failed (recoverable)",
        CaptureState::Failed { recoverable: false, .. } => "failed",
    }
}

fn is_failed_state(state: &CaptureState) -> bool {
    matches!(state, CaptureState::Failed { .. })
}

#[component]
pub fn History(mut screen: Signal<Screen>, mut selected_session_id: Signal<Option<SessionId>>) -> Element {
    let state = use_context::<Arc<AppState>>();
    let sessions = use_signal({
        let state = state.clone();
        // Loaded once on mount rather than polled — unlike the main screen's
        // live status, past sessions don't change while this screen is open
        // (there's no way to start a new recording from here).
        move || state.store.list_sessions().unwrap_or_default()
    });

    rsx! {
        style { "{STYLE}" }
        main { class: "history-container",
            div { class: "history-header",
                button { onclick: move |_| screen.set(Screen::Main), "← 戻る" }
                h1 { "過去のセッション" }
            }
            if sessions().is_empty() {
                p { class: "hint", "記録されたセッションがありません" }
            } else {
                div { class: "history-list",
                    for item in sessions() {
                        button {
                            class: "history-row",
                            key: "{item.session_id}",
                            onclick: move |_| {
                                selected_session_id.set(Some(item.session_id));
                                screen.set(Screen::Main);
                            },
                            p { class: "history-started", "{format_datetime(item.started_at)}" }
                            p { class: "history-duration", "録音時間: {format_duration(item.started_at, item.ended_at)}" }
                            p {
                                class: if is_failed_state(&item.capture_state) { "history-state history-state-failed" } else { "history-state" },
                                "状態: {describe_capture_state(&item.capture_state)}"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_started_at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-18T14:30:22Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn format_duration_renders_mmss_when_ended_at_is_set() {
        let started_at = sample_started_at();
        let ended_at = started_at + chrono::Duration::seconds(125);
        assert_eq!(format_duration(started_at, Some(ended_at)), "02:05");
    }

    #[test]
    fn format_duration_reports_still_recording_when_ended_at_is_unset() {
        assert_eq!(format_duration(sample_started_at(), None), "recording中");
    }

    #[test]
    fn describe_capture_state_distinguishes_recoverable_from_plain_failed() {
        let recoverable = CaptureState::Failed { recoverable: true, reason: "crash".to_string() };
        let not_recoverable = CaptureState::Failed { recoverable: false, reason: "denied".to_string() };
        assert_eq!(describe_capture_state(&recoverable), "failed (recoverable)");
        assert_eq!(describe_capture_state(&not_recoverable), "failed");
        assert_ne!(describe_capture_state(&recoverable), describe_capture_state(&not_recoverable));
    }

    #[test]
    fn describe_capture_state_covers_every_non_failed_variant() {
        assert_eq!(describe_capture_state(&CaptureState::Idle), "idle");
        assert_eq!(describe_capture_state(&CaptureState::Preparing), "準備中");
        assert_eq!(describe_capture_state(&CaptureState::Recording), "recording中");
        assert_eq!(describe_capture_state(&CaptureState::Stopping), "停止処理中");
        assert_eq!(describe_capture_state(&CaptureState::Finalizing), "確定処理中");
        assert_eq!(describe_capture_state(&CaptureState::Finalized), "完了");
    }

    #[test]
    fn is_failed_state_is_true_only_for_failed_variants() {
        assert!(is_failed_state(&CaptureState::Failed { recoverable: true, reason: String::new() }));
        assert!(is_failed_state(&CaptureState::Failed { recoverable: false, reason: String::new() }));
        assert!(!is_failed_state(&CaptureState::Recording));
        assert!(!is_failed_state(&CaptureState::Finalized));
    }
}
