//! Orchestrates `capture -> align -> segment -> encode -> commit -> upload ->
//! finalize` for the meeting recorder. Stage 1 (task #7): an OS-independent pipeline
//! driven by `pseudo_source`, proving the wiring between `audio-timeline`,
//! `segment-store`, `session-store`, and `upload-client` without any real capture
//! backend. Stage 2 (task #10) adds a Windows supervisor and the frame-conversion
//! layer that feeds it into stage 1's pipeline. Stage 3 (task #11) adds a standing
//! upload worker and `CaptureState`/`UploadState` lifecycle management
//! (`upload_worker`, `session_lifecycle`), including force-quit crash recovery.

mod error;
mod normalize;
mod pipeline;
#[cfg(feature = "live-transcription")]
mod resample;
mod segmenter;
mod session_lifecycle;
mod stt_provider_kind;
mod timeline_adapter;
mod upload_worker;

pub mod pseudo_source;

#[cfg(feature = "windows-supervisor")]
pub mod live_transcription;
#[cfg(feature = "windows-supervisor")]
pub mod windows_frame_collector;
#[cfg(feature = "windows-supervisor")]
pub mod windows_session;
#[cfg(feature = "windows-supervisor")]
pub mod windows_supervisor;

// Only re-exported when `macos-supervisor` is off: both this and
// `macos_frame_collector::LevelSnapshot` are structurally identical but distinct
// types, so re-exporting both under the same top-level name would collide if a
// caller ever enabled both features together (e.g. `cargo build --all-features`,
// which nothing stops even though the two are only ever meaningful on their own
// respective OS). Callers building with `macos-supervisor` reach the macOS type via
// `app_service::macos_frame_collector::LevelSnapshot` instead of a root re-export.
#[cfg(all(feature = "windows-supervisor", not(feature = "macos-supervisor")))]
pub use windows_frame_collector::LevelSnapshot;
#[cfg(feature = "windows-supervisor")]
pub use windows_session::run_windows_capture_session;
// `live_transcription` (unlike `LevelSnapshot`) has no macOS-side equivalent type
// to collide with (see that module's doc comment), so this re-export doesn't need
// the same `not(feature = "macos-supervisor")` guard.
#[cfg(feature = "windows-supervisor")]
pub use live_transcription::{TrackTranscriptionStatus, TranscriptionStatus};

#[cfg(feature = "macos-supervisor")]
pub mod macos_frame_collector;
#[cfg(feature = "macos-supervisor")]
pub mod macos_session;
#[cfg(feature = "macos-supervisor")]
pub mod macos_supervisor;

#[cfg(feature = "macos-supervisor")]
pub use macos_session::run_macos_capture_session;

pub use error::AppServiceError;
pub use normalize::normalize_to_mono;
pub use pipeline::run_pipeline;
pub use segmenter::{segment_pcm, PendingSegment};
pub use session_lifecycle::{begin_session, end_session, recover_incomplete_sessions};
// Unconditional (no `windows-supervisor`/`live-transcription` gate, unlike
// `live_transcription`'s own re-export below): `apps/desktop`'s settings screen
// (task #49) needs to save/load the user's selected STT provider on every
// platform, not just where a real live-transcription session can actually open.
pub use stt_provider_kind::{SttProviderKind, CREDENTIAL_SERVICE, SELECTED_STT_PROVIDER_ACCOUNT};
pub use timeline_adapter::align_track;
pub use upload_worker::{run_until_drained, upload_pending_once, UploadPassSummary};
