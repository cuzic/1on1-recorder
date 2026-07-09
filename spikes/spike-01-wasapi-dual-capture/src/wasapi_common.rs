// spike-windows-01-02-detail-design.md §4.4/§4.5
//
// マイクとEndpoint Loopbackは、デバイス種別とstreamFlagsのみが異なる。
// デバイス解決(この関数)より後段のキャプチャループ本体は
// spike_common::capture_loop::run_capture_loop へ抽出済み(§10 手順6。
// SPIKE-02のProcess Loopbackとも共有する)。

use crate::device_select::{resolve_capture_device, resolve_render_device, DeviceRole};
use spike_common::frame_record::StreamId;
use spike_common::{AudioFormatInfo, CaptureEvent, CaptureExit, SpikeError, StopSignal};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use windows::Win32::Media::Audio::{IAudioCaptureClient, IAudioClient, AUDCLNT_STREAMFLAGS_LOOPBACK};

/// マイク/レンダーデバイスの指定を「文字列+ロール」で保持する。IMMDeviceそのものは
/// 保持しない(P0-3: COM所有権をcapture MTAスレッドへ一本化する方針のため、
/// デバイス解決自体をinit_and_capture内で行う)。
pub enum DeviceSelector {
    Capture { id_or_default: String, role: DeviceRole },
    Render { id_or_default: String, role: DeviceRole },
}

pub struct WasapiInitParams {
    pub device: DeviceSelector,
    pub extra_stream_flags: u32, // 0 または AUDCLNT_STREAMFLAGS_LOOPBACK
    pub stream_id: StreamId,
    pub callback_timeout_ms: u32,
    /// §3.8のbounded channelが満杯でフレームをdropした回数を数える。
    pub pipeline_drop_counter: Arc<AtomicU64>,
}

pub fn init_and_capture(
    params: WasapiInitParams,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    stop: &StopSignal,
    capture_epoch: u64,
) -> Result<CaptureExit, SpikeError> {
    let _com = spike_common::com_guard::ComApartment::new_mta()?;

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
    // 実際に解決されたdevice id/friendly_nameをsummary.json(§4.8)へ残すため、
    // Activateの前(deviceがまだローカルにある間)に読み取っておく。
    let device_id = crate::device_select::read_device_id(&device)?;
    let device_friendly_name = crate::device_select::read_friendly_name(&device).unwrap_or_default();

    // enumerator/deviceはこの関数のローカル変数であり、他スレッドへは渡さない。
    let audio_client: IAudioClient = unsafe { device.Activate(windows::Win32::System::Com::CLSCTX_ALL, None)? };

    let mix_format =
        spike_common::WaveFormatBox::from_raw(unsafe { audio_client.GetMixFormat()? });
    let format_info = AudioFormatInfo::from_waveformatex(mix_format.as_ref());

    unsafe {
        audio_client.Initialize(
            windows::Win32::Media::Audio::AUDCLNT_SHAREMODE_SHARED,
            windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | params.extra_stream_flags,
            0, // hnsBufferDuration: 0で最小レイテンシをOSに委ねる
            0,
            mix_format.as_ref(),
            None,
        )?;
    }

    let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService()? };

    spike_common::run_capture_loop(
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

// AUDCLNT_STREAMFLAGS_LOOPBACKはloopback_stream.rs側でextra_stream_flagsとして渡す。
#[allow(dead_code)]
const _: u32 = AUDCLNT_STREAMFLAGS_LOOPBACK;
