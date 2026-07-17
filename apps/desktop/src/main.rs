mod actions;
mod app_state;
mod config;
mod level;
mod recording;
mod settings;
mod status;
mod transcript;
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
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("1on1-recorder")
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

    let state = Arc::new(AppState {
        store,
        adapter,
        config,
        credential_store,
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
