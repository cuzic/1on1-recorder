use tauri::State;

use crate::recording;
use crate::state::AppState;
use crate::status::{self, Status};

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> Status {
    status::current(&state)
}

/// design.md §14.1's "録音同意確認" — a recording session cannot start until this
/// has been called at least once in the current app run (`start_recording`
/// enforces that; this command only records the confirmation itself).
#[tauri::command]
pub fn confirm_consent(state: State<'_, AppState>) -> Status {
    *state.consent_confirmed.lock().unwrap() = true;
    status::current(&state)
}

#[tauri::command]
pub fn start_recording(state: State<'_, AppState>) -> Result<Status, String> {
    if !*state.consent_confirmed.lock().unwrap() {
        return Err("recording consent has not been confirmed yet".to_string());
    }
    if state.current.lock().unwrap().is_some() {
        return Err("a recording is already in progress".to_string());
    }

    *state.last_error.lock().unwrap() = None;
    match recording::start(&state) {
        Ok(_session_id) => Ok(status::current(&state)),
        Err(e) => {
            *state.last_error.lock().unwrap() = Some(e.clone());
            Err(e)
        }
    }
}

#[tauri::command]
pub async fn stop_recording(state: State<'_, AppState>) -> Result<Status, String> {
    match recording::stop(&state).await {
        Ok(summary) => {
            *state.last_summary.lock().unwrap() = Some(summary);
            Ok(status::current(&state))
        }
        Err(e) => {
            *state.last_error.lock().unwrap() = Some(e.clone());
            Err(e)
        }
    }
}
