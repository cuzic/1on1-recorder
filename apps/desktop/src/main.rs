mod actions;
mod app_settings;
mod app_state;
mod config;
mod export;
mod gap_retranscription;
mod history;
mod level;
mod recording;
mod settings;
mod status;
mod summary_consumer;
mod summary_template;
mod transcript;
mod transcription_status;
mod ui;
mod ui_consumer;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use credential_store::CredentialStore;
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

/// Logs to `<app_data_dir>/app.log` in addition to stdout — a GUI-subsystem exe
/// launched by double-click has no console to write stdout to at all, and a
/// panic before any window appears (the exact class of bug this is here to
/// catch — this app has so far only ever been cross-compile-checked, never
/// actually run on real Windows hardware) would otherwise leave no trace.
/// Falls back to stdout-only if the log file can't be opened (e.g. read-only
/// app data dir) rather than failing startup over a logging problem.
fn init_logging(app_data_dir: &std::path::Path) {
    let log_path = app_data_dir.join("app.log");
    match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(file) => {
            tracing_subscriber::fmt().with_writer(std::sync::Mutex::new(file)).with_ansi(false).init();
        }
        Err(err) => {
            tracing_subscriber::fmt::try_init().ok();
            tracing::warn!(%err, ?log_path, "failed to open log file, logging to stdout only");
        }
    }

    std::panic::set_hook(Box::new(|info| {
        tracing::error!(%info, "panic");
    }));
}

fn main() {
    let app_data_dir = app_data_dir();
    std::fs::create_dir_all(&app_data_dir).ok();
    init_logging(&app_data_dir);

    tracing::info!("1on1 Recorder starting up (pid {})", std::process::id());

    let config = Config::load(app_data_dir.clone());
    std::fs::create_dir_all(&config.sessions_root).ok();

    let store = Arc::new(session_store::SessionStore::open(&config.session_db_path).expect("failed to open session-store"));

    let credential_store = Arc::new(credential_store::FallbackCredentialStore::new(app_data_dir.join("credentials")).expect("failed to open credential-store"));
    let token_provider = Arc::new(credential_store::CredentialStoreTokenProvider::new(credential_store.clone(), config.credential_service.clone(), config.credential_account.clone()));
    let adapter = Arc::new(upload_client::HttpUploadClient::new(config.api_base_url.clone(), Duration::from_secs(30), token_provider));

    let app_settings = app_settings::AppSettings::load(&app_data_dir);
    let app_settings = std::sync::Arc::new(std::sync::Mutex::new(app_settings));

    let broker = local_broker::LocalBroker::new();
    let summary_consumer = summary_consumer::SummaryConsumer::new(
        broker.clone(),
        store.clone(),
        credential_store.clone(),
        app_settings.clone(),
    );

    // Settings provider for Rhai plugins
    let settings_provider = Arc::new(AppSettingsProvider {
        app_settings: app_settings.clone(),
        credential_store: credential_store.clone(),
    });

    let mut rhai_engine = rhai_engine::RhaiEngine::new(
        broker.clone(),
        store.clone(),
        credential_store.clone(),
        settings_provider,
    );

    // Load plugins from the app data directory
    let plugin_dir = app_data_dir.join("plugins");
    if let Err(err) = rhai_engine.load_plugins(&plugin_dir) {
        tracing::warn!(%err, "failed to load Rhai plugins");
    }

    let state = Arc::new(AppState {
        store,
        adapter,
        config,
        broker,
        summary_consumer,
        rhai_engine,
        credential_store,
        app_data_dir: app_data_dir.clone(),
        app_settings,
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

/// Implements rhai_engine::SettingsProvider for the desktop app.
struct AppSettingsProvider {
    app_settings: Arc<Mutex<app_settings::AppSettings>>,
    credential_store: Arc<credential_store::FallbackCredentialStore>,
}

impl rhai_engine::SettingsProvider for AppSettingsProvider {
    fn get(&self, key: &str) -> Option<String> {
        match key {
            "ollama_base_url" => self.app_settings.lock().unwrap().ollama_base_url.clone(),
            "summary_template" => self.app_settings.lock().unwrap().summary_template.clone(),
            "summary_provider_key" => self.credential_store
                .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT)
                .ok(),
            _ => None,
        }
    }

    fn selected_model(&self) -> String {
        let provider_key = self.credential_store
            .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT)
            .unwrap_or_else(|_| "claude".to_string());
        self.credential_store
            .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_MODEL_ACCOUNT)
            .unwrap_or_else(|_| {
                crate::settings::SummaryProvider::from_key(&provider_key)
                    .default_model()
                    .to_string()
            })
    }

    fn session_metadata(&self, _session_id: recorder_domain::SessionId) -> rhai::Map {
        rhai::Map::new()
    }
}
