//! spike-plan.md SPIKE-03 合否基準:「相互相関による同期差測定」の実装。
//!
//! SelfとRemoteは異なるトーン周波数(440Hz/880Hz)を持つため、トーン自体を
//! 直接相互相関しても意味がない。そのためpseudo_source::generateは、両ソースへ
//! 「真の経過時間」基準で同時に同一の同期パルスを注入している。ここでは、
//! アライメント後の2トラックから同じ時刻付近のパルス窓を切り出し、
//! 相互相関のピーク位置からラグ(サンプル単位)を推定する。

/// aを基準に、bを`-max_lag..=max_lag`の範囲でずらしながら相互相関を計算し、
/// スコアが最大となるラグを返す。ラグが正の値は「bがaより遅れている」ことを表す。
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

/// `center_sample`付近の窓を両トラックから**同じ絶対区間**で切り出し、
/// `best_lag`でラグを推定する。
///
/// 両トラックの切り出し窓は`window_radius + max_lag`分の半径を持つ、同一の
/// 絶対区間にする(非対称にa=window_radius, b=window_radius+max_lagとすると、
/// |lag|がwindow_radiusを超えた候補で重なりサンプル数が極端に少なくなり、
/// 少数サンプルの偶然の相関で誤ったラグを検出しうる。実際にこの非対称実装で
/// 誤検出を起こしたため、対称な絶対区間の切り出しに修正した)。
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
        let signal: Vec<f32> = (0..n)
            .map(|i| ((i as f64) * 0.3).sin() as f32)
            .collect();
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
