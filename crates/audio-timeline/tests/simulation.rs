//! End-to-end proof that `TimelineAligner` actually keeps two independently-drifting,
//! independently-lossy pseudo sources in sync over a long recording, without relying on
//! any OS audio API. Mirrors the acceptance criteria this alignment policy was
//! originally validated against (design.md §3.2 / §19.2): track lengths must match
//! exactly and stay within 100ms of sync even with clock drift, packet loss, and
//! discontinuities.
//!
//! `cargo test`'s default debug build runs a short scenario for speed; the full
//! 2-hour scenario and the >=10x realtime speedup requirement are `#[ignore]`d and
//! meant to be run in `--release` (numeric code is typically 10-50x slower in debug).

use audio_timeline::{xcorr, AudioPacket, TimelineAligner};
use std::f64::consts::PI;

/// Deterministic xorshift64* PRNG so tests are fully reproducible from a seed alone.
struct SimpleRng(u64);

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn next_bool_with_probability(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

struct PseudoSourceConfig {
    tone_freq_hz: f64,
    nominal_rate_hz: u32,
    /// How far this source's real clock deviates from nominal, in ppm.
    drift_ppm: f64,
    packet_duration_ms: u32,
    packet_loss_probability: f64,
    discontinuity_probability: f64,
    /// A marker pulse injected into both sources simultaneously on true elapsed time,
    /// used to measure sync error via cross-correlation after alignment.
    sync_pulse_interval_secs: f64,
    sync_pulse_duration_ms: f64,
    seed: u64,
}

fn pulse_sample(t_in_pulse_secs: f64, pulse_duration_secs: f64) -> f32 {
    // Raised-cosine envelope + 2kHz carrier: distinct enough from the 440/880Hz test
    // tones to make cross-correlation detection reliable.
    let envelope = 0.5 - 0.5 * (2.0 * PI * t_in_pulse_secs / pulse_duration_secs).cos();
    let carrier = (2.0 * PI * 2000.0 * t_in_pulse_secs).sin();
    (envelope * carrier * 0.9) as f32
}

/// Generates `duration_secs` of a pseudo audio source as a sequence of `AudioPacket`s,
/// with clock drift, packet loss, and discontinuities applied.
fn generate(config: &PseudoSourceConfig, duration_secs: f64) -> Vec<AudioPacket> {
    let mut rng = SimpleRng::new(config.seed);
    let actual_rate_hz = config.nominal_rate_hz as f64 * (1.0 + config.drift_ppm / 1_000_000.0);
    let nominal_duration_ns = config.packet_duration_ms as u64 * 1_000_000;
    let total_packets = ((duration_secs * 1000.0) / config.packet_duration_ms as f64).round() as u64;

    let mut packets = Vec::with_capacity(total_packets as usize);
    let mut host_time_ns: u64 = 0;
    // Cumulative-rounding device sample position avoids error accumulation over long runs.
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

        if rng.next_bool_with_probability(config.packet_loss_probability) {
            continue; // Whole packet lost; the aligner should detect this as a gap.
        }

        let discontinuity = rng.next_bool_with_probability(config.discontinuity_probability);
        if discontinuity && samples.len() > 4 {
            // Mimic the OS reporting a discontinuity: drop the tail of the packet, so
            // the actual frame count comes in short of what was expected.
            let drop = (samples.len() / 3).max(1);
            samples.truncate(samples.len().saturating_sub(drop));
        }

        packets.push(AudioPacket {
            host_time_ns: packet_start_ns,
            nominal_duration_ns,
            samples,
            discontinuity,
        });
    }

    packets
}

const NOMINAL_RATE_HZ: u32 = 48_000;

struct ScenarioConfig {
    duration_secs: f64,
    self_drift_ppm: f64,
    remote_drift_ppm: f64,
    self_packet_ms: u32,
    remote_packet_ms: u32,
    packet_loss_probability: f64,
    discontinuity_probability: f64,
    sync_pulse_interval_secs: f64,
    sync_pulse_duration_ms: f64,
    seed: u64,
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

struct SimulationResult {
    length_diff_frames: i64,
    sync_lag_ms_max_abs: f64,
    self_gaps_detected: u64,
    self_hard_jumps: u64,
    realtime_speedup_factor: f64,
}

fn run_simulation(config: &ScenarioConfig) -> SimulationResult {
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
        seed: config.seed ^ 0xa5a5_a5a5_a5a5_a5a5,
    };

    let self_packets = generate(&self_source_config, config.duration_secs);
    let remote_packets = generate(&remote_source_config, config.duration_secs);

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
    let self_track = self_aligner.into_output();
    let remote_track = remote_aligner.into_output();

    let pulse_period_samples =
        (config.sync_pulse_interval_secs * NOMINAL_RATE_HZ as f64).round() as usize;
    let window_radius = (0.05 * NOMINAL_RATE_HZ as f64).round() as usize; // +-50ms
    let max_lag = (0.15 * NOMINAL_RATE_HZ as f64).round() as i64; // search up to 150ms

    let mut sync_lag_ms_max_abs = 0.0f64;
    let mut center = pulse_period_samples; // skip the first period to avoid edge effects
    while center < self_track.len() && center < remote_track.len() {
        if let Some((lag, _score)) =
            xcorr::measure_lag_at(&self_track, &remote_track, center, window_radius, max_lag)
        {
            let lag_ms = (lag as f64 / NOMINAL_RATE_HZ as f64 * 1000.0).abs();
            sync_lag_ms_max_abs = sync_lag_ms_max_abs.max(lag_ms);
        }
        center += pulse_period_samples;
    }

    let wall_clock_secs = wall_clock_start.elapsed().as_secs_f64();
    let realtime_speedup_factor = if wall_clock_secs > 0.0 {
        config.duration_secs / wall_clock_secs
    } else {
        f64::INFINITY
    };

    SimulationResult {
        length_diff_frames: self_track.len() as i64 - remote_track.len() as i64,
        sync_lag_ms_max_abs,
        self_gaps_detected: self_stats.gaps_detected,
        self_hard_jumps: self_stats.hard_jumps,
        realtime_speedup_factor,
    }
}

fn assert_correctness(config: &ScenarioConfig) {
    let result = run_simulation(config);

    assert_eq!(
        result.length_diff_frames, 0,
        "self/remote track lengths must match exactly"
    );
    assert!(
        result.sync_lag_ms_max_abs <= 100.0,
        "sync error must stay within 100ms (design.md §3.2): {}ms",
        result.sync_lag_ms_max_abs
    );
    // Confirm packet loss/discontinuity were actually injected (otherwise this test
    // wouldn't be exercising anything interesting).
    assert!(result.self_gaps_detected > 0);
    assert!(result.self_hard_jumps > 0);
}

#[test]
fn short_scenario_meets_acceptance_criteria() {
    let config = ScenarioConfig {
        duration_secs: 120.0,
        ..ScenarioConfig::default()
    };
    assert_correctness(&config);
}

#[test]
fn no_drift_no_loss_scenario_is_still_length_exact_and_in_sync() {
    // Even with zero drift/loss, differing callback periods (10ms vs 20ms) alone
    // shouldn't break alignment.
    let config = ScenarioConfig {
        duration_secs: 60.0,
        self_drift_ppm: 0.0,
        remote_drift_ppm: 0.0,
        packet_loss_probability: 0.0,
        discontinuity_probability: 0.0,
        ..ScenarioConfig::default()
    };
    let result = run_simulation(&config);
    assert_eq!(result.length_diff_frames, 0);
    assert_eq!(result.self_gaps_detected, 0);
    assert_eq!(result.self_hard_jumps, 0);
    assert!(result.sync_lag_ms_max_abs <= 20.0);
}

#[test]
#[ignore = "full 2-hour scenario; run manually in release mode"]
fn full_two_hour_scenario_meets_acceptance_criteria() {
    let config = ScenarioConfig::default();
    assert_correctness(&config);
}

#[test]
#[ignore = "run in release mode: cargo test -p audio-timeline --release --test simulation -- --ignored realtime_speedup"]
fn realtime_speedup_is_at_least_10x_in_release_build() {
    let config = ScenarioConfig {
        duration_secs: 600.0,
        ..ScenarioConfig::default()
    };
    let result = run_simulation(&config);
    assert!(
        result.realtime_speedup_factor >= 10.0,
        "must process at least 10x realtime in release mode: {}x",
        result.realtime_speedup_factor
    );
}
