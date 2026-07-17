//! SQLite-backed ledger of sessions, tracks, segments, upload status, and events.
//! Consolidates what spike-04's `SegmentDb` and spike-08's `SpoolDb` each tracked on
//! their own into one schema, so `segment-store` and `upload-client` register into the
//! same source of truth instead of two independently-evolving databases.

mod error;
mod schema;
mod state_codec;
mod store;

pub use error::StoreError;
pub use store::{SessionStore, Summary, TranscriptSegment};
