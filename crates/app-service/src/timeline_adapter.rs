//! Converts one track's `CapturedFrame` sequence into a single continuous, aligned
//! `Vec<f32>` at a fixed nominal rate — the bridge between a capture backend's raw
//! frame stream and `audio-timeline`'s alignment policy.

use audio_timeline::{AudioPacket, TimelineAligner};
use recorder_domain::CapturedFrame;

use crate::normalize::normalize_to_mono;

/// Feeds every frame through `normalize_to_mono` and `TimelineAligner::ingest`, then
/// pads to `total_duration_ns` so two independently-aligned tracks (Self/Remote) end
/// up the same length even if one lost its final frames.
pub fn align_track(frames: &[CapturedFrame], nominal_rate_hz: u32, nominal_frame_interval_ns: u64, total_duration_ns: u64) -> Vec<f32> {
    let mut aligner = TimelineAligner::new(nominal_rate_hz);
    for frame in frames {
        let mono = normalize_to_mono(frame, nominal_rate_hz);
        let packet = AudioPacket {
            host_time_ns: frame.host_time_ns,
            nominal_duration_ns: nominal_frame_interval_ns,
            samples: mono,
            discontinuity: frame.discontinuity,
        };
        aligner.ingest(&packet);
    }
    aligner.finalize_up_to(total_duration_ns);
    aligner.into_output()
}
