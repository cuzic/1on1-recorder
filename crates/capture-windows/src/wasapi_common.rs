//! Microphone and endpoint loopback differ only in which kind of device is opened and
//! which stream flags are used. Everything past device resolution (the capture loop
//! itself) lives in `capture_loop::run_capture_loop`, shared by both.

use crate::device_select::{resolve_capture_device, resolve_render_device};
use crate::error::CaptureError;
use crate::{AudioFormatInfo, CaptureEvent, CaptureExit, StopSignal};
use capture_api::rebinding::{BindingKind, DeviceRole};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use windows::Win32::Media::Audio::{IAudioCaptureClient, IAudioClient, AUDCLNT_STREAMFLAGS_LOOPBACK};

/// A microphone/render device selection kept as "string + role", never the `IMMDevice`
/// itself (device resolution happens inside `init_and_capture`, keeping COM ownership
/// on a single capture MTA thread).
pub enum DeviceSelector {
    Capture { id_or_default: String, role: DeviceRole },
    Render { id_or_default: String, role: DeviceRole },
}

pub struct WasapiInitParams {
    pub device: DeviceSelector,
    pub extra_stream_flags: u32, // 0, or AUDCLNT_STREAMFLAGS_LOOPBACK
    pub stream_id: BindingKind,
    pub callback_timeout_ms: u32,
    /// Counts frames dropped because the bounded channel to the consumer was full.
    pub pipeline_drop_counter: Arc<AtomicU64>,
}

pub fn init_and_capture(
    params: WasapiInitParams,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    stop: &StopSignal,
    capture_epoch: u64,
) -> Result<CaptureExit, CaptureError> {
    let _com = crate::com_guard::ComApartment::new_mta()?;

    let enumerator: windows::Win32::Media::Audio::IMMDeviceEnumerator = unsafe {
        windows::Win32::System::Com::CoCreateInstance(
            &windows::Win32::Media::Audio::MMDeviceEnumerator,
            None,
            windows::Win32::System::Com::CLSCTX_ALL,
        )?
    };
    let device = match &params.device {
        DeviceSelector::Capture { id_or_default, role } => {
            resolve_capture_device(&enumerator, id_or_default, *role)?
        }
        DeviceSelector::Render { id_or_default, role } => {
            resolve_render_device(&enumerator, id_or_default, *role)?
        }
    };
    // Read the resolved device's id/friendly_name before Activate (while `device` is
    // still local) so callers can report which device "default" actually resolved to.
    let device_id = crate::device_select::read_device_id(&device)?;
    let device_friendly_name = crate::device_select::read_friendly_name(&device).unwrap_or_default();

    // `enumerator`/`device` are local to this function and never handed to another thread.
    let audio_client: IAudioClient = unsafe { device.Activate(windows::Win32::System::Com::CLSCTX_ALL, None)? };

    let mix_format =
        crate::WaveFormatBox::from_raw(unsafe { audio_client.GetMixFormat()? });
    let format_info = AudioFormatInfo::from_waveformatex(mix_format.as_ref());

    unsafe {
        audio_client.Initialize(
            windows::Win32::Media::Audio::AUDCLNT_SHAREMODE_SHARED,
            windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | params.extra_stream_flags,
            0, // hnsBufferDuration: 0 lets the OS pick the minimum latency
            0,
            mix_format.as_ref(),
            None,
        )?;
    }

    let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService()? };

    crate::run_capture_loop(
        audio_client,
        capture_client,
        params.stream_id,
        None,
        capture_epoch,
        format_info,
        device_id,
        device_friendly_name,
        params.pipeline_drop_counter,
        params.callback_timeout_ms,
        tx,
        stop,
    )
}

// AUDCLNT_STREAMFLAGS_LOOPBACK is passed as extra_stream_flags by loopback_stream.rs.
#[allow(dead_code)]
const _: u32 = AUDCLNT_STREAMFLAGS_LOOPBACK;
