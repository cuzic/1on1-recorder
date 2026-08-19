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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use capture_api::rebinding::BindingKind;
use screencapturekit::cm::{CMSampleBuffer, CMSampleBufferExt};
use screencapturekit::error::SCError;
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::delegate_trait::SCStreamDelegateTrait;
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
    /// Per-binding epochs, not one shared value — the `.microphone` and `.audio`
    /// outputs share this one `SCStream`/thread, but each is tagged with its own
    /// binding's own epoch (see `MacosSupervisor::reconcile_active_stream`'s doc
    /// comment on why: `decide()` allocates a fresh `StreamEpoch` independently per
    /// binding, so a single stream-wide epoch would only ever match one of the two
    /// bindings' `Running.epoch`, permanently starving the other of accepted
    /// frames). `None` when that output isn't enabled (`outputs.microphone`/
    /// `outputs.system_audio` is `false`) — `run()` only reads the epoch for an
    /// output it's actually turning on.
    microphone_epoch: Option<u64>,
    system_audio_epoch: Option<u64>,
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
        microphone_epoch: Option<u64>,
        system_audio_epoch: Option<u64>,
        microphone_device_id: Option<String>,
    ) -> Self {
        Self {
            filter,
            sample_rate,
            channels,
            outputs,
            system_audio_binding,
            microphone_epoch,
            system_audio_epoch,
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

        // See `StreamErrorDelegate`'s doc comment for why this needs its own
        // owned signal rather than reusing `stop`.
        let stream_dead = Arc::new(StopSignal::new());
        let delegate_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let delegate = StreamErrorDelegate { error: delegate_error.clone(), stream_dead: stream_dead.clone() };
        let mut stream = SCStream::new_with_delegate(&self.filter, &config, delegate);

        if self.outputs.microphone {
            let handler = FrameForwarder::new(
                tx.clone(),
                BindingKind::Microphone,
                self.sample_rate,
                self.channels,
                self.microphone_epoch.expect(
                    "outputs.microphone is true but ScreenCaptureKitStream::new wasn't given a microphone_epoch",
                ),
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
                self.channels,
                self.system_audio_epoch.expect(
                    "outputs.system_audio is true but ScreenCaptureKitStream::new wasn't given a system_audio_epoch",
                ),
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
        // thread's only remaining job is to block until told to stop, whether
        // that's `stop` (the caller, e.g. `reconcile_active_stream`, tearing this
        // stream down to rebuild it) or `stream_dead` (the delegate observing
        // ScreenCaptureKit stopping the stream on its own — see
        // `StreamErrorDelegate`'s doc comment).
        while !stop.wait_timeout(Duration::from_millis(200)) && !stream_dead.is_signaled() {}

        // `stop` takes priority when both happen to be signaled at once (a
        // caller-requested teardown racing a delegate error is treated as the
        // clean stop it was): only report the delegate's error as this run's
        // outcome when the caller never asked to stop at all. Returning `Err`
        // here (rather than `Ok(CaptureExit::DeviceLost)`) matters —
        // `spawn_capture_thread` turns an `Err` into a real
        // `CaptureEvent::StreamError` per binding this stream serves, which is
        // what actually reaches the rebinding FSM; an `Ok` exit only produces
        // `CaptureEvent::StreamStopped`, which `MacosSupervisor::handle_capture_event`
        // treats as informational-only.
        if !stop.is_signaled() && stream_dead.is_signaled() {
            let message = delegate_error.lock().unwrap().take().unwrap_or_else(|| "stream stopped unexpectedly".to_string());
            return Err(CaptureError::ScreenCaptureKit(message));
        }

        let _ = stream.stop_capture();
        Ok(CaptureExit::StoppedByRequest)
    }
}

/// `SCStreamDelegateTrait` implementation that turns Apple's
/// `stream(_:didStopWithError:)` callback — the only way `ScreenCaptureKit`
/// reports the shared stream stopping unexpectedly (captured content going away,
/// screen-recording/microphone permission revoked mid-session, the system tearing
/// the stream down, …) — into something `run()`'s blocking wait loop can observe.
/// Without this, `run()` had no way to detect that kind of stop at all: only a
/// caller-requested `stop_capture` (observed via `stop`, the `StopSignal` `run()`
/// is already given) or a CoreAudio device/default-device change (observed
/// indirectly via `capture-macos::device_watch`) could end a session — a stream
/// dying entirely on its own (no CoreAudio device event involved) would otherwise
/// hang `run()` forever, never feeding the rebinding FSM.
///
/// `error`/`stream_dead` are a separate `Arc<Mutex<Option<String>>>`/`Arc<StopSignal>`
/// pair rather than reusing `run()`'s own `stop: &StopSignal` parameter: that
/// parameter is a borrowed reference tied to `run()`'s call frame, but
/// `SCStream::new_with_delegate` requires `delegate: impl SCStreamDelegateTrait +
/// 'static` — the delegate can outlive this stack frame (it's owned by the
/// `SCStream`/GCD, not `run()`), so it needs its own owned, `'static` signal.
struct StreamErrorDelegate {
    error: Arc<Mutex<Option<String>>>,
    stream_dead: Arc<StopSignal>,
}

impl SCStreamDelegateTrait for StreamErrorDelegate {
    fn did_stop_with_error(&self, error: SCError) {
        *self.error.lock().unwrap() = Some(error.to_string());
        self.stream_dead.signal();
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
    /// Needed to turn `samples.len()` (the total f32 count across *all* channels,
    /// interleaved or not — see `audio_buffer_list_to_f32`'s doc comment) back into a
    /// frame count. Without this, `frame_count` silently over-counts by a factor of
    /// `channels` for any multi-channel stream.
    channels: u16,
    capture_epoch: u64,
    packet_seq: Arc<AtomicU64>,
    device_position_frames: Arc<AtomicU64>,
}

impl FrameForwarder {
    fn new(
        tx: crossbeam_channel::Sender<CaptureEvent>,
        binding: BindingKind,
        sample_rate: u32,
        channels: u16,
        capture_epoch: u64,
    ) -> Self {
        Self {
            tx,
            binding,
            sample_rate,
            channels,
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
        // `samples` holds every channel's samples (see `audio_buffer_list_to_f32`'s
        // doc comment — the total count is `channels * frame_count` whether the
        // buffer list is interleaved or planar), so it must be divided by the
        // channel count to recover the actual frame count. Previously this used
        // `samples.len()` directly, which for any stream with `channels > 1`
        // over-counted frames by that factor — and since `device_position_frames`
        // (below) accumulates `frame_count` and is what
        // `macos_frame_collector::to_captured_frame` divides by `sample_rate` to
        // derive this track's `source_time_ns` (not `host_time_ns`, which comes from
        // `capture_time_ns`/the `CMSampleBuffer` presentation timestamp instead),
        // that inflation made the reported source-time timeline run faster than
        // real time.
        //
        // Residual caveat, not yet resolved: `self.channels` is the value this
        // stream *configured* via `SCStreamConfiguration::with_channel_count`
        // (`run()`, above), not a measurement of what this specific output actually
        // delivers per callback — `with_channel_count` is documented against the
        // `.audio` (system-audio) output, and whether ScreenCaptureKit's
        // `.microphone` output always honors the same channel count hasn't been
        // confirmed against a real build (see this file's and `lib.rs`'s top-level
        // "not yet verified" notes). If it doesn't, this division would be wrong in
        // the opposite direction. Confirm against real ScreenCaptureKit output
        // before trusting this in production.
        let channels = (self.channels as u32).max(1);
        // The trailing `.max(1)` (inherited from before this fix, not newly
        // introduced by it) means an empty buffer still advances
        // `device_position_frames` by 1 rather than 0. Left as-is rather than
        // changed to `unwrap_or(0)`/no floor: whether ScreenCaptureKit's callback
        // can ever legitimately fire with a non-null but zero-sample buffer list
        // isn't confirmed, and changing this without a real build to check against
        // risks trading one unverified assumption for another.
        let frame_count = ((samples.len() as u32) / channels).max(1);

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

#[cfg(test)]
mod tests {
    use super::*;

    // `StreamErrorDelegate` and `SCError` are both plain Rust types with no
    // ScreenCaptureKit/Swift bridge involved in their construction (unlike most of
    // this crate — see `lib.rs`'s top-level doc comment on why the crate as a whole
    // has never been compiled: the `screencapturekit` crate's own build script
    // shells out to `swiftc`, which this repo's Linux dev environment doesn't have).
    // This is deliberately the one piece of `sc_stream.rs` exercised directly,
    // covering the "(未検証)" gap `docs/adr/0001-*.md` flags: whether
    // `did_stop_with_error` actually records the message and signals `stream_dead`
    // the way `run()`'s wait loop (177-192 in this file) assumes it does.
    #[test]
    fn did_stop_with_error_records_the_message_and_signals_stream_dead() {
        let error_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let stream_dead = Arc::new(StopSignal::new());
        let delegate = StreamErrorDelegate {
            error: error_slot.clone(),
            stream_dead: stream_dead.clone(),
        };

        assert!(!stream_dead.is_signaled());

        delegate.did_stop_with_error(SCError::stream_error("simulated stop"));

        assert!(stream_dead.is_signaled());
        assert_eq!(
            error_slot.lock().unwrap().as_deref(),
            Some(SCError::stream_error("simulated stop").to_string().as_str()),
        );
    }

    #[test]
    fn did_stop_with_error_overwrites_a_previously_recorded_message() {
        // `run()` only ever reads `error` once, after observing `stream_dead`, so
        // this isn't exercised in practice today — but `did_stop_with_error` has no
        // guard against being invoked more than once, so document the actual
        // "last write wins" behavior rather than leaving it merely assumed.
        let error_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let stream_dead = Arc::new(StopSignal::new());
        let delegate = StreamErrorDelegate {
            error: error_slot.clone(),
            stream_dead: stream_dead.clone(),
        };

        delegate.did_stop_with_error(SCError::stream_error("first"));
        delegate.did_stop_with_error(SCError::stream_error("second"));

        assert_eq!(
            error_slot.lock().unwrap().as_deref(),
            Some(SCError::stream_error("second").to_string().as_str()),
        );
    }
}
