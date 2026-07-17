//! The direct-call replacements for the old Tauri `#[tauri::command]` handlers
//! (`commands.rs`) — the UI calls these functions in-process instead of going
//! through IPC, but the logic (and its result shape, `Status`/`Result<Status, String>`)
//! is unchanged.

use crate::app_state::AppState;
use crate::recording;
use crate::status::{self, Status};

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
        Ok(_session_id) => Ok(status::current(state)),
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
