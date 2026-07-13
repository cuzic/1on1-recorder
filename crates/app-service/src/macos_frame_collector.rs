//! The macOS conversion layer: turns `capture-macos`'s raw,
//! host-clock-timestamped `FrameSinkEvent`s (forwarded by
//! `MacosSupervisor::set_frame_sink`) into `recorder_domain::CapturedFrame`s,
//! ready for stage 1's `timeline_adapter::align_track` — mirrors
//! `windows_frame_collector.rs` exactly in shape, differing only in the
//! timestamp conversion (already nanoseconds here, no `*100` needed — see
//! `capture_macos::timestamp`'s module doc comment for why).

use std::collections::HashMap;
use std::sync::Mutex;

use capture_api::rebinding::BindingKind;
use capture_macos::CapturedFrameRecord;
use crossbeam_channel::Receiver;
use recorder_domain::{CapturedFrame, TrackKind};

use crate::macos_supervisor::FrameSinkEvent;

/// Identical shape to `windows_frame_collector::LevelSnapshot` — see that type's
/// doc comment.
#[derive(Debug, Clone, Copy, Default)]
pub struct LevelSnapshot {
    pub self_rms: f32,
    pub self_peak: f32,
    pub remote_rms: f32,
    pub remote_peak: f32,
}

fn rms_and_peak(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    let rms = (sum_sq / samples.len() as f32).sqrt();
    let peak = samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    (rms, peak)
}

/// Same mapping as `windows_frame_collector::track_for_binding`: Microphone is
/// always `Self`, EndpointLoopback is always `Remote`. `ProcessLoopback` has no
/// track mapping yet — `macos_supervisor` doesn't produce it today either (see
/// that module's `unfiltered_display_filter`).
pub fn track_for_binding(binding: BindingKind) -> Option<TrackKind> {
    match binding {
        BindingKind::Microphone => Some(TrackKind::SelfMic),
        BindingKind::EndpointLoopback => Some(TrackKind::RemoteAudio),
        BindingKind::ProcessLoopback => None,
    }
}

/// `host_time_ns` comes straight from `capture_time_ns` — already nanoseconds
/// (see `capture_macos::timestamp::cmtime_to_ns`), unlike Windows's
/// `capture_qpc_100ns` which needs a `*100` step. `source_time_ns`'s derivation
/// mirrors `windows_frame_collector::to_captured_frame`'s exactly, modulo the
/// caveat that `device_position_frames` here is accumulated locally rather than
/// hardware-reported (see `capture_macos::frame::CapturedFrameRecord`'s doc
/// comment) — it's still a valid elapsed-sample-count-based diagnostic value,
/// just not a hardware-verified one.
fn to_captured_frame(
    track: TrackKind,
    record: &CapturedFrameRecord,
    samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
) -> CapturedFrame {
    let source_time_ns = if sample_rate > 0 {
        Some(((record.device_position_frames as f64 / sample_rate as f64) * 1_000_000_000.0) as u64)
    } else {
        None
    };
    CapturedFrame {
        track,
        host_time_ns: record.capture_time_ns,
        source_time_ns,
        sample_rate,
        channels,
        samples,
        discontinuity: record.discontinuity,
    }
}

/// Identical shape to `windows_frame_collector::CollectedFrames`.
pub struct CollectedFrames {
    pub self_frames: Vec<CapturedFrame>,
    pub remote_frames: Vec<CapturedFrame>,
    pub self_nominal_frame_interval_ns: u64,
    pub remote_nominal_frame_interval_ns: u64,
}

/// Drains `rx` until it disconnects, converting and sorting every frame into its
/// track — see `windows_frame_collector::collect_frames`'s doc comment for the
/// full rationale (in-memory buffering scope, `level_sink` update timing); it
/// applies identically here.
///
/// Note: `self_nominal_frame_interval_ns`/`remote_nominal_frame_interval_ns` are
/// currently always `0` on macOS — see `capture-macos`'s README "What's not here
/// yet" section for why (ScreenCaptureKit has no WASAPI-`GetDevicePeriod`
/// equivalent to query it from).
pub fn collect_frames(
    rx: &Receiver<FrameSinkEvent>,
    level_sink: Option<&Mutex<LevelSnapshot>>,
) -> CollectedFrames {
    let mut self_frames = Vec::new();
    let mut remote_frames = Vec::new();
    let mut formats: HashMap<BindingKind, (u32, u16)> = HashMap::new();
    let mut intervals: HashMap<BindingKind, u64> = HashMap::new();

    while let Ok(event) = rx.recv() {
        match event {
            FrameSinkEvent::StreamStarted {
                binding,
                sample_rate,
                channels,
                nominal_frame_interval_ns,
            } => {
                formats.insert(binding, (sample_rate, channels));
                intervals.insert(binding, nominal_frame_interval_ns);
            }
            FrameSinkEvent::Frame { record, samples } => {
                let Some(track) = track_for_binding(record.stream) else {
                    continue;
                };
                let (sample_rate, channels) =
                    formats.get(&record.stream).copied().unwrap_or((48_000, 1));
                let frame = to_captured_frame(track, &record, samples, sample_rate, channels);

                if let Some(sink) = level_sink {
                    let (rms, peak) = rms_and_peak(&frame.samples);
                    let mut snapshot = sink.lock().unwrap();
                    match track {
                        TrackKind::SelfMic => (snapshot.self_rms, snapshot.self_peak) = (rms, peak),
                        TrackKind::RemoteAudio => {
                            (snapshot.remote_rms, snapshot.remote_peak) = (rms, peak)
                        }
                    }
                }

                match track {
                    TrackKind::SelfMic => self_frames.push(frame),
                    TrackKind::RemoteAudio => remote_frames.push(frame),
                }
            }
        }
    }

    CollectedFrames {
        self_frames,
        remote_frames,
        self_nominal_frame_interval_ns: intervals
            .get(&BindingKind::Microphone)
            .copied()
            .unwrap_or(0),
        remote_nominal_frame_interval_ns: intervals
            .get(&BindingKind::EndpointLoopback)
            .copied()
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_for_binding_maps_phase_1a_bindings_and_excludes_process_loopback() {
        assert_eq!(
            track_for_binding(BindingKind::Microphone),
            Some(TrackKind::SelfMic)
        );
        assert_eq!(
            track_for_binding(BindingKind::EndpointLoopback),
            Some(TrackKind::RemoteAudio)
        );
        assert_eq!(track_for_binding(BindingKind::ProcessLoopback), None);
    }

    #[test]
    fn to_captured_frame_passes_through_capture_time_ns_unchanged() {
        let record = CapturedFrameRecord::from_raw(
            BindingKind::Microphone,
            1,
            1_234_500,
            0,
            960,
            false,
            false,
            0,
            None,
        );
        let frame = to_captured_frame(TrackKind::SelfMic, &record, vec![0.0; 960], 48_000, 1);
        assert_eq!(frame.host_time_ns, 1_234_500);
    }

    #[test]
    fn to_captured_frame_derives_source_time_from_device_position() {
        let record = CapturedFrameRecord::from_raw(
            BindingKind::Microphone,
            1,
            0,
            48_000,
            960,
            false,
            false,
            0,
            None,
        );
        let frame = to_captured_frame(TrackKind::SelfMic, &record, vec![0.0; 960], 48_000, 1);
        assert_eq!(frame.source_time_ns, Some(1_000_000_000));
    }

    #[test]
    fn rms_and_peak_of_silence_is_zero() {
        assert_eq!(rms_and_peak(&[0.0; 100]), (0.0, 0.0));
    }

    #[test]
    fn rms_and_peak_of_a_constant_amplitude_signal() {
        let (rms, peak) = rms_and_peak(&[0.5, -0.5, 0.5, -0.5]);
        assert!((rms - 0.5).abs() < 1e-6);
        assert!((peak - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rms_and_peak_of_empty_samples_is_zero() {
        assert_eq!(rms_and_peak(&[]), (0.0, 0.0));
    }
}
