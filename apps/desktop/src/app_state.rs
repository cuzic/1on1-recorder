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
    /// Backs the settings screen (Deepgram / summary provider API keys and the
    /// selected-provider/model strings) — same instance `main.rs` already builds
    /// for the upload bearer token, so there's one credential-store handle per app
    /// run rather than one per consumer.
    pub credential_store: Arc<credential_store::FallbackCredentialStore>,
    pub consent_confirmed: Mutex<bool>,
    pub current: Mutex<Option<ActiveRecording>>,
    pub last_error: Mutex<Option<String>>,
    pub last_summary: Mutex<Option<SessionSummary>>,
}

pub struct ActiveRecording {
    pub session_id: SessionId,
    /// Only read back out on the dev-mode `stop` path (see `recording.rs`) — the
    /// real Windows/macOS paths already moved their own clone into the spawned
    /// capture task, so this copy goes unread there.
    #[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
    pub manifest: SessionManifest,
    pub started_at: Instant,
    /// Updated live by `app_service::windows_frame_collector`/
    /// `app_service::macos_frame_collector` as real capture happens — only exists
    /// on Windows/macOS. Other platforms report `level::dev_placeholder_level(elapsed)`
    /// instead (see `recording.rs`), since there is no real capture to compute a
    /// level from. The two platforms' `LevelSnapshot` types are structurally
    /// identical but distinct (see `app-service`'s `lib.rs` for why macOS's isn't
    /// re-exported at the crate root the way Windows's is).
    #[cfg(windows)]
    pub level: Arc<Mutex<app_service::LevelSnapshot>>,
    #[cfg(target_os = "macos")]
    pub level: Arc<Mutex<app_service::macos_frame_collector::LevelSnapshot>>,
    /// Task #52: `app_service::live_transcription`'s per-track Deepgram connection
    /// status side channel — only exists on Windows, the only platform that wires
    /// up live transcription at all (see `app_service::live_transcription`'s doc
    /// comment on macOS scope). `status::current` reports
    /// `transcription_status::TranscriptionStatus::unavailable()` on every other
    /// platform instead.
    #[cfg(windows)]
    pub transcription_status: Arc<Mutex<app_service::TranscriptionStatus>>,
    #[cfg(any(windows, target_os = "macos"))]
    pub shutdown_tx: crossbeam_channel::Sender<()>,
    #[cfg(any(windows, target_os = "macos"))]
    pub join_handle: tokio::task::JoinHandle<Result<SessionSummary, app_service::AppServiceError>>,
}
