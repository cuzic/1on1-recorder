//! Orchestrates `capture -> align -> segment -> encode -> commit -> upload ->
//! finalize` for the meeting recorder. Stage 1 (task #7): an OS-independent pipeline
//! driven by `pseudo_source`, proving the wiring between `audio-timeline`,
//! `segment-store`, `session-store`, and `upload-client` without any real capture
//! backend. Stage 2 (task #10) adds a Windows supervisor; stage 3 (task #11) adds a
//! standing upload worker and richer recording-state management — neither exists yet.

mod error;
mod normalize;
mod pipeline;
mod segmenter;
mod timeline_adapter;

pub mod pseudo_source;

#[cfg(feature = "windows-supervisor")]
pub mod windows_supervisor;

pub use error::AppServiceError;
pub use normalize::normalize_to_mono;
pub use pipeline::run_pipeline;
pub use segmenter::{segment_pcm, PendingSegment};
pub use timeline_adapter::align_track;
