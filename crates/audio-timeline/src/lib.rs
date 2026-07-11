//! Aligns two (or more) independently-clocked real-time audio streams onto a shared
//! timeline, absorbing small clock drift and hiding packet loss / discontinuities,
//! without losing sync over long recordings.
//!
//! This crate factors out the alignment *policy* from any particular capture backend:
//! feed it a sequence of [`AudioPacket`]s (samples plus a monotonic host-clock arrival
//! time and nominal duration) via [`TimelineAligner::ingest`], and it produces a single
//! continuous track. Two independently-drifting sources fed through their own
//! `TimelineAligner` instances end up on the same timeline without ever needing to talk
//! to each other directly.
//!
//! See [`aligner`] for the alignment policy itself, and [`xcorr`] for a way to
//! independently measure how well two aligned tracks stayed in sync.

pub mod aligner;
pub mod resample;
pub mod xcorr;

pub use aligner::{AlignerStats, AudioPacket, TimelineAligner, MAX_SMOOTH_RATIO_DEVIATION};
pub use resample::linear_resample;
