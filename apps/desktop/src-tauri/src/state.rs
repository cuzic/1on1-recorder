use std::sync::Mutex;
use std::sync::Arc;
use std::time::Instant;

use recorder_domain::{SessionId, SessionManifest, SessionSummary};
use session_store::SessionStore;
use upload_client::HttpUploadClient;

use crate::config::Config;

pub struct AppState {
    pub store: Arc<SessionStore>,
    pub adapter: Arc<HttpUploadClient>,
    pub config: Config,
    pub consent_confirmed: Mutex<bool>,
    pub current: Mutex<Option<ActiveRecording>>,
    pub last_error: Mutex<Option<String>>,
    pub last_summary: Mutex<Option<SessionSummary>>,
}

pub struct ActiveRecording {
    pub session_id: SessionId,
    /// Only read back out on the non-Windows dev-mode `stop` path (see
    /// `recording.rs`) — the real Windows path already moved its own clone into
    /// the spawned capture task, so this copy goes unread there.
    #[cfg_attr(windows, allow(dead_code))]
    pub manifest: SessionManifest,
    pub started_at: Instant,
    /// Updated live by `app_service::windows_frame_collector` as real capture
    /// happens — only exists on Windows. Other platforms report
    /// `level::dev_placeholder_level(elapsed)` instead (see `recording.rs`), since
    /// there is no real capture to compute a level from.
    #[cfg(windows)]
    pub level: Arc<Mutex<app_service::LevelSnapshot>>,
    #[cfg(windows)]
    pub shutdown_tx: crossbeam_channel::Sender<()>,
    #[cfg(windows)]
    pub join_handle: tauri::async_runtime::JoinHandle<Result<SessionSummary, app_service::AppServiceError>>,
}
