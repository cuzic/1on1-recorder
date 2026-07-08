// spike-windows-01-02-detail-design.md §4.9
//
// P0-5: gap検出・drift回帰・単調性チェック・wakeジッタは、必ず
// group_records_by_epochで(stream, capture_epoch, target_pid)単位に
// 分割してから、各グループへ個別に適用すること。epoch境界をまたいで
// 連続処理すると、再アタッチ時の正当なリセットを巨大なgap/overlapや
// 単調性違反として誤検出する。

use crate::frame_record::{CapturedFrameRecord, StreamId};
use std::collections::HashMap;

/// 解析対象のグルーピングキー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EpochKey {
    pub stream: StreamId,
    pub capture_epoch: u64,
    pub target_pid: Option<u32>,
}

pub struct EpochRecords<'a> {
    pub key: EpochKey,
    pub records: Vec<&'a CapturedFrameRecord>, // packet_seq昇順
    /// このepochの最初/最後のレコードのcapture_qpc_100ns。
    /// 相対drift比較で、時間範囲が重なるepoch同士だけを比較するために使う。
    pub qpc_range_100ns: (u64, u64),
}

/// CSVから読み込んだレコード列を(stream, capture_epoch, target_pid)でグルーピングする。
pub fn group_records_by_epoch(records: &[CapturedFrameRecord]) -> Vec<EpochRecords<'_>> {
    let mut buckets: HashMap<EpochKey, Vec<&CapturedFrameRecord>> = HashMap::new();
    for r in records {
        let key = EpochKey {
            stream: r.stream,
            capture_epoch: r.capture_epoch,
            target_pid: r.target_pid,
        };
        buckets.entry(key).or_default().push(r);
    }

    buckets
        .into_iter()
        .map(|(key, mut recs)| {
            recs.sort_by_key(|r| r.packet_seq);
            let qpc_range_100ns = (
                recs.first().map(|r| r.capture_qpc_100ns).unwrap_or(0),
                recs.last().map(|r| r.capture_qpc_100ns).unwrap_or(0),
            );
            EpochRecords {
                key,
                records: recs,
                qpc_range_100ns,
            }
        })
        .collect()
}

/// device_position_frames の連続性チェック(パケット欠落検出)。
/// 1つのEpochRecords(単一のstream・epoch・PID)をpacket_seq順に走査し、
/// 前パケットの終端位置と現パケットの開始位置を比較する。
#[derive(Debug, Default, Clone, Copy)]
pub struct PositionGapStats {
    pub gap_frames_total: u64,
    pub overlap_frames_total: u64,
    pub gap_events: u64,
    pub overlap_events: u64,
}

pub fn detect_position_gaps(records: &[&CapturedFrameRecord]) -> PositionGapStats {
    let mut stats = PositionGapStats::default();
    let mut prev_end: Option<u64> = None;
    for r in records {
        if let Some(expected_next) = prev_end {
            let actual = r.device_position_frames;
            if actual > expected_next {
                stats.gap_frames_total += actual - expected_next;
                stats.gap_events += 1;
            } else if actual < expected_next {
                stats.overlap_frames_total += expected_next - actual;
                stats.overlap_events += 1;
            }
        }
        prev_end = Some(r.device_position_frames + r.frame_count as u64);
    }
    stats
}

/// QPC経過時間に対するdevice_position_framesの傾きを線形回帰で求め、
/// 公称サンプルレートとの差をppmで返す。
#[derive(Debug, Clone, Copy)]
pub struct ClockDriftEstimate {
    pub effective_sample_rate_hz: f64,
    pub drift_ppm: f64,
}

fn least_squares_slope(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let sum_x: f64 = points.iter().map(|(x, _)| x).sum();
    let sum_y: f64 = points.iter().map(|(_, y)| y).sum();
    let sum_xy: f64 = points.iter().map(|(x, y)| x * y).sum();
    let sum_xx: f64 = points.iter().map(|(x, _)| x * x).sum();
    let denom = n * sum_xx - sum_x * sum_x;
    if denom.abs() < f64::EPSILON {
        return 0.0;
    }
    (n * sum_xy - sum_x * sum_y) / denom
}

/// 【単位の訂正】capture_qpc_100nsは既に100ns単位であり、QPCの生カウントでは
/// ない。秒への換算は定数10_000_000.0で行う(周波数での除算は不要・不正)。
/// 先頭値を差し引いた相対値で回帰し、浮動小数点の桁落ちを避ける。
pub fn estimate_clock_drift(
    position_series: &[(u64, u64)], // (capture_qpc_100ns, device_position_frames)
    nominal_sample_rate_hz: u32,
) -> ClockDriftEstimate {
    if position_series.is_empty() {
        return ClockDriftEstimate {
            effective_sample_rate_hz: nominal_sample_rate_hz as f64,
            drift_ppm: 0.0,
        };
    }
    let (first_qpc_100ns, first_pos_frames) = position_series[0];

    let points: Vec<(f64, f64)> = position_series
        .iter()
        .map(|&(qpc_100ns, pos_frames)| {
            let x_sec = (qpc_100ns.saturating_sub(first_qpc_100ns)) as f64 / 10_000_000.0;
            let y_frames = (pos_frames.saturating_sub(first_pos_frames)) as f64;
            (x_sec, y_frames)
        })
        .collect();

    let effective_sample_rate_hz = least_squares_slope(&points);
    let drift_ppm = (effective_sample_rate_hz / nominal_sample_rate_hz as f64 - 1.0) * 1_000_000.0;

    ClockDriftEstimate {
        effective_sample_rate_hz,
        drift_ppm,
    }
}

/// 2ストリーム間の相対drift。**同じ時間範囲を共有するepoch同士でのみ**比較すること。
pub fn relative_drift_ppm(mic: &ClockDriftEstimate, loopback: &ClockDriftEstimate) -> f64 {
    (mic.effective_sample_rate_hz / loopback.effective_sample_rate_hz - 1.0) * 1_000_000.0
}

/// mic_epochとloopback_epochのqpc_range_100nsが重ならない場合はNoneを返す
/// (比較不能。summaryへはnullとして出力する)。
pub fn overlapping_relative_drift_ppm(
    mic_epoch: &EpochRecords,
    mic_drift: &ClockDriftEstimate,
    loopback_epoch: &EpochRecords,
    loopback_drift: &ClockDriftEstimate,
) -> Option<f64> {
    let (a_start, a_end) = mic_epoch.qpc_range_100ns;
    let (b_start, b_end) = loopback_epoch.qpc_range_100ns;
    if a_start > b_end || b_start > a_end {
        return None;
    }
    Some(relative_drift_ppm(mic_drift, loopback_drift))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IntervalStats {
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WakeTimingReport {
    pub expected_interval_ms: f64,
    pub interval: IntervalStats,
    pub jitter: JitterStats,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// wake_seq単位のwake_qpc_100ns差分列からWakeTimingReportを算出する。
/// packet_seq側の重複値(同一wakeで複数パケットを排出した場合)を混ぜないこと。
pub fn compute_wake_timing(wake_qpc_100ns_series: &[u64], expected_interval_ms: f64) -> WakeTimingReport {
    if wake_qpc_100ns_series.len() < 2 {
        return WakeTimingReport {
            expected_interval_ms,
            ..Default::default()
        };
    }
    let mut intervals_ms: Vec<f64> = wake_qpc_100ns_series
        .windows(2)
        .map(|w| (w[1].saturating_sub(w[0])) as f64 / 10_000.0)
        .collect();
    intervals_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let mean_interval = intervals_ms.iter().sum::<f64>() / intervals_ms.len() as f64;
    let interval = IntervalStats {
        mean_ms: mean_interval,
        p95_ms: percentile(&intervals_ms, 0.95),
        p99_ms: percentile(&intervals_ms, 0.99),
        max_ms: *intervals_ms.last().unwrap(),
    };

    let mut jitters_ms: Vec<f64> = intervals_ms.iter().map(|v| v - expected_interval_ms).collect();
    jitters_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean_jitter = jitters_ms.iter().sum::<f64>() / jitters_ms.len() as f64;
    let jitter = JitterStats {
        mean_ms: mean_jitter,
        p95_ms: percentile(&jitters_ms, 0.95),
        p99_ms: percentile(&jitters_ms, 0.99),
        max_ms: *jitters_ms.last().unwrap(),
    };

    WakeTimingReport {
        expected_interval_ms,
        interval,
        jitter,
    }
}

/// wake_qpc_100ns - capture_qpc_100ns を「起床時点で観測されたパケットの
/// 経過時間」として集計する。スケジューリング遅延と断定しないこと(§3.2参照)。
pub fn compute_packet_age_at_wake(records: &[&CapturedFrameRecord]) -> JitterStats {
    let mut ages_ms: Vec<f64> = records
        .iter()
        .map(|r| (r.wake_qpc_100ns.saturating_sub(r.capture_qpc_100ns)) as f64 / 10_000.0)
        .collect();
    if ages_ms.is_empty() {
        return JitterStats::default();
    }
    ages_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean_ms = ages_ms.iter().sum::<f64>() / ages_ms.len() as f64;
    JitterStats {
        mean_ms,
        p95_ms: percentile(&ages_ms, 0.95),
        p99_ms: percentile(&ages_ms, 0.99),
        max_ms: *ages_ms.last().unwrap(),
    }
}

pub fn detect_monotonic_violations(qpc_series_100ns: &[u64]) -> u64 {
    let mut violations = 0u64;
    let mut last: Option<u64> = None;
    for &v in qpc_series_100ns {
        if let Some(l) = last {
            if v < l {
                violations += 1;
            }
        }
        last = Some(v);
    }
    violations
}

/// GetProcessTimes(kernel time + user time)の差分を経過壁時計時間で割り、
/// 論理コア数で割らない「1コア相当%」として算出する。
pub struct ProcessTimes {
    pub kernel_time_100ns: u64,
    pub user_time_100ns: u64,
}

pub fn measure_cpu_percent(start: ProcessTimes, end: ProcessTimes, wall_secs: f64) -> f64 {
    if wall_secs <= 0.0 {
        return 0.0;
    }
    let cpu_100ns = (end.kernel_time_100ns + end.user_time_100ns)
        .saturating_sub(start.kernel_time_100ns + start.user_time_100ns);
    let cpu_secs = cpu_100ns as f64 / 10_000_000.0;
    (cpu_secs / wall_secs) * 100.0
}

pub fn measure_peak_working_set_bytes() -> u64 {
    // TODO(§4.9): GetProcessMemoryInfo().PeakWorkingSetSize (Win32専用)
    0
}
