//! WASAPI-backed audio capture for Windows: microphone and system-audio (endpoint
//! loopback) capture, with QPC-based timestamps precise enough to feed
//! `audio-timeline`, and resilience to device changes via `capture-api`'s rebinding
//! state machine.
//!
//! Ported from `spikes/spike-01-wasapi-dual-capture` and `spikes/spike-common`, which
//! validated this against real Windows hardware (see the project's spike-plan.md).
//! Process loopback (capturing a specific application's audio only) is not ported yet
//! — Phase 1A only needs microphone + endpoint loopback; process loopback follows in
//! a later phase.

pub mod capture_loop;
pub mod com_guard;
pub mod device_select;
pub mod device_watch;
pub mod error;
pub mod frame;
pub mod loopback_stream;
pub mod mic_stream;
pub mod mmcss;
pub mod timestamp;
pub mod wasapi_common;

pub use capture_loop::run_capture_loop;
pub use error::CaptureError;
pub use frame::CapturedFrameRecord;

use capture_api::rebinding::BindingKind;
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::Foundation::HANDLE;

/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT. Unlike KSDATAFORMAT_SUBTYPE_PCM, the `windows`
/// crate's generated bindings don't include this GUID, so it's hardcoded here from
/// ks.h/mmreg.h.
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

pub enum CaptureEvent {
    Frame {
        record: CapturedFrameRecord,
        samples: Vec<f32>,
    },
    StreamStarted {
        stream: BindingKind,
        format: AudioFormatInfo,
        qpc_freq_hz: u64,
        /// The endpoint actually resolved (`IMMDevice::GetId()`), so that resolving
        /// "default" can still be verified after the fact.
        device_id: String,
        device_friendly_name: String,
    },
    StreamError {
        stream: BindingKind,
        error: String,
    },
    /// `mmcss_applied`: whether this stream's capture thread was successfully
    /// registered with MMCSS.
    StreamStopped {
        stream: BindingKind,
        exit: CaptureExit,
        mmcss_applied: bool,
    },
    /// `IAudioSessionEvents::OnSessionDisconnected` was observed. `reason_raw` is the
    /// raw `AudioSessionDisconnectReason` value (not translated, since COM types
    /// aren't carried across threads directly).
    SessionDisconnected {
        stream: BindingKind,
        reason_raw: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureExit {
    StoppedByRequest,
    DeviceLost,
}

/// A safely-interpreted `WAVEFORMATEXTENSIBLE`.
#[derive(Debug, Clone)]
pub struct AudioFormatInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub is_float: bool,
    /// `WAVEFORMATEX::wFormatTag` (e.g. `WAVE_FORMAT_PCM`, `WAVE_FORMAT_IEEE_FLOAT`,
    /// `WAVE_FORMAT_EXTENSIBLE`).
    pub format_tag: u16,
    /// The SubFormat GUID, when `wFormatTag == WAVE_FORMAT_EXTENSIBLE`.
    pub sub_format: Option<windows::core::GUID>,
    pub block_align: u16,
    /// `WAVEFORMATEXTENSIBLE::Samples.wValidBitsPerSample`.
    pub valid_bits_per_sample: u16,
    /// `WAVEFORMATEXTENSIBLE::dwChannelMask`.
    pub channel_mask: u32,
    pub bytes_per_sample: u16,
}

impl AudioFormatInfo {
    /// Builds from either `GetMixFormat`'s result or a fixed format, reinterpreting as
    /// `WAVEFORMATEXTENSIBLE` when `wFormatTag` indicates it.
    pub fn from_waveformatex(wfx: &WAVEFORMATEX) -> Self {
        let block_align = wfx.nBlockAlign;
        let bytes_per_sample = if wfx.nChannels > 0 {
            block_align / wfx.nChannels
        } else {
            wfx.wBitsPerSample / 8
        };

        // Only reinterpret as WAVEFORMATEXTENSIBLE once cbSize confirms there's
        // actually room for its extra fields; a too-small cbSize (a malformed format
        // description) falls back to the non-extensible interpretation instead of
        // reading out of bounds.
        let extensible_extra_size =
            std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>();
        if wfx.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE
            && wfx.cbSize as usize >= extensible_extra_size
        {
            // WAVEFORMATEX shares its layout with WAVEFORMATEXTENSIBLE's leading
            // `Format` field, so reinterpreting the same memory is safe once cbSize
            // has been checked. The original WAVEFORMATEX isn't guaranteed 4-byte
            // aligned, so read_unaligned onto the stack rather than taking a
            // reference (a reference to a field of a packed struct is rejected, E0793).
            let ext = unsafe {
                std::ptr::read_unaligned(wfx as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE)
            };
            let valid_bits_per_sample = unsafe { ext.Samples.wValidBitsPerSample };
            // WAVEFORMATEXTENSIBLE is a packed struct, so no reference to any of its
            // fields can be taken (even to a copy); copy into a local first.
            let sub_format = ext.SubFormat;
            Self {
                sample_rate: wfx.nSamplesPerSec,
                channels: wfx.nChannels,
                bits_per_sample: wfx.wBitsPerSample,
                is_float: sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
                format_tag: wfx.wFormatTag,
                sub_format: Some(sub_format),
                block_align,
                valid_bits_per_sample,
                channel_mask: ext.dwChannelMask,
                bytes_per_sample,
            }
        } else {
            Self {
                sample_rate: wfx.nSamplesPerSec,
                channels: wfx.nChannels,
                bits_per_sample: wfx.wBitsPerSample,
                is_float: wfx.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT,
                format_tag: wfx.wFormatTag,
                sub_format: None,
                block_align,
                valid_bits_per_sample: wfx.wBitsPerSample,
                channel_mask: 0,
                bytes_per_sample,
            }
        }
    }
}

/// RAII wrapper around the `*mut WAVEFORMATEX` returned by `IAudioClient::GetMixFormat`.
/// The caller is responsible for freeing it with `CoTaskMemFree`.
pub struct WaveFormatBox {
    ptr: *mut WAVEFORMATEX,
}

impl WaveFormatBox {
    pub fn from_raw(ptr: *mut WAVEFORMATEX) -> Self {
        Self { ptr }
    }

    pub fn as_ref(&self) -> &WAVEFORMATEX {
        unsafe { &*self.ptr }
    }
}

impl Drop for WaveFormatBox {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.ptr as *const _)) };
    }
}

/// Stop notification via a manual-reset Win32 event object. The capture loop waits on
/// `[audio_ready_event, stop_event]` simultaneously via `WaitForMultipleObjects`, so it
/// can exit as soon as this is signaled.
pub struct StopSignal {
    event: HANDLE,
}

// HANDLE (`*mut c_void`) has neither Send nor Sync by default, so sharing an
// `Arc<StopSignal>` across threads wouldn't compile without this. SetEvent/CloseHandle/
// WaitForMultipleObjects can all safely be called from any thread, so this is sound.
unsafe impl Send for StopSignal {}
unsafe impl Sync for StopSignal {}

impl StopSignal {
    pub fn new() -> windows::core::Result<Self> {
        // manual_reset=true, initial_state=false, unnamed event.
        let event = unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None)? };
        Ok(Self { event })
    }

    /// Signals the event; every thread waiting on [`handle`](Self::handle) wakes
    /// immediately.
    pub fn signal(&self) -> windows::core::Result<()> {
        unsafe { windows::Win32::System::Threading::SetEvent(self.event) }
    }

    pub fn handle(&self) -> HANDLE {
        self.event
    }
}

impl Drop for StopSignal {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.event) };
    }
}

pub trait CaptureStream: Send {
    fn stream_id(&self) -> BindingKind;

    /// Blocks the calling thread, continuing capture until `stop` is signaled or an
    /// unrecoverable error occurs.
    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, CaptureError>;
}

/// The return value of the `JoinHandle` produced by [`spawn_capture_thread`]. The
/// caller should treat this — not `CaptureEvent::StreamStopped` delivered over the
/// shared channel — as the source of truth for rebinding decisions.
pub enum CaptureThreadOutcome {
    Stopped { exit: CaptureExit, mmcss_applied: bool },
    Errored { error: CaptureError, mmcss_applied: bool },
}

pub fn spawn_capture_thread(
    stream: Box<dyn CaptureStream>,
    tx: crossbeam_channel::Sender<CaptureEvent>,
    stop: std::sync::Arc<StopSignal>,
) -> std::thread::JoinHandle<CaptureThreadOutcome> {
    let stream_id = stream.stream_id();
    std::thread::Builder::new()
        .name(format!("capture-{:?}", stream_id))
        .spawn(move || {
            let (mmcss_applied, run_result) =
                mmcss::with_pro_audio_priority(|| stream.run(&tx, &stop));

            match run_result {
                Ok(exit) => {
                    let _ = tx.send(CaptureEvent::StreamStopped {
                        stream: stream_id,
                        exit,
                        mmcss_applied,
                    });
                    CaptureThreadOutcome::Stopped { exit, mmcss_applied }
                }
                Err(e) => {
                    let _ = tx.send(CaptureEvent::StreamError {
                        stream: stream_id,
                        error: e.to_string(),
                    });
                    CaptureThreadOutcome::Errored {
                        error: e,
                        mmcss_applied,
                    }
                }
            }
        })
        .expect("failed to spawn capture thread")
}
