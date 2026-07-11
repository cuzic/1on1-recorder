//! Core domain types for the meeting recorder, ported from design.md §9/§10/§13.
//! Contains no OS-specific types — those live in `capture-windows` and friends.

mod frame;
mod segment;
mod session;
mod state;
mod track;
mod upload;

pub use frame::CapturedFrame;
pub use segment::{AudioCodec, AudioSegment};
pub use session::{
    AudioManifest, CaptureManifest, ConsentManifest, RemoteSourceKind, SessionId, SessionManifest,
};
pub use state::{CaptureState, UploadState};
pub use track::TrackKind;
pub use upload::{RemoteSession, SessionSummary, UploadAdapter, UploadError, UploadReceipt};
