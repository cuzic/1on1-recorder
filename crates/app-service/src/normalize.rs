//! Downmix-to-mono and resample-to-target-rate, run on every `CapturedFrame` before it
//! becomes an `audio_timeline::AudioPacket` — `audio-timeline` assumes mono input at
//! one fixed nominal rate (its `TimelineAligner::new(nominal_rate_hz)`), but a real
//! capture backend can hand back stereo and/or a device's native sample rate (e.g.
//! 44.1kHz) instead of the 48kHz mono Phase 1A standardizes on.

use audio_timeline::linear_resample;
use recorder_domain::CapturedFrame;

/// Downmixes (by averaging channels) and resamples `frame`'s samples to mono at
/// `target_rate_hz`. A no-op copy when the frame is already mono at that rate.
pub fn normalize_to_mono(frame: &CapturedFrame, target_rate_hz: u32) -> Vec<f32> {
    let channels = frame.channels.max(1) as usize;
    let mono: Vec<f32> = if channels <= 1 {
        frame.samples.clone()
    } else {
        frame.samples.chunks_exact(channels).map(|c| c.iter().sum::<f32>() / channels as f32).collect()
    };

    if frame.sample_rate == target_rate_hz || mono.is_empty() {
        mono
    } else {
        let target_len = ((mono.len() as f64) * target_rate_hz as f64 / frame.sample_rate as f64).round() as usize;
        linear_resample(&mono, target_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use recorder_domain::TrackKind;

    fn frame(sample_rate: u32, channels: u16, samples: Vec<f32>) -> CapturedFrame {
        CapturedFrame { track: TrackKind::SelfMic, host_time_ns: 0, source_time_ns: None, sample_rate, channels, samples, discontinuity: false }
    }

    #[test]
    fn mono_at_target_rate_is_unchanged() {
        let f = frame(48_000, 1, vec![0.1, 0.2, 0.3]);
        assert_eq!(normalize_to_mono(&f, 48_000), vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn stereo_is_downmixed_by_averaging_channels() {
        let f = frame(48_000, 2, vec![1.0, -1.0, 0.5, 0.5]);
        assert_eq!(normalize_to_mono(&f, 48_000), vec![0.0, 0.5]);
    }

    #[test]
    fn different_sample_rate_is_resampled_to_target_length() {
        let f = frame(44_100, 1, vec![0.0; 441]);
        let target_len = (441.0_f64 * 48_000.0 / 44_100.0).round() as usize;
        assert_eq!(normalize_to_mono(&f, 48_000).len(), target_len);
    }
}
