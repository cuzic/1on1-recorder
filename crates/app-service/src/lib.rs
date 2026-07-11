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
mod segmenter;
mod session_lifecycle;
mod timeline_adapter;
mod upload_worker;

pub mod pseudo_source;

#[cfg(feature = "windows-supervisor")]
pub mod windows_frame_collector;
#[cfg(feature = "windows-supervisor")]
pub mod windows_session;
#[cfg(feature = "windows-supervisor")]
pub mod windows_supervisor;

pub use error::AppServiceError;
pub use normalize::normalize_to_mono;
pub use pipeline::run_pipeline;
pub use segmenter::{segment_pcm, PendingSegment};
pub use session_lifecycle::{begin_session, end_session, recover_incomplete_sessions};
pub use timeline_adapter::align_track;
pub use upload_worker::{run_until_drained, upload_pending_once, UploadPassSummary};
