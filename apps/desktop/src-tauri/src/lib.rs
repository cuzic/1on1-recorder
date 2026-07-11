//! Task #8: the Tauri 2 + Vue shell, ported from `spikes/spike-05-tauri-tray` and
//! wired to the real recording pipeline (`app-service`, `session-store`,
//! `upload-client`, `credential-store`) instead of a dummy sine-wave level meter.
//!
//! Rust is the single source of truth for capture/upload state (design.md §6.1):
//! the frontend only polls [`commands::get_status`] and calls the other commands —
//! it never tracks recording state of its own.

mod commands;
mod config;
mod level;
mod recording;
mod state;
mod status;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

use config::Config;
use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::try_init().ok();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![commands::get_status, commands::confirm_consent, commands::start_recording, commands::stop_recording])
        .setup(|app| {
            let app_data_dir = app.path().app_data_dir().expect("failed to resolve app data dir");
            std::fs::create_dir_all(&app_data_dir).ok();

            let config = Config::load(app_data_dir.clone());
            std::fs::create_dir_all(&config.sessions_root).ok();
            let sessions_root = config.sessions_root.clone();

            let store = Arc::new(session_store::SessionStore::open(&config.session_db_path).expect("failed to open session-store"));

            let credential_store = Arc::new(credential_store::FallbackCredentialStore::new(app_data_dir.join("credentials")).expect("failed to open credential-store"));
            let token_provider = Arc::new(credential_store::CredentialStoreTokenProvider::new(credential_store, config.credential_service.clone(), config.credential_account.clone()));
            let adapter = Arc::new(upload_client::HttpUploadClient::new(config.api_base_url.clone(), Duration::from_secs(30), token_provider));

            let store_for_recovery = store.clone();
            let adapter_for_recovery = adapter.clone();

            app.manage(AppState {
                store,
                adapter,
                config,
                consent_confirmed: Mutex::new(false),
                current: Mutex::new(None),
                last_error: Mutex::new(None),
                last_summary: Mutex::new(None),
            });

            // design.md's force-quit recovery (task #11): resume any session a
            // previous process instance left mid-flight, before any new recording
            // can start. Runs in the background — it must not block app startup,
            // and a failure here is logged, not fatal (the app should still open
            // so the user isn't stuck if e.g. the API is unreachable at launch).
            tauri::async_runtime::spawn(async move {
                match app_service::recover_incomplete_sessions(&store_for_recovery, adapter_for_recovery.as_ref(), &sessions_root, 30_000, 48_000, 1, Duration::from_secs(2), 10).await {
                    Ok(recovered) if !recovered.is_empty() => {
                        tracing::info!(count = recovered.len(), "recovered incomplete sessions from a previous run");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "startup session recovery failed"),
                }
            });

            // --- Tray icon, ported from spikes/spike-05-tauri-tray. Hide/show is
            // kept (per task #8's own scope note) but is not part of Phase 1A's
            // completion criteria.
            let show_item = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Hides instead of closing, so a recording in progress (and the
                // app state that owns it) survives the window being closed.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn show_main_window(app: &AppHandle) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    });
}
