//! A synthetic stand-in for a real OS capture backend (`capture-windows` and future
//! `capture-linux`/`capture-macos`), used only to exercise this crate's pipeline
//! without any real audio hardware or OS API. Stage 2 (task #10) replaces this with
//! actual `capture-windows` output.

use recorder_domain::{CapturedFrame, TrackKind};

#[derive(Debug, Clone, Copy)]
pub struct PseudoSourceConfig {
    pub duration_secs: u32,
    /// How often the (simulated) backend delivers a frame — a fixed property of the
    /// capture stream itself (e.g. WASAPI's configured buffer period), independent of
    /// how many samples end up in any one frame. This is exactly the "nominal
    /// duration" `audio_timeline::AudioPacket` needs and `CapturedFrame` doesn't
    /// carry, since it isn't something derivable from a single frame in isolation.
    pub frame_interval_ms: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub tone_freq_hz: f32,
}

/// Generates a steady (zero-jitter, zero-drift) sequence of frames for one track. Real
/// alignment-under-drift behavior is `audio-timeline`'s own concern and is already
/// covered by that crate's simulation tests — this pseudo source only needs to prove
/// the pipeline's wiring, not re-prove the alignment math.
pub fn generate_frames(track: TrackKind, config: &PseudoSourceConfig) -> Vec<CapturedFrame> {
    let interval_ns = config.frame_interval_ms as u64 * 1_000_000;
    let samples_per_frame = (config.sample_rate as u64 * config.frame_interval_ms as u64 / 1000) as usize;
    let total_frames = (config.duration_secs as u64 * 1000 / config.frame_interval_ms as u64) as usize;
    let channels = config.channels.max(1) as usize;

    let mut frames = Vec::with_capacity(total_frames);
    for i in 0..total_frames {
        let mut samples = Vec::with_capacity(samples_per_frame * channels);
        for s in 0..samples_per_frame {
            let global_sample_index = i * samples_per_frame + s;
            let t = global_sample_index as f32 / config.sample_rate as f32;
            let v = 0.2 * (2.0 * std::f32::consts::PI * config.tone_freq_hz * t).sin();
            for _ in 0..channels {
                samples.push(v);
            }
        }
        frames.push(CapturedFrame {
            track,
            host_time_ns: i as u64 * interval_ns,
            source_time_ns: None,
            sample_rate: config.sample_rate,
            channels: config.channels,
            samples,
            discontinuity: false,
        });
    }
    frames
}

pub fn nominal_frame_interval_ns(config: &PseudoSourceConfig) -> u64 {
    config.frame_interval_ms as u64 * 1_000_000
}
