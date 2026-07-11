//! Opus/Ogg encoding and atomic segment commit for the meeting recorder, with
//! restart-time recovery. Ported from spike-04-opus-atomic-commit, but registers
//! committed segments into `session-store`'s shared schema instead of a standalone
//! `SegmentDb`, and keys segments by `(session_id, track, sequence)` so Self/Remote
//! never collide.

mod error;
mod granule;
mod hash;
mod opus_ogg;
mod recovery;
mod segment_writer;

pub use error::SegmentStoreError;
pub use opus_ogg::{encode_segment_to_ogg_opus, FRAME_SAMPLES, SAMPLE_RATE_HZ};
pub use recovery::{scan_and_recover, RecoveredKind};
pub use segment_writer::{commit_segment, CrashPoint, SegmentRequest};
