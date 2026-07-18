mod actions;
mod app_settings;
mod app_state;
mod config;
mod level;
mod recording;
mod settings;
mod status;
mod transcript;
mod transcription_status;
mod ui;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::window::WindowBuilder;
use dioxus::desktop::{Config as DesktopConfig, WindowCloseBehaviour};
use dioxus::prelude::*;

use app_state::AppState;
use config::Config;

const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.png");

/// Ported from the old Tauri shell's `app.path().app_data_dir()` — Tauri namespaced
/// this automatically under the app's `identifier` (`tauri.conf.json`); `dirs`'s
/// platform data dir is not app-specific, so the app name is appended here to get
/// the same effective layout (`.../1on1-recorder/sessions`, `.../credentials`, etc).
fn app_data_dir() -> std::path::PathBuf {
    let base = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    let dir = base.join("1on1-recorder");
    migrate_from_old_tauri_app_data_dir(&base, &dir);
    dir
}

/// One-time migration for local installs made before the Dioxus migration (#30),
/// when this directory was namespaced under the old Tauri shell's
/// `tauri.conf.json` `identifier` (`com.example.onononerecorder`) instead of the
/// plain app name used above. Without this, any credentials/session history saved
/// by the old Tauri build would silently appear missing under the new path rather
/// than carrying over. Only renames when the new directory doesn't exist yet, so it
/// never clobbers a install that already has both (e.g. a stale old directory left
/// behind after a previous successful migration).
fn migrate_from_old_tauri_app_data_dir(base: &std::path::Path, new_dir: &std::path::Path) {
    let old_dir = base.join("com.example.onononerecorder");
    if new_dir.exists() || !old_dir.exists() {
        return;
    }
    match std::fs::rename(&old_dir, new_dir) {
        Ok(()) => tracing::info!(?old_dir, ?new_dir, "migrated app data directory from old Tauri shell's identifier-based path"),
        Err(err) => tracing::warn!(%err, ?old_dir, ?new_dir, "failed to migrate old Tauri app data directory"),
    }
}

fn main() {
    tracing_subscriber::fmt::try_init().ok();

    let app_data_dir = app_data_dir();
    std::fs::create_dir_all(&app_data_dir).ok();

    let config = Config::load(app_data_dir.clone());
    std::fs::create_dir_all(&config.sessions_root).ok();

    let store = Arc::new(session_store::SessionStore::open(&config.session_db_path).expect("failed to open session-store"));

    let credential_store = Arc::new(credential_store::FallbackCredentialStore::new(app_data_dir.join("credentials")).expect("failed to open credential-store"));
    let token_provider = Arc::new(credential_store::CredentialStoreTokenProvider::new(credential_store.clone(), config.credential_service.clone(), config.credential_account.clone()));
    let adapter = Arc::new(upload_client::HttpUploadClient::new(config.api_base_url.clone(), Duration::from_secs(30), token_provider));

    let app_settings = app_settings::AppSettings::load(&app_data_dir);

    let state = Arc::new(AppState {
        store,
        adapter,
        config,
        credential_store,
        app_data_dir: app_data_dir.clone(),
        app_settings: Mutex::new(app_settings),
        consent_confirmed: Mutex::new(false),
        current: Mutex::new(None),
        last_error: Mutex::new(None),
        last_summary: Mutex::new(None),
    });

    let window = WindowBuilder::new().with_title("1on1 Recorder").with_inner_size(LogicalSize::new(480.0, 640.0));

    let mut desktop_config = DesktopConfig::new().with_window(window).with_close_behaviour(WindowCloseBehaviour::WindowHides);
    if let Ok(icon) = dioxus::desktop::icon_from_memory::<dioxus::desktop::tao::window::Icon>(ICON_BYTES) {
        desktop_config = desktop_config.with_icon(icon);
    }

    LaunchBuilder::desktop().with_cfg(desktop_config).with_context(state).launch(app_entry);
}

fn app_entry() -> Element {
    ui::App()
}
