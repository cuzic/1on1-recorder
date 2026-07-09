//! design.md §11.2〜§11.4 の共通タイムライン整列 + drift補正の実装。
//!
//! 各ソースは独立に`TimelineAligner`へ`PseudoPacket`を`ingest`する。
//! 出力は「T0からの経過時間 = サンプルインデックス / nominal_rate_hz」という
//! 単一の連続トラック(mono, f32, nominal_rate_hz固定)であり、20msスロットへの
//! 割付けはこの連続トラックを単純にスライスするだけで得られる(§11.2の
//! `timeline_offset = frame.host_time - T0`と等価)。
//!
//! ドリフト補正の考え方: 各パケットの`host_time_ns`区間はホスト側の単調時計
//! (ドリフトしない)で決まっているため、「その区間に本来何サンプル必要か」
//! (`expected_frames`)をhost時間から直接計算できる。実際に届いたサンプル数
//! (`actual_frames`)との比率がわずかであれば線形補間で滑らかに引き伸ばし、
//! 大きければ(discontinuityやpacket lossに起因すると判断し)滑らかな補正を
//! せず無音挿入/超過サンプル破棄で処理する(design.md §11.4の二段階方針)。

use crate::pseudo_source::PseudoPacket;
use crate::resample::linear_resample;

/// 単一パケットでの補正比率がこれを超えたら「クロックドリフトではなく
/// 本物の不連続/欠落」とみなし、滑らかな補正をせずhard jumpで処理する。
/// 実際のクロックドリフトはppmオーダー(高々数百ppm)であり、5%は十分に
/// 大きい安全マージン。
pub const MAX_SMOOTH_RATIO_DEVIATION: f64 = 0.05;

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct AlignerStats {
    pub packets_processed: u64,
    /// host時間上でデータが全く届かなかった区間の数(packet lossまたは
    /// source restartによる欠落)。
    pub gaps_detected: u64,
    pub discontinuities_seen: u64,
    pub silence_frames_inserted: u64,
    pub resampled_frames: u64,
    /// discontinuity/大きな欠落によりhard jump(破棄 or 無音挿入で帳尻を
    /// 合わせる)処理をした回数。
    pub hard_jumps: u64,
    pub max_single_packet_ratio_deviation: f64,
}

pub struct TimelineAligner {
    nominal_rate_hz: u32,
    output: Vec<f32>,
    /// このソースの出力がこれまでにカバーしているhost時間の終端(ns)。
    next_expected_ns: Option<u64>,
    stats: AlignerStats,
}

fn ns_to_frames_round(ns: u64, rate_hz: u32) -> usize {
    ((ns as f64) * rate_hz as f64 / 1e9).round().max(0.0) as usize
}

impl TimelineAligner {
    pub fn new(nominal_rate_hz: u32) -> Self {
        Self {
            nominal_rate_hz,
            output: Vec::new(),
            next_expected_ns: None,
            stats: AlignerStats::default(),
        }
    }

    pub fn stats(&self) -> AlignerStats {
        self.stats
    }

    pub fn output(&self) -> &[f32] {
        &self.output
    }

    pub fn into_output(self) -> Vec<f32> {
        self.output
    }

    pub fn ingest(&mut self, packet: &PseudoPacket) {
        let start_ns = self.next_expected_ns.unwrap_or(packet.host_time_ns);

        if packet.host_time_ns > start_ns {
            // 直前の出力終端から今回のパケット開始までデータが全く届いていない
            // (packet lossまたはsource restart)。その区間はsilenceで埋める。
            let gap_ns = packet.host_time_ns - start_ns;
            let gap_frames = ns_to_frames_round(gap_ns, self.nominal_rate_hz);
            self.output.resize(self.output.len() + gap_frames, 0.0);
            self.stats.silence_frames_inserted += gap_frames as u64;
            self.stats.gaps_detected += 1;
        }

        let expected_frames = ns_to_frames_round(packet.nominal_duration_ns, self.nominal_rate_hz);
        let actual_frames = packet.samples.len();

        if expected_frames == 0 {
            // 何もすることがない(理論上は起きない想定だが、0除算等を避けるため
            // 明示的に扱う)。
        } else if actual_frames == 0 {
            self.output.resize(self.output.len() + expected_frames, 0.0);
            self.stats.silence_frames_inserted += expected_frames as u64;
        } else {
            let ratio = expected_frames as f64 / actual_frames as f64;
            let deviation = (ratio - 1.0).abs();
            if deviation > self.stats.max_single_packet_ratio_deviation {
                self.stats.max_single_packet_ratio_deviation = deviation;
            }

            if packet.discontinuity || deviation > MAX_SMOOTH_RATIO_DEVIATION {
                self.stats.hard_jumps += 1;
                if actual_frames >= expected_frames {
                    self.output
                        .extend_from_slice(&packet.samples[..expected_frames]);
                } else {
                    self.output.extend_from_slice(&packet.samples);
                    let pad = expected_frames - actual_frames;
                    self.output.resize(self.output.len() + pad, 0.0);
                    self.stats.silence_frames_inserted += pad as u64;
                }
            } else {
                let resampled = linear_resample(&packet.samples, expected_frames);
                self.stats.resampled_frames += resampled.len() as u64;
                self.output.extend_from_slice(&resampled);
            }
        }

        if packet.discontinuity {
            self.stats.discontinuities_seen += 1;
        }
        self.stats.packets_processed += 1;
        self.next_expected_ns = Some(packet.host_time_ns + packet.nominal_duration_ns);
    }

    /// 指定したhost時間(ns)までを疑似的な「シミュレーション終了」として
    /// 埋め切る。末尾でpacket lossが起きていた場合、出力トラック長を
    /// 揃えるために呼ぶ(design.md §11.3「Self/Remoteのファイル長は常に一致させる」)。
    pub fn finalize_up_to(&mut self, end_ns: u64) {
        let start_ns = self.next_expected_ns.unwrap_or(0);
        if end_ns > start_ns {
            let gap_ns = end_ns - start_ns;
            let gap_frames = ns_to_frames_round(gap_ns, self.nominal_rate_hz);
            self.output.resize(self.output.len() + gap_frames, 0.0);
            self.stats.silence_frames_inserted += gap_frames as u64;
        }
        self.next_expected_ns = Some(end_ns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(host_time_ns: u64, duration_ns: u64, samples: Vec<f32>, discontinuity: bool) -> PseudoPacket {
        PseudoPacket {
            host_time_ns,
            nominal_duration_ns: duration_ns,
            samples,
            discontinuity,
        }
    }

    #[test]
    fn gentle_drift_is_resampled_not_dropped() {
        let mut aligner = TimelineAligner::new(1000); // 1kHzで計算しやすくする
        // 10ms(=10フレーム期待)のところ、わずかに多い11フレーム(10%届いた…
        // ではなくppmオーダーの想定なので、5%未満のずれで試す: 期待10、実際10.3相当)
        // 整数フレームでは表現しづらいため、期待20フレームに対し実際21フレーム(5%)を使う。
        aligner.ingest(&packet(0, 20_000_000, vec![0.0; 21], false));
        let stats = aligner.stats();
        assert_eq!(stats.hard_jumps, 0);
        assert_eq!(stats.resampled_frames, 20);
        assert_eq!(aligner.output().len(), 20);
    }

    #[test]
    fn large_deviation_is_hard_jump_not_smooth_resample() {
        let mut aligner = TimelineAligner::new(1000);
        // 期待20フレームに対し実際10フレームしかない(50%不足) -> hard jump
        aligner.ingest(&packet(0, 20_000_000, vec![1.0; 10], false));
        let stats = aligner.stats();
        assert_eq!(stats.hard_jumps, 1);
        assert_eq!(stats.resampled_frames, 0);
        assert_eq!(aligner.output().len(), 20);
        // 破棄ではなく無音挿入で埋められていること
        assert!(aligner.output()[10..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn packet_loss_gap_is_filled_with_silence_and_length_preserved() {
        let mut aligner = TimelineAligner::new(1000);
        aligner.ingest(&packet(0, 10_000_000, vec![1.0; 10], false));
        // 次のパケットが20ms分あとから届く(間の10ms分のパケットが丸ごとロスト)。
        aligner.ingest(&packet(20_000_000, 10_000_000, vec![1.0; 10], false));
        let stats = aligner.stats();
        assert_eq!(stats.gaps_detected, 1);
        assert_eq!(stats.silence_frames_inserted, 10);
        assert_eq!(aligner.output().len(), 30); // 10 + 10(silence) + 10
    }

    #[test]
    fn discontinuity_flag_forces_hard_jump_even_within_threshold() {
        let mut aligner = TimelineAligner::new(1000);
        // 期待20、実際19(5%未満のずれ)だが discontinuity=true なので hard jump扱い。
        aligner.ingest(&packet(0, 20_000_000, vec![1.0; 19], true));
        let stats = aligner.stats();
        assert_eq!(stats.hard_jumps, 1);
        assert_eq!(stats.resampled_frames, 0);
        assert_eq!(stats.discontinuities_seen, 1);
    }
}
