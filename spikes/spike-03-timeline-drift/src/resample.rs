//! design.md §11.4: 「小さい差: resampler比率を緩やかに調整」の実装。
//!
//! rubatoクレート(crates.ioで利用可能なことは確認済み)を評価したが、本スパイクが
//! 検証したいのは比率がほぼ1.0近辺(実運用ではppmオーダー)の緩やかなドリフト
//! 吸収であり、sinc補間ベースの汎用リサンプラーは本用途には過剰と判断し、
//! 単純な線形補間で実装する。

/// `input`を線形補間で`output_len`サンプルへ引き伸ばし/圧縮する。
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
        // 単調増加であること
        for w in out.windows(2) {
            assert!(w[1] >= w[0] - 1e-6);
        }
    }

    #[test]
    fn handles_empty_input() {
        assert_eq!(linear_resample(&[], 10), vec![0.0; 10]);
    }
}
