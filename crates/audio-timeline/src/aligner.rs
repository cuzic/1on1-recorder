//! Aligns a sequence of variably-timed audio packets from one source onto a single
//! continuous timeline anchored to a monotonic host clock, absorbing small clock drift
//! and hiding packet loss / discontinuities.
//!
//! Each packet's `host_time_ns` interval is defined against the host's monotonic clock
//! (which does not drift), so the number of samples that interval *should* contain
//! (`expected_frames`) can be computed directly from host time. Comparing that against
//! the number of samples actually delivered (`actual_frames`) tells us how far the
//! source's own clock has drifted for that packet:
//!
//! - If the deviation is small, it's treated as ordinary clock drift and smoothed away
//!   with linear interpolation.
//! - If the deviation is large (or the packet is flagged as discontinuous), it's treated
//!   as a real discontinuity or dropped audio rather than drift, and handled with a hard
//!   jump (silence padding or truncation) instead of a smooth resample.

use crate::resample::linear_resample;

/// A single arrival of audio from a source, in enough detail to align to a common
/// timeline. `samples` is mono `f32` at a nominal sample rate fixed by the aligner.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    /// Arrival time on a monotonic host clock (nanoseconds). Used to place this packet
    /// on the shared timeline.
    pub host_time_ns: u64,
    /// The nominal (drift-free) duration this packet represents. Used to compute how
    /// many samples the packet *should* contain.
    pub nominal_duration_ns: u64,
    /// The samples actually delivered for this interval (already reflecting whatever
    /// drift or loss occurred upstream).
    pub samples: Vec<f32>,
    /// Whether the source flagged this packet as discontinuous (e.g. WASAPI's
    /// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`). When set, the packet is always handled
    /// as a hard jump regardless of how small the frame-count deviation is.
    pub discontinuity: bool,
}

/// Per-packet frame-count deviations at or below this ratio are treated as ordinary
/// clock drift and smoothed with linear interpolation. Above this, the packet is
/// treated as a genuine discontinuity/loss and handled with a hard jump instead.
/// Real-world clock drift is on the order of a few hundred parts-per-million at most;
/// 5% is a generous safety margin above that.
pub const MAX_SMOOTH_RATIO_DEVIATION: f64 = 0.05;

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct AlignerStats {
    pub packets_processed: u64,
    /// Number of intervals where no data arrived at all (packet loss or a source restart).
    pub gaps_detected: u64,
    pub discontinuities_seen: u64,
    pub silence_frames_inserted: u64,
    pub resampled_frames: u64,
    /// Number of times a discontinuity or large deviation forced a hard jump (silence
    /// padding or truncation) instead of a smooth resample.
    pub hard_jumps: u64,
    pub max_single_packet_ratio_deviation: f64,
}

/// Aligns one source's packet stream onto a continuous mono `f32` timeline at a fixed
/// nominal sample rate. Feed packets in arrival order via [`ingest`](Self::ingest); read
/// the aligned output at any time via [`output`](Self::output).
pub struct TimelineAligner {
    nominal_rate_hz: u32,
    output: Vec<f32>,
    /// The host-time end (ns) of the interval this source's output currently covers.
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

    pub fn ingest(&mut self, packet: &AudioPacket) {
        let start_ns = self.next_expected_ns.unwrap_or(packet.host_time_ns);

        if packet.host_time_ns > start_ns {
            // Nothing arrived between the end of the previous output and the start of
            // this packet (packet loss or a source restart). Fill that gap with silence.
            let gap_ns = packet.host_time_ns - start_ns;
            let gap_frames = ns_to_frames_round(gap_ns, self.nominal_rate_hz);
            self.output.resize(self.output.len() + gap_frames, 0.0);
            self.stats.silence_frames_inserted += gap_frames as u64;
            self.stats.gaps_detected += 1;
        }

        let expected_frames = ns_to_frames_round(packet.nominal_duration_ns, self.nominal_rate_hz);
        let actual_frames = packet.samples.len();

        if expected_frames == 0 {
            // Nothing to do; guards against a degenerate zero-length interval.
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

    /// Pads the output with silence up to `end_ns` on the host clock. Call this once at
    /// the end of a session so that two independently-aligned tracks (e.g. a "self" and
    /// a "remote" track) always end up the same length even if one lost its final
    /// packets.
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

    fn packet(host_time_ns: u64, duration_ns: u64, samples: Vec<f32>, discontinuity: bool) -> AudioPacket {
        AudioPacket {
            host_time_ns,
            nominal_duration_ns: duration_ns,
            samples,
            discontinuity,
        }
    }

    #[test]
    fn gentle_drift_is_resampled_not_dropped() {
        let mut aligner = TimelineAligner::new(1000); // 1kHz makes the arithmetic easy to read.
        // Expect 20 frames (20ms), actually got 21 (5% deviation, right at clock-drift scale).
        aligner.ingest(&packet(0, 20_000_000, vec![0.0; 21], false));
        let stats = aligner.stats();
        assert_eq!(stats.hard_jumps, 0);
        assert_eq!(stats.resampled_frames, 20);
        assert_eq!(aligner.output().len(), 20);
    }

    #[test]
    fn large_deviation_is_hard_jump_not_smooth_resample() {
        let mut aligner = TimelineAligner::new(1000);
        // Expect 20 frames, got only 10 (50% short) -> hard jump.
        aligner.ingest(&packet(0, 20_000_000, vec![1.0; 10], false));
        let stats = aligner.stats();
        assert_eq!(stats.hard_jumps, 1);
        assert_eq!(stats.resampled_frames, 0);
        assert_eq!(aligner.output().len(), 20);
        // Padded with silence, not simply truncated.
        assert!(aligner.output()[10..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn packet_loss_gap_is_filled_with_silence_and_length_preserved() {
        let mut aligner = TimelineAligner::new(1000);
        aligner.ingest(&packet(0, 10_000_000, vec![1.0; 10], false));
        // The next packet arrives 20ms later instead of 10ms -> a whole 10ms packet was lost.
        aligner.ingest(&packet(20_000_000, 10_000_000, vec![1.0; 10], false));
        let stats = aligner.stats();
        assert_eq!(stats.gaps_detected, 1);
        assert_eq!(stats.silence_frames_inserted, 10);
        assert_eq!(aligner.output().len(), 30); // 10 + 10(silence) + 10
    }

    #[test]
    fn discontinuity_flag_forces_hard_jump_even_within_threshold() {
        let mut aligner = TimelineAligner::new(1000);
        // Expect 20, got 19 (under 5% deviation) but discontinuity=true forces a hard jump.
        aligner.ingest(&packet(0, 20_000_000, vec![1.0; 19], true));
        let stats = aligner.stats();
        assert_eq!(stats.hard_jumps, 1);
        assert_eq!(stats.resampled_frames, 0);
        assert_eq!(stats.discontinuities_seen, 1);
    }
}
