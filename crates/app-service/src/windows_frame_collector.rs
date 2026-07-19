//! Task #10's conversion layer: turns `capture-windows`'s raw, QPC-timestamped
//! `FrameSinkEvent`s (forwarded by `WindowsSupervisor::set_frame_sink`) into
//! `recorder_domain::CapturedFrame`s (nanosecond timestamps), ready for stage 1's
//! `timeline_adapter::align_track` — the same conversion any capture backend needs
//! to feed this crate's OS-independent pipeline.

use std::collections::HashMap;
use std::sync::Mutex;

use capture_api::rebinding::BindingKind;
use capture_windows::CapturedFrameRecord;
use crossbeam_channel::Receiver;
use recorder_domain::{CapturedFrame, TrackKind};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender;

use crate::windows_supervisor::FrameSinkEvent;

/// The most recent per-track level, for a UI meter — updated on every `Frame`
/// event `collect_frames` sees, independent of (and much cheaper than) the
/// batch collection it's already doing. `rms`/`peak` are in the same `[0.0, 1.0]`-ish
/// range as the raw `f32` samples themselves (no dB conversion).
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

/// Phase 1A's fixed capture-windows setup: Microphone is always the `Self` track,
/// EndpointLoopback is always `Remote`. Process loopback isn't part of Phase 1A
/// (see `capture-windows`'s own README) so it has no track mapping.
pub fn track_for_binding(binding: BindingKind) -> Option<TrackKind> {
    match binding {
        BindingKind::Microphone => Some(TrackKind::SelfMic),
        BindingKind::EndpointLoopback => Some(TrackKind::RemoteAudio),
        BindingKind::ProcessLoopback => None,
    }
}

/// `host_time_ns` comes straight from `capture_qpc_100ns` (already normalized to
/// 100ns units across the machine-wide QPC clock domain by
/// `capture_windows::timestamp::QpcClock`) — just converted to nanoseconds.
///
/// `source_time_ns` is the *device's own* notion of elapsed time, derived from
/// `device_position_frames` (the device's internal cumulative sample counter) at
/// this stream's sample rate — a second, independent clock distinct from the host's
/// QPC clock, useful for diagnosing how far the two have diverged. It is not used
/// for alignment (`host_time_ns` is — see `audio_timeline::TimelineAligner`).
fn to_captured_frame(track: TrackKind, record: &CapturedFrameRecord, samples: Vec<f32>, sample_rate: u32, channels: u16) -> CapturedFrame {
    let source_time_ns = if sample_rate > 0 {
        Some(((record.device_position_frames as f64 / sample_rate as f64) * 1_000_000_000.0) as u64)
    } else {
        None
    };
    CapturedFrame {
        track,
        host_time_ns: record.capture_qpc_100ns.saturating_mul(100),
        source_time_ns,
        sample_rate,
        channels,
        samples,
        discontinuity: record.discontinuity,
    }
}

pub struct CollectedFrames {
    pub self_frames: Vec<CapturedFrame>,
    pub remote_frames: Vec<CapturedFrame>,
    /// `IAudioClient::GetDevicePeriod`'s value for each binding, needed by
    /// `timeline_adapter::align_track` — 0 if that binding's `StreamStarted` was
    /// never observed (e.g. it never actually started).
    pub self_nominal_frame_interval_ns: u64,
    pub remote_nominal_frame_interval_ns: u64,
}

/// Drains `rx` until it disconnects (i.e. the `WindowsSupervisor` that owns the
/// sending half was dropped, normally after `run_until_shutdown` returns),
/// converting and sorting every frame into its track. `level_sink`, if given, is
/// updated with each track's latest RMS/peak as frames arrive — cheap enough to
/// compute per-frame that it doesn't need its own consumer (which would recreate
/// the competing-consumer race `FrameSinkEvent` itself exists to avoid; see
/// `windows_supervisor`'s doc comment).
///
/// `stt_sink`, if given, is a second side channel of the same shape: every
/// frame's raw PCM (plus its track and sample rate) is forwarded there too, so
/// `live_transcription` can stream audio into an STT provider as capture
/// happens, without this function itself knowing anything about STT. This
/// function runs synchronously on its own `collector` thread (see
/// `windows_session::run_capture_blocking`), draining `rx` as fast as
/// `WindowsSupervisor` produces `FrameSinkEvent`s — an `.await` here isn't even
/// possible, and blocking on `stt_sink` (an async mpsc `Sender`) would stall that
/// drain, backing up `rx` behind it and delaying capture itself (task #86). So a
/// slow/stalled STT consumer (reconnect backoff, a full provider send queue, task
/// #82/#83) must never be able to block this loop: `stt_sink` is a *bounded*
/// channel and this uses `try_send`, not `send` — a full channel just drops that
/// chunk (see `try_send`'s `Full` arm below) rather than blocking. A `Closed`
/// error (the receiving end was dropped, e.g. no STT session ever started) is
/// silently ignored — same "best-effort side channel" spirit as `level_sink`.
///
/// Buffers an entire session's samples in memory — acceptable for proving real
/// `capture-windows` audio flows through the exact pipeline stage 1 validated with
/// `pseudo_source`, but not how a long-running recording should work in
/// production. Segmenting incrementally as capture progresses (bounding memory,
/// uploading while still recording) is stage 3's job (task #11), not this
/// function's.
pub fn collect_frames(
    rx: &Receiver<FrameSinkEvent>,
    level_sink: Option<&Mutex<LevelSnapshot>>,
    stt_sink: Option<&Sender<(TrackKind, Vec<f32>, u32)>>,
) -> CollectedFrames {
    let mut self_frames = Vec::new();
    let mut remote_frames = Vec::new();
    let mut formats: HashMap<BindingKind, (u32, u16)> = HashMap::new();
    let mut intervals: HashMap<BindingKind, u64> = HashMap::new();
    // Counts `stt_sink` drops so the warning below can be rate-limited (logging
    // every single drop would itself be a log-spam source once the STT side is
    // stuck for more than an instant — see `stt_sink`'s doc comment above).
    let mut stt_sink_drops: u64 = 0;

    while let Ok(event) = rx.recv() {
        match event {
            FrameSinkEvent::StreamStarted { binding, sample_rate, channels, nominal_frame_interval_ns } => {
                formats.insert(binding, (sample_rate, channels));
                intervals.insert(binding, nominal_frame_interval_ns);
            }
            FrameSinkEvent::Frame { record, samples } => {
                let Some(track) = track_for_binding(record.stream) else { continue };
                // Falls back to 48kHz mono if a frame somehow arrives before its
                // stream's own StreamStarted (shouldn't happen — capture-windows
                // always sends StreamStarted before the first Frame — but a wrong
                // guess here is better than a panic).
                let (sample_rate, channels) = formats.get(&record.stream).copied().unwrap_or((48_000, 1));
                let frame = to_captured_frame(track, &record, samples, sample_rate, channels);

                if let Some(sink) = level_sink {
                    let (rms, peak) = rms_and_peak(&frame.samples);
                    let mut snapshot = sink.lock().unwrap();
                    match track {
                        TrackKind::SelfMic => (snapshot.self_rms, snapshot.self_peak) = (rms, peak),
                        TrackKind::RemoteAudio => (snapshot.remote_rms, snapshot.remote_peak) = (rms, peak),
                    }
                }

                if let Some(sink) = stt_sink {
                    match sink.try_send((track, frame.samples.clone(), sample_rate)) {
                        Ok(()) => {}
                        Err(TrySendError::Closed(_)) => {}
                        Err(TrySendError::Full(_)) => {
                            stt_sink_drops += 1;
                            // First drop logs immediately (so a stuck STT side shows up
                            // right away), then every 100th after that — frequent enough
                            // to see the problem is ongoing, not so frequent it becomes
                            // its own log-spam problem on a long stall.
                            if stt_sink_drops == 1 || stt_sink_drops.is_multiple_of(100) {
                                tracing::warn!(stt_sink_drops, ?track, "live transcription channel full, dropping PCM chunk (STT falling behind capture)");
                            }
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
        self_nominal_frame_interval_ns: intervals.get(&BindingKind::Microphone).copied().unwrap_or(0),
        remote_nominal_frame_interval_ns: intervals.get(&BindingKind::EndpointLoopback).copied().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_for_binding_maps_phase_1a_bindings_and_excludes_process_loopback() {
        assert_eq!(track_for_binding(BindingKind::Microphone), Some(TrackKind::SelfMic));
        assert_eq!(track_for_binding(BindingKind::EndpointLoopback), Some(TrackKind::RemoteAudio));
        assert_eq!(track_for_binding(BindingKind::ProcessLoopback), None);
    }

    #[test]
    fn to_captured_frame_converts_qpc_100ns_to_nanoseconds() {
        let record = CapturedFrameRecord::from_raw(BindingKind::Microphone, 1, 0, 0, 0, 12_345, 960, 0, 0, None);
        let frame = to_captured_frame(TrackKind::SelfMic, &record, vec![0.0; 960], 48_000, 1);
        assert_eq!(frame.host_time_ns, 1_234_500);
    }

    #[test]
    fn to_captured_frame_derives_source_time_from_device_position() {
        let record = CapturedFrameRecord::from_raw(BindingKind::Microphone, 1, 0, 0, 48_000, 0, 960, 0, 0, None);
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

    fn frame_event(binding: BindingKind, packet_seq: u64) -> FrameSinkEvent {
        let record = CapturedFrameRecord::from_raw(binding, packet_seq, packet_seq, 0, 0, packet_seq * 1_000, 960, 0, 0, None);
        FrameSinkEvent::Frame { record, samples: vec![0.0; 960] }
    }

    /// task #86: a full `stt_sink` must drop the overflowing chunk via `try_send`
    /// rather than block `collect_frames` — otherwise a stalled STT consumer would
    /// delay draining `rx`, i.e. delay capture itself (see `collect_frames`'s doc
    /// comment).
    #[test]
    fn collect_frames_drops_stt_sink_overflow_without_blocking_collection() {
        let (frame_tx, frame_rx) = crossbeam_channel::unbounded();
        let (stt_tx, mut stt_rx) = tokio::sync::mpsc::channel(1);

        frame_tx
            .send(FrameSinkEvent::StreamStarted { binding: BindingKind::Microphone, sample_rate: 48_000, channels: 1, nominal_frame_interval_ns: 10_000_000 })
            .unwrap();
        for packet_seq in 0..3 {
            frame_tx.send(frame_event(BindingKind::Microphone, packet_seq)).unwrap();
        }
        drop(frame_tx); // lets collect_frames' `while let Ok(event) = rx.recv()` end

        let collected = collect_frames(&frame_rx, None, Some(&stt_tx));

        // All three frames are still in the batch collection — dropping from
        // `stt_sink` must not lose anything from `self_frames`/`remote_frames`.
        assert_eq!(collected.self_frames.len(), 3);

        // Capacity 1 and nothing was reading concurrently while collect_frames ran,
        // so only the first of the three chunks made it through; the other two hit
        // `try_send`'s `Full` arm and were dropped instead of blocking.
        assert!(stt_rx.try_recv().is_ok());
        assert!(stt_rx.try_recv().is_err());
    }

    /// Companion to the overflow test above: with a channel that's never full,
    /// every chunk reaches `stt_sink` unchanged (no regression from switching
    /// `send` to `try_send`).
    #[test]
    fn collect_frames_forwards_every_chunk_when_stt_sink_has_room() {
        let (frame_tx, frame_rx) = crossbeam_channel::unbounded();
        let (stt_tx, mut stt_rx) = tokio::sync::mpsc::channel(8);

        frame_tx
            .send(FrameSinkEvent::StreamStarted { binding: BindingKind::Microphone, sample_rate: 48_000, channels: 1, nominal_frame_interval_ns: 10_000_000 })
            .unwrap();
        for packet_seq in 0..3 {
            frame_tx.send(frame_event(BindingKind::Microphone, packet_seq)).unwrap();
        }
        drop(frame_tx);

        collect_frames(&frame_rx, None, Some(&stt_tx));

        for _ in 0..3 {
            assert!(stt_rx.try_recv().is_ok());
        }
        assert!(stt_rx.try_recv().is_err());
    }
}
