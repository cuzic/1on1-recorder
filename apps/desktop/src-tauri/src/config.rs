use std::path::PathBuf;

/// Phase 1A records to a fixed API endpoint (design.md §21) — no login/settings UI
/// exists yet, so the base URL is read from an env var with a placeholder default
/// rather than hardcoded to a real service. `credential-store` (via
/// `CREDENTIAL_SERVICE`/`CREDENTIAL_ACCOUNT`) is where the actual bearer token is
/// expected to already be provisioned (out of band — Phase 1A's UI scope doesn't
/// include a "log in" screen either; see this crate's README).
pub struct Config {
    pub api_base_url: String,
    pub credential_service: String,
    pub credential_account: String,
    pub sessions_root: PathBuf,
    pub session_db_path: PathBuf,
    pub bitrate_bps: i32,
}

impl Config {
    pub fn load(app_data_dir: PathBuf) -> Self {
        let api_base_url = std::env::var("RECORDER_API_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
        Self {
            api_base_url,
            credential_service: "1on1-recorder".to_string(),
            credential_account: "api-token".to_string(),
            sessions_root: app_data_dir.join("sessions"),
            session_db_path: app_data_dir.join("session-store.sqlite3"),
            bitrate_bps: 32_000,
        }
    }
}
