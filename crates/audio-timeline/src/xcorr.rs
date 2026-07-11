//! Cross-correlation based lag measurement between two signals.
//!
//! Useful for measuring how well two aligned tracks actually stayed in sync — e.g. in
//! tests, by injecting a known marker into both sources and measuring the lag between
//! them after alignment.

/// Slides `b` against `a` over `-max_lag..=max_lag` and returns the lag at which their
/// cross-correlation is highest, along with that peak score. A positive lag means `b`
/// lags behind `a`.
pub fn best_lag(a: &[f32], b: &[f32], max_lag: i64) -> (i64, f64) {
    let mut best_lag = 0i64;
    let mut best_score = f64::MIN;

    for lag in -max_lag..=max_lag {
        let mut sum = 0.0f64;
        let mut count = 0u64;
        for i in 0..a.len() {
            let j = i as i64 + lag;
            if j < 0 || j as usize >= b.len() {
                continue;
            }
            sum += a[i] as f64 * b[j as usize] as f64;
            count += 1;
        }
        if count == 0 {
            continue;
        }
        let score = sum / count as f64;
        if score > best_score {
            best_score = score;
            best_lag = lag;
        }
    }
    (best_lag, best_score)
}

/// Cuts out a window around `center_sample` from both tracks, using the **same absolute
/// range** in each, then estimates the lag between them via [`best_lag`].
///
/// Both windows must span an identical absolute range (radius `window_radius +
/// max_lag`), not an asymmetric one (e.g. `a` sized to `window_radius` and `b` sized to
/// `window_radius + max_lag`): an asymmetric window leaves very few overlapping samples
/// for candidates near `|lag| == window_radius`, which can produce a spurious
/// best-scoring lag from a handful of coincidentally-correlated samples.
pub fn measure_lag_at(
    track_a: &[f32],
    track_b: &[f32],
    center_sample: usize,
    window_radius: usize,
    max_lag: i64,
) -> Option<(i64, f64)> {
    let total_radius = window_radius + max_lag.unsigned_abs() as usize;
    let start = center_sample.saturating_sub(total_radius);
    let end_a = (center_sample + total_radius).min(track_a.len());
    let end_b = (center_sample + total_radius).min(track_b.len());
    if start >= end_a || start >= end_b {
        return None;
    }
    let a = &track_a[start..end_a];
    let b = &track_b[start..end_b];
    Some(best_lag(a, b, max_lag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_shift() {
        let n = 200;
        let signal: Vec<f32> = (0..n).map(|i| ((i as f64) * 0.3).sin() as f32).collect();
        let shift = 7i64;
        let mut shifted = vec![0.0f32; n];
        for i in 0..n {
            let j = i as i64 - shift;
            if j >= 0 && (j as usize) < n {
                shifted[i] = signal[j as usize];
            }
        }
        let (lag, _score) = best_lag(&signal, &shifted, 20);
        assert_eq!(lag, shift);
    }

    #[test]
    fn zero_lag_for_identical_signals() {
        let n = 100;
        let signal: Vec<f32> = (0..n).map(|i| ((i as f64) * 0.5).sin() as f32).collect();
        let (lag, _score) = best_lag(&signal, &signal, 10);
        assert_eq!(lag, 0);
    }
}
