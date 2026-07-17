//! Resamples the fixed-format PCM `windows_frame_collector::collect_frames` feeds
//! into `live_transcription`'s `stt_sink` down to whatever rate the selected STT
//! provider requires (task #48): `stt-openai` requires exactly 24kHz mono PCM16 and
//! `stt-assemblyai` requires exactly 16kHz mono PCM16, both hard-rejecting any other
//! rate (see each crate's module doc comment); `stt-deepgram`/`stt-google` accept
//! whatever rate they're given. Reuses `audio_timeline::linear_resample` — the same
//! simple linear-interpolation approach `normalize.rs` already uses for its own
//! cross-sample-rate step — rather than pulling in a dedicated resampling crate.

use audio_timeline::linear_resample;

/// Resamples `samples` (mono PCM `f32`, nominally `-1.0..=1.0`) from `from_hz` to
/// `to_hz`. A no-op clone when the rates already match or `samples` is empty (mirrors
/// `normalize::normalize_to_mono`'s same two short-circuits).
pub fn resample(samples: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || samples.is_empty() {
        return samples.to_vec();
    }
    let target_len = ((samples.len() as f64) * to_hz as f64 / from_hz as f64).round() as usize;
    linear_resample(samples, target_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_is_returned_unchanged() {
        assert_eq!(resample(&[0.1, 0.2, 0.3], 48_000, 48_000), vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn empty_input_stays_empty() {
        assert!(resample(&[], 48_000, 16_000).is_empty());
    }

    #[test]
    fn downsamples_48k_to_16k_by_a_third() {
        let input = vec![0.0_f32; 480];
        assert_eq!(resample(&input, 48_000, 16_000).len(), 160);
    }

    #[test]
    fn downsamples_48k_to_24k_by_half() {
        let input = vec![0.0_f32; 480];
        assert_eq!(resample(&input, 48_000, 24_000).len(), 240);
    }

    #[test]
    fn upsamples_16k_to_48k() {
        let input = vec![0.0_f32; 160];
        assert_eq!(resample(&input, 16_000, 48_000).len(), 480);
    }

    #[test]
    fn preserves_signal_shape_across_a_resample() {
        // A single-cycle ramp should still start near its original first sample and
        // end near its original last sample after resampling, not get scrambled.
        let input: Vec<f32> = (0..48).map(|i| i as f32 / 48.0).collect();
        let output = resample(&input, 48_000, 16_000);
        assert!((output[0] - input[0]).abs() < 0.05);
        assert!((output[output.len() - 1] - input[input.len() - 1]).abs() < 0.05);
    }
}
