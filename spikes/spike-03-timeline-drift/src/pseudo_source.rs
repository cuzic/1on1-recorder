//! design.md §19.2 / spike-plan.md SPIKE-03 検証手順1:
//! OS APIなしで2つの疑似音源(Self: 440Hz, Remote: 880Hz)を生成する。
//! 異なるsample rate・意図的なclock drift・packet loss・discontinuity・
//! source restart(長い欠落として表現)を再現する。

use crate::rng::SimpleRng;
use std::f64::consts::PI;

#[derive(Debug, Clone)]
pub struct PseudoSourceConfig {
    pub tone_freq_hz: f64,
    pub nominal_rate_hz: u32,
    /// 実際のデバイスクロックが公称値からどれだけずれているか(ppm)。
    /// +50なら実際の実効サンプルレートは公称より50ppm速い。
    pub drift_ppm: f64,
    /// このソースのコールバック周期(異なるコールバック周期の再現用)。
    pub packet_duration_ms: u32,
    pub packet_loss_probability: f64,
    pub discontinuity_probability: f64,
    /// 全ソース共通の同期用パルス(真の経過時間basisで両ソースへ同時に注入する。
    /// アライメント後にcross-correlationで同期差を測るための基準信号)。
    pub sync_pulse_interval_secs: f64,
    pub sync_pulse_duration_ms: f64,
    pub seed: u64,
}

#[derive(Debug, Clone)]
pub struct PseudoPacket {
    /// 到着時刻(単調ホスト時刻、ns)。共通タイムラインへの配置に使う
    /// (design.md §11.2の`frame.host_time`に相当)。
    pub host_time_ns: u64,
    /// このパケットが表す公称(ドリフトなし)継続時間。期待サンプル数の算出に使う。
    pub nominal_duration_ns: u64,
    /// 実際に届いたサンプル(ドリフト・discontinuityの影響を受けた後の値)。
    pub samples: Vec<f32>,
    /// このパケットにdiscontinuityが含まれるか(OSのAUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY相当)。
    pub discontinuity: bool,
}

fn pulse_sample(t_in_pulse_secs: f64, pulse_duration_secs: f64) -> f32 {
    // raised-cosineエンベロープ + 2kHzキャリア。440/880Hzのトーンと明確に区別できる
    // 遷移波形にすることで、cross-correlationでの検出を安定させる。
    let envelope = 0.5 - 0.5 * (2.0 * PI * t_in_pulse_secs / pulse_duration_secs).cos();
    let carrier = (2.0 * PI * 2000.0 * t_in_pulse_secs).sin();
    (envelope * carrier * 0.9) as f32
}

/// `duration_secs`分の疑似音源をPseudoPacket列として生成する。
/// 生成後にpacket loss/discontinuityを適用するため、真の信号内容(音の高さ、
/// 同期パルスの位置)はドリフト以外の理由では歪まない。
pub fn generate(config: &PseudoSourceConfig, duration_secs: f64) -> Vec<PseudoPacket> {
    let mut rng = SimpleRng::new(config.seed);
    let actual_rate_hz = config.nominal_rate_hz as f64 * (1.0 + config.drift_ppm / 1_000_000.0);
    let nominal_duration_ns = config.packet_duration_ms as u64 * 1_000_000;
    let total_packets = ((duration_secs * 1000.0) / config.packet_duration_ms as f64).round() as u64;

    let mut packets = Vec::with_capacity(total_packets as usize);
    let mut host_time_ns: u64 = 0;
    // デバイスの累積サンプル数。累積丸めで長時間実行時の誤差蓄積を避ける
    // (spike-common::analyze同様の考え方)。
    let mut device_sample_pos: u64 = 0;
    let mut phase: f64 = 0.0;
    let pulse_duration_secs = config.sync_pulse_duration_ms / 1000.0;

    for _ in 0..total_packets {
        let packet_start_ns = host_time_ns;
        let packet_end_ns = host_time_ns + nominal_duration_ns;
        host_time_ns = packet_end_ns;

        let end_device_pos = (actual_rate_hz * (packet_end_ns as f64 / 1e9)).round() as u64;
        let frame_count = end_device_pos.saturating_sub(device_sample_pos);

        let mut samples = Vec::with_capacity(frame_count as usize);
        for _ in 0..frame_count {
            let true_time_secs = device_sample_pos as f64 / actual_rate_hz;
            let t_in_period = true_time_secs % config.sync_pulse_interval_secs;
            let value = if t_in_period < pulse_duration_secs {
                pulse_sample(t_in_period, pulse_duration_secs)
            } else {
                (phase.sin() * 0.5) as f32
            };
            samples.push(value);

            phase += 2.0 * PI * config.tone_freq_hz / actual_rate_hz;
            if phase > 2.0 * PI {
                phase -= 2.0 * PI;
            }
            device_sample_pos += 1;
        }

        let lost = rng.next_bool_with_probability(config.packet_loss_probability);
        if lost {
            continue; // このパケットは丸ごと失われる(共通タイムライン側はgapとして検出する)
        }

        let discontinuity = rng.next_bool_with_probability(config.discontinuity_probability);
        if discontinuity && samples.len() > 4 {
            // OSがdiscontinuityを報告するケースを模す: パケット後半の一部を
            // 欠落させる(実際のフレーム数が期待より少なくなる)。
            let drop = (samples.len() / 3).max(1);
            samples.truncate(samples.len().saturating_sub(drop));
        }

        packets.push(PseudoPacket {
            host_time_ns: packet_start_ns,
            nominal_duration_ns,
            samples,
            discontinuity,
        });
    }

    packets
}
