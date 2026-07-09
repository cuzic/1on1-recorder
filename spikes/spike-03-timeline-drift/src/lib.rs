//! spike-plan.md SPIKE-03: 共通タイムライン整列・drift 補正(疑似音源)。
//! design.md §11(音声タイムライン設計)・§19.2(疑似キャプチャテスト)を
//! OS API非依存で検証する。

pub mod aligner;
pub mod pseudo_source;
pub mod resample;
pub mod rng;
pub mod xcorr;

use aligner::TimelineAligner;
use pseudo_source::PseudoSourceConfig;
use serde::Serialize;

pub const NOMINAL_RATE_HZ: u32 = 48_000;

#[derive(Debug, Serialize)]
pub struct SimulationReport {
    pub duration_secs: f64,
    pub self_len_frames: usize,
    pub remote_len_frames: usize,
    pub length_diff_frames: i64,
    pub self_stats: aligner::AlignerStats,
    pub remote_stats: aligner::AlignerStats,
    /// 各同期パルス位置で測定したラグ(ms)。design.md §3.2の「2時間録音後の
    /// Self/Remote同期差100ms以内」を裏付ける実測値。
    pub sync_lag_ms_samples: Vec<f64>,
    pub sync_lag_ms_max_abs: f64,
    pub sync_lag_ms_mean_abs: f64,
    pub wall_clock_secs: f64,
    pub realtime_speedup_factor: f64,
}

pub struct ScenarioConfig {
    pub duration_secs: f64,
    pub self_drift_ppm: f64,
    pub remote_drift_ppm: f64,
    pub self_packet_ms: u32,
    pub remote_packet_ms: u32,
    pub packet_loss_probability: f64,
    pub discontinuity_probability: f64,
    pub sync_pulse_interval_secs: f64,
    pub sync_pulse_duration_ms: f64,
    pub seed: u64,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            duration_secs: 2.0 * 3600.0,
            self_drift_ppm: 50.0,
            remote_drift_ppm: -50.0,
            self_packet_ms: 10,
            remote_packet_ms: 20,
            packet_loss_probability: 0.001,
            discontinuity_probability: 0.0005,
            sync_pulse_interval_secs: 60.0,
            sync_pulse_duration_ms: 5.0,
            seed: 0x5eed_1234,
        }
    }
}

/// design.md §19.2の疑似キャプチャテストを実行し、両トラックとレポートを返す。
pub fn run_simulation(config: &ScenarioConfig) -> (Vec<f32>, Vec<f32>, SimulationReport) {
    let wall_clock_start = std::time::Instant::now();

    let self_source_config = PseudoSourceConfig {
        tone_freq_hz: 440.0,
        nominal_rate_hz: NOMINAL_RATE_HZ,
        drift_ppm: config.self_drift_ppm,
        packet_duration_ms: config.self_packet_ms,
        packet_loss_probability: config.packet_loss_probability,
        discontinuity_probability: config.discontinuity_probability,
        sync_pulse_interval_secs: config.sync_pulse_interval_secs,
        sync_pulse_duration_ms: config.sync_pulse_duration_ms,
        seed: config.seed,
    };
    let remote_source_config = PseudoSourceConfig {
        tone_freq_hz: 880.0,
        nominal_rate_hz: NOMINAL_RATE_HZ,
        drift_ppm: config.remote_drift_ppm,
        packet_duration_ms: config.remote_packet_ms,
        packet_loss_probability: config.packet_loss_probability,
        discontinuity_probability: config.discontinuity_probability,
        sync_pulse_interval_secs: config.sync_pulse_interval_secs,
        sync_pulse_duration_ms: config.sync_pulse_duration_ms,
        // Self/Remoteとで異なるパケットロス・discontinuityの出方を再現するため
        // seedを変える(同期パルス自体は真の経過時間basisで両者に共通)。
        seed: config.seed ^ 0xa5a5_a5a5_a5a5_a5a5,
    };

    let self_packets = pseudo_source::generate(&self_source_config, config.duration_secs);
    let remote_packets = pseudo_source::generate(&remote_source_config, config.duration_secs);

    let mut self_aligner = TimelineAligner::new(NOMINAL_RATE_HZ);
    for p in &self_packets {
        self_aligner.ingest(p);
    }
    let mut remote_aligner = TimelineAligner::new(NOMINAL_RATE_HZ);
    for p in &remote_packets {
        remote_aligner.ingest(p);
    }

    let end_ns = (config.duration_secs * 1e9) as u64;
    self_aligner.finalize_up_to(end_ns);
    remote_aligner.finalize_up_to(end_ns);

    let self_stats = self_aligner.stats();
    let remote_stats = remote_aligner.stats();
    let self_track = self_aligner.into_output();
    let remote_track = remote_aligner.into_output();

    // 同期パルス位置での同期差測定(design.md §3.2の品質ゴールと突き合わせる)。
    let pulse_period_samples =
        (config.sync_pulse_interval_secs * NOMINAL_RATE_HZ as f64).round() as usize;
    let window_radius = (0.05 * NOMINAL_RATE_HZ as f64).round() as usize; // ±50ms
    let max_lag = (0.15 * NOMINAL_RATE_HZ as f64).round() as i64; // 最大150ms探索

    // 同期パルスは各周期の先頭(t=0, sync_pulse_interval_secs, 2*...)に注入されて
    // いるため、測定中心もそこに合わせる(周期の半分ずらすと、パルスが存在しない
    // 位置を相関計算してしまい、意味のないラグを検出してしまう。実際にこの
    // ズレで誤検出したため、center=0起点に修正した)。境界での非対称な窓を
    // 避けるため、最初の1周期はスキップする。
    let mut sync_lag_ms_samples = Vec::new();
    let mut center = pulse_period_samples;
    while center < self_track.len() && center < remote_track.len() {
        if let Some((lag, _score)) =
            xcorr::measure_lag_at(&self_track, &remote_track, center, window_radius, max_lag)
        {
            sync_lag_ms_samples.push(lag as f64 / NOMINAL_RATE_HZ as f64 * 1000.0);
        }
        center += pulse_period_samples;
    }

    let sync_lag_ms_max_abs = sync_lag_ms_samples
        .iter()
        .fold(0.0f64, |acc, v| acc.max(v.abs()));
    let sync_lag_ms_mean_abs = if sync_lag_ms_samples.is_empty() {
        0.0
    } else {
        sync_lag_ms_samples.iter().map(|v| v.abs()).sum::<f64>() / sync_lag_ms_samples.len() as f64
    };

    let wall_clock_secs = wall_clock_start.elapsed().as_secs_f64();
    let realtime_speedup_factor = if wall_clock_secs > 0.0 {
        config.duration_secs / wall_clock_secs
    } else {
        f64::INFINITY
    };

    let report = SimulationReport {
        duration_secs: config.duration_secs,
        self_len_frames: self_track.len(),
        remote_len_frames: remote_track.len(),
        length_diff_frames: self_track.len() as i64 - remote_track.len() as i64,
        self_stats,
        remote_stats,
        sync_lag_ms_samples,
        sync_lag_ms_max_abs,
        sync_lag_ms_mean_abs,
        wall_clock_secs,
        realtime_speedup_factor,
    };

    (self_track, remote_track, report)
}
