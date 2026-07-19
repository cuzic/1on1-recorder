//! Simple signal-level measurement shared by callers that need a cheap loudness
//! estimate — e.g. a crude speech/silence threshold — without a real VAD library.

/// Root-mean-square amplitude of `samples` (expected in `-1.0..=1.0`, as produced by
/// the rest of this crate's resampling/alignment code). `0.0` for an empty slice.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_empty_is_zero() {
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn rms_of_silence_is_zero() {
        assert_eq!(rms(&[0.0; 10]), 0.0);
    }

    #[test]
    fn rms_of_a_constant_tone_is_its_amplitude() {
        assert!((rms(&[0.5; 10]) - 0.5).abs() < 1e-6);
    }
}
