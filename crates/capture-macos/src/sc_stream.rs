//! The `CaptureStream` implementation wrapping one shared `SCStream` that delivers
//! both `SCStreamOutputType::Microphone` and `SCStreamOutputType::Audio` (see
//! `lib.rs`'s module doc comment for why this is one stream/one thread rather than
//! `capture-windows`'s one-stream-per-binding split).
//!
//! Unlike WASAPI's poll-style capture loop (`capture-windows::capture_loop`,
//! blocking on `WaitForMultipleObjects` and draining packets each wake),
//! ScreenCaptureKit is callback-driven: Apple's GCD invokes
//! `SCStreamOutputTrait::did_output_sample_buffer` on its own dispatch queue
//! whenever a buffer is ready. `run()`'s job is therefore just to start the stream,
//! register the handler, then block the calling thread on `stop` until told to
//! shut down — all the actual frame delivery happens on GCD's own thread(s), not
//! the thread `run()` executes on.
//!
//! **Not yet verified against a real build** — see `lib.rs`'s top-level doc comment.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use capture_api::rebinding::BindingKind;
use screencapturekit::cm::{CMSampleBuffer, CMSampleBufferExt};
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::SCStream;

use crate::error::{CaptureError, TccService};
use crate::frame::CapturedFrameRecord;
use crate::permissions::classify_stream_start_error;
use crate::timestamp::{cmtime_to_ns, CMTimeLike};
use crate::{CaptureEvent, CaptureExit, CaptureStream, StopSignal};

/// Which outputs to enable on the shared `SCStream`. Task 3 (mic-only) uses
/// `{ microphone: true, system_audio: false }`; task 4 turns `system_audio` on too.
#[derive(Debug, Clone, Copy)]
pub struct StreamOutputs {
    pub microphone: bool,
    pub system_audio: bool,
}

pub struct ScreenCaptureKitStream {
    filter: SCContentFilter,
    sample_rate: u32,
    channels: u16,
    outputs: StreamOutputs,
    /// `EndpointLoopback` for unfiltered system audio, `ProcessLoopback` for an
    /// app-scoped filter (see `app_filter.rs`) — the binding tag used for the
    /// `.audio` output's frames. The `.microphone` output always tags
    /// `BindingKind::Microphone`.
    system_audio_binding: BindingKind,
    capture_epoch: u64,
    /// CoreAudio device UID to pin the `.microphone` output to, or `None` to use
    /// whatever `SCStreamConfiguration` defaults to (the OS default input) — see
    /// `set_microphone_capture_device_id`'s doc comment in the `screencapturekit`
    /// crate. There is no render/output-device equivalent: ScreenCaptureKit's
    /// `captures_audio` taps the system-wide audio mix, not a specific output
    /// device, so a pinned `render_device_id` has nothing to bind to here.
    microphone_device_id: Option<String>,
}

impl ScreenCaptureKitStream {
    pub fn new(
        filter: SCContentFilter,
        sample_rate: u32,
        channels: u16,
        outputs: StreamOutputs,
        system_audio_binding: BindingKind,
        capture_epoch: u64,
        microphone_device_id: Option<String>,
    ) -> Self {
        Self {
            filter,
            sample_rate,
            channels,
            outputs,
            system_audio_binding,
            capture_epoch,
            microphone_device_id,
        }
    }
}

impl CaptureStream for ScreenCaptureKitStream {
    fn bindings(&self) -> Vec<BindingKind> {
        let mut bindings = Vec::with_capacity(2);
        if self.outputs.microphone {
            bindings.push(BindingKind::Microphone);
        }
        if self.outputs.system_audio {
            bindings.push(self.system_audio_binding);
        }
        bindings
    }

    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, CaptureError> {
        let mut config = SCStreamConfiguration::new()
            .with_captures_audio(self.outputs.system_audio)
            .with_captures_microphone(self.outputs.microphone)
            .with_sample_rate(self.sample_rate as i32)
            .with_channel_count(self.channels as i32);

        if self.outputs.microphone {
            if let Some(device_id) = &self.microphone_device_id {
                config.set_microphone_capture_device_id(device_id);
            }
        }

        let mut stream = SCStream::new(&self.filter, &config);

        if self.outputs.microphone {
            let handler = FrameForwarder::new(
                tx.clone(),
                BindingKind::Microphone,
                self.sample_rate,
                self.capture_epoch,
            );
            stream.add_output_handler(handler, SCStreamOutputType::Microphone);
            tx.send(CaptureEvent::StreamStarted {
                stream: BindingKind::Microphone,
                sample_rate: self.sample_rate,
                channels: self.channels,
                nominal_frame_interval_ns: 0,
            })
            .ok();
        }
        if self.outputs.system_audio {
            let handler = FrameForwarder::new(
                tx.clone(),
                self.system_audio_binding,
                self.sample_rate,
                self.capture_epoch,
            );
            stream.add_output_handler(handler, SCStreamOutputType::Audio);
            tx.send(CaptureEvent::StreamStarted {
                stream: self.system_audio_binding,
                sample_rate: self.sample_rate,
                channels: self.channels,
                nominal_frame_interval_ns: 0,
            })
            .ok();
        }

        stream.start_capture().map_err(|err| {
            // Only one TCC service is meaningfully distinguishable from the start
            // error alone; if both outputs are enabled and the failure turns out to
            // be microphone-specific vs. screen-recording-specific, that nuance is
            // lost here. Default to whichever service this stream needed first
            // (system audio, since it needs Screen & System Audio Recording, the
            // "bigger" of the two grants) — refine once real error shapes are known.
            let service = if self.outputs.system_audio {
                TccService::ScreenAndSystemAudioRecording
            } else {
                TccService::Microphone
            };
            classify_stream_start_error(service, err)
        })?;

        // ScreenCaptureKit is callback-driven (see module doc comment) — this
        // thread's only remaining job is to block until told to stop.
        while !stop.wait_timeout(Duration::from_millis(200)) {}

        let _ = stream.stop_capture();
        Ok(CaptureExit::StoppedByRequest)
    }
}

/// One `SCStreamOutputTrait` implementation per output type, sharing nothing but
/// the channel sender — GCD may invoke `did_output_sample_buffer` concurrently from
/// arbitrary threads (hence `Send + Sync`, and an atomic rather than a plain `u64`
/// for the packet sequence counter).
struct FrameForwarder {
    tx: crossbeam_channel::Sender<CaptureEvent>,
    binding: BindingKind,
    sample_rate: u32,
    capture_epoch: u64,
    packet_seq: Arc<AtomicU64>,
    device_position_frames: Arc<AtomicU64>,
}

impl FrameForwarder {
    fn new(
        tx: crossbeam_channel::Sender<CaptureEvent>,
        binding: BindingKind,
        sample_rate: u32,
        capture_epoch: u64,
    ) -> Self {
        Self {
            tx,
            binding,
            sample_rate,
            capture_epoch,
            packet_seq: Arc::new(AtomicU64::new(0)),
            device_position_frames: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl SCStreamOutputTrait for FrameForwarder {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _of_type: SCStreamOutputType) {
        let Some(buffer_list) = sample.audio_buffer_list() else {
            return;
        };

        let presentation = sample.presentation_timestamp();
        let capture_time_ns = cmtime_to_ns(CMTimeLike {
            value: presentation.value,
            timescale: presentation.timescale,
        })
        .unwrap_or(0);

        let samples = audio_buffer_list_to_f32(&buffer_list);
        let frame_count = if self.sample_rate > 0 {
            (samples.len() as u32).max(1)
        } else {
            samples.len() as u32
        };

        let packet_seq = self.packet_seq.fetch_add(1, Ordering::SeqCst);
        let device_position_frames = self
            .device_position_frames
            .fetch_add(frame_count as u64, Ordering::SeqCst);

        let record = CapturedFrameRecord::from_raw(
            self.binding,
            packet_seq,
            capture_time_ns,
            device_position_frames,
            frame_count,
            false, // discontinuity: ScreenCaptureKit doesn't surface this directly;
            // revisit once SCStreamFrameInfo's status field is confirmed on a real
            // build (it may carry a "buffer dropped" signal usable here).
            false, // silent: same caveat as above.
            self.capture_epoch,
            None,
        );

        let _ = self.tx.send(CaptureEvent::Frame { record, samples });
    }
}

/// Converts ScreenCaptureKit's `AudioBufferList` (raw PCM bytes, expected Float32
/// per `SCStreamConfiguration`'s implicit format) into the flat `Vec<f32>` shape
/// `recorder_domain::CapturedFrame::samples` and `capture-windows`'s
/// `copy_to_f32_vec` both use. **Not yet verified**: assumes the buffer's bytes are
/// native-endian `f32`, interleaved if `channel_count > 1` — needs confirming
/// against the real `AudioBufferList` layout ScreenCaptureKit delivers on first
/// real build (non-interleaved/planar delivery would need a different conversion).
fn audio_buffer_list_to_f32(buffer_list: &screencapturekit::AudioBufferList) -> Vec<f32> {
    let mut samples = Vec::new();
    for buffer in buffer_list {
        let chunks = buffer.data().chunks_exact(4);
        samples.extend(
            chunks.map(|bytes| f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        );
    }
    samples
}
