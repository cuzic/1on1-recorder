//! The direct-call replacements for the old Tauri `#[tauri::command]` handlers
//! (`commands.rs`) — the UI calls these functions in-process instead of going
//! through IPC, but the logic (and its result shape, `Status`/`Result<Status, String>`)
//! is unchanged.

use crate::app_state::AppState;
use crate::hint_consumer;
use crate::recording;
use crate::status::{self, Status};
use crate::ui_consumer;

pub fn get_status(state: &AppState) -> Status {
    status::current(state)
}

/// design.md §14.1's "録音同意確認" — a recording session cannot start until this
/// has been called at least once in the current app run (`start_recording`
/// enforces that; this function only records the confirmation itself).
pub fn confirm_consent(state: &AppState) -> Status {
    *state.consent_confirmed.lock().unwrap() = true;
    status::current(state)
}

pub fn start_recording(state: &AppState) -> Result<Status, String> {
    if !*state.consent_confirmed.lock().unwrap() {
        return Err("recording consent has not been confirmed yet".to_string());
    }
    if state.current.lock().unwrap().is_some() {
        return Err("a recording is already in progress".to_string());
    }

    *state.last_error.lock().unwrap() = None;
    match recording::start(state) {
        Ok(session_id) => {
            // Both callers (the GUI's "録音開始" button and the control-server-
            // backed CLI, `control_server.rs`) go through this one function, so
            // neither can start a session with live transcript updates, an
            // auto-summary, live hints, or Rhai plugin dispatch wired up while
            // the others are missing — see `ui_consumer`/`hint_consumer`/
            // `SummaryConsumer`/`RhaiEngine`'s own doc comments for what each
            // of these does. None of the returned `JoinHandle`s need to be
            // held onto: every one of these tasks ends itself on the
            // `session.{id}.stopped` broker signal `recording::stop`
            // publishes (see that module).
            if let Some(active) = state.current.lock().unwrap().as_ref() {
                ui_consumer::spawn_ui_consumer(state.broker.clone(), session_id, state.store.clone(), active.transcript_buffer.clone());

                // Opt-in (see `AppSettings::hint_enabled`'s doc comment) —
                // an unconfigured/disabled install shouldn't subscribe to
                // anything hint-related at all, let alone fail a RAG query
                // on every debounce interval of every recording.
                let hint_settings = {
                    let settings = state.app_settings.lock().unwrap();
                    (settings.hint_enabled.unwrap_or(false), settings.hint_debounce_seconds.unwrap_or(15))
                };
                if let (true, debounce_seconds) = hint_settings {
                    hint_consumer::spawn_hint_consumer(state.broker.clone(), session_id, active.hint_buffer.clone());
                    state.rhai_engine.spawn_hint_debounce_driver(&state.broker, session_id, std::time::Duration::from_secs(debounce_seconds as u64));
                }
            }
            state.summary_consumer.spawn_auto_summary(session_id);
            state.rhai_engine.spawn_session(&state.broker, session_id);
            Ok(status::current(state))
        }
        Err(e) => {
            *state.last_error.lock().unwrap() = Some(e.clone());
            Err(e)
        }
    }
}

pub async fn stop_recording(state: &AppState) -> Result<Status, String> {
    match recording::stop(state).await {
        Ok(summary) => {
            *state.last_summary.lock().unwrap() = Some(summary);
            Ok(status::current(state))
        }
        Err(e) => {
            *state.last_error.lock().unwrap() = Some(e.clone());
            Err(e)
        }
    }
}
