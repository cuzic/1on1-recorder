//! Small-ratio resampling used to absorb clock drift within a single packet.
//!
//! `rubato` was evaluated as a replacement (see the crate-level docs), but its API is
//! built around continuously streaming fixed-size chunks with a runtime-adjustable
//! ratio, not one-shot "resize this buffer to exactly N frames" calls. Since the ratio
//! we need to correct is always close to 1.0 (real-world clock drift is on the order of
//! parts-per-million), a simple linear interpolation is sufficient and keeps this crate
//! dependency-free. Revisit this if higher audio fidelity than linear interpolation
//! provides is ever required.

/// Stretches or compresses `input` to exactly `output_len` samples via linear interpolation.
pub fn linear_resample(input: &[f32], output_len: usize) -> Vec<f32> {
    if output_len == 0 {
        return Vec::new();
    }
    if input.is_empty() {
        return vec![0.0; output_len];
    }
    if input.len() == 1 || output_len == 1 {
        return vec![input[0]; output_len];
    }

    let mut out = Vec::with_capacity(output_len);
    let scale = (input.len() - 1) as f64 / (output_len - 1) as f64;
    for i in 0..output_len {
        let pos = i as f64 * scale;
        let idx = (pos.floor() as usize).min(input.len() - 2);
        let frac = pos - idx as f64;
        let a = input[idx] as f64;
        let b = input[idx + 1] as f64;
        out.push((a + (b - a) * frac) as f32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_lengths_match() {
        let input = vec![0.0, 0.5, 1.0, -0.5];
        let out = linear_resample(&input, input.len());
        for (a, b) in input.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn stretches_short_ramp() {
        let input = vec![0.0, 1.0];
        let out = linear_resample(&input, 5);
        assert_eq!(out.len(), 5);
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[4] - 1.0).abs() < 1e-6);
        for w in out.windows(2) {
            assert!(w[1] >= w[0] - 1e-6);
        }
    }

    #[test]
    fn handles_empty_input() {
        assert_eq!(linear_resample(&[], 10), vec![0.0; 10]);
    }
}
