// spike-windows-01-02-detail-design.md §4.4/§4.5
//
// マイクとEndpoint Loopbackは、デバイス種別とstreamFlagsのみが異なり、
// 初期化・キャプチャループの本体(run_capture_loop)は共通化する。
// 実際のWASAPI呼び出し(unsafe FFI)は§7の既知の不確実性(windows crateの
// 実際のAPIパス)を解消してから実装するため、ここではtodo!()で止めている
// 箇所がある。制御フロー(バックプレッシャー、wake/packet分離、
// idle_timeout_count、停止シグナルの扱い)はそのまま実装済みのコードとして
// 書き下している。

use crate::device_select::{resolve_capture_device, resolve_render_device, DeviceRole};
use spike_common::frame_record::{CapturedFrameRecord, StreamId};
use spike_common::{AudioFormatInfo, CaptureEvent, CaptureExit, SpikeError, StopSignal};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, AUDCLNT_STREAMFLAGS_LOOPBACK,
};

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

/// GetBufferで取得したパケットの解放を保証するRAIIガード。
/// 「ReleaseBufferより先にチャネル送信してはいけない」という制約を型で強制する。
struct CapturePacketGuard<'a> {
    client: &'a IAudioCaptureClient,
    frames: u32,
}

impl<'a> Drop for CapturePacketGuard<'a> {
    fn drop(&mut self) {
        // 戻り値のエラーはログのみに留める(Drop内でpanicさせない)。
        let _ = unsafe { self.client.ReleaseBuffer(self.frames) };
    }
}

enum WaitResult {
    Signaled(usize),
    Timeout,
}

fn wait_for_multiple(
    _handles: &[windows::Win32::Foundation::HANDLE],
    _timeout_ms: u32,
) -> WaitResult {
    todo!("§4.4: WaitForMultipleObjects(handles, bWaitAll=false, timeout_ms)")
}

/// init_and_captureのステップ5以降(イベント待ち→GetBuffer/ReleaseBufferループ→Stop)
/// を共通関数に切り出す。マイク/Endpoint Loopbackはinit_and_captureがデバイス解決
/// からこの関数を呼ぶ形になり、Process Loopback(spike-02側)は独自のActivate/
/// Initialize手順の後に同じ形の関数を直接呼ぶ。
#[allow(clippy::too_many_arguments)]
pub fn run_capture_loop(
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    stream_id: StreamId,
    target_pid: Option<u32>,
    capture_epoch: u64,
    format_info: AudioFormatInfo,
    pipeline_drop_counter: Arc<AtomicU64>,
    callback_timeout_ms: u32,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    stop: &StopSignal,
) -> Result<CaptureExit, SpikeError> {
    let audio_ready_event: windows::Win32::Foundation::HANDLE =
        todo!("§4.4: CreateEventW(None, false, false, None) + audio_client.SetEventHandle(...)");

    let qpc_clock = spike_common::timestamp::QpcClock::query()?;

    unsafe { audio_client.Start()? };
    tx.send(CaptureEvent::StreamStarted {
        stream: stream_id,
        format: format_info.clone(),
        qpc_freq_hz: qpc_clock.freq_hz(),
    })
    .ok();

    let wait_handles = [audio_ready_event, stop.handle()];
    let mut wake_seq: u64 = 0;
    let mut packet_seq: u64 = 0;
    // Process Loopbackで対象アプリが無音の間、通知が来ないままタイムアウトを
    // 繰り返す回数。エラーではなく仕様上の挙動として記録する。
    let mut idle_timeout_count: u64 = 0;

    let exit = loop {
        match wait_for_multiple(&wait_handles, callback_timeout_ms) {
            WaitResult::Signaled(0) => {
                wake_seq += 1;
                let wake_qpc_100ns = qpc_clock.now_100ns();

                loop {
                    let packet_len = unsafe { capture_client.GetNextPacketSize()? };
                    if packet_len == 0 {
                        break;
                    }

                    let mut data_ptr: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    let mut device_position_frames: u64 = 0;
                    let mut capture_qpc_100ns: u64 = 0;
                    unsafe {
                        capture_client.GetBuffer(
                            &mut data_ptr,
                            &mut frames,
                            &mut flags,
                            Some(&mut device_position_frames),
                            Some(&mut capture_qpc_100ns),
                        )?;
                    }

                    // GetBufferの直後にガードを構築する。以後のどの経路でも
                    // ReleaseBufferが必ず呼ばれる。ガードが生きている間に
                    // tx.send()は行わない。
                    let guard = CapturePacketGuard {
                        client: &capture_client,
                        frames,
                    };
                    let is_silent =
                        flags & CapturedFrameRecord::FLAG_SILENT != 0 || data_ptr.is_null();
                    let samples = if is_silent {
                        vec![0.0f32; frames as usize * format_info.channels as usize]
                    } else {
                        todo!("§4.4: copy_to_f32_vec(data_ptr, frames, &format_info)")
                    };
                    drop(guard); // ここでReleaseBufferが確定する

                    let record = CapturedFrameRecord::from_raw(
                        stream_id,
                        wake_seq,
                        packet_seq,
                        wake_qpc_100ns,
                        device_position_frames,
                        capture_qpc_100ns,
                        frames,
                        flags,
                        capture_epoch,
                        target_pid,
                    );
                    packet_seq += 1;

                    match tx.try_send(CaptureEvent::Frame { record, samples }) {
                        Ok(()) => {}
                        Err(crossbeam_channel::TrySendError::Full(_)) => {
                            pipeline_drop_counter.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                            unsafe { audio_client.Stop()? };
                            return Ok(CaptureExit::StoppedByRequest);
                        }
                    }
                }
            }
            WaitResult::Signaled(1) => {
                break CaptureExit::StoppedByRequest; // stop_event
            }
            WaitResult::Signaled(_) => unreachable!(),
            WaitResult::Timeout => {
                // Process Loopbackは対象アプリが無音の場合エラーではなく単に
                // 通知が来ない仕様。マイク/Endpoint Loopbackでは異常寄りに扱う。
                match stream_id {
                    StreamId::ProcessLoopback => {
                        idle_timeout_count += 1;
                    }
                    StreamId::Mic | StreamId::EndpointLoopback => {
                        let _ = tx.send(CaptureEvent::StreamError {
                            stream: stream_id,
                            error: format!("callback timeout({callback_timeout_ms}ms)"),
                        });
                    }
                }
                continue;
            }
        }
    };

    unsafe { audio_client.Stop()?; }
    tracing::info!(stream = ?stream_id, idle_timeout_count, "capture loop finished");
    Ok(exit)
}

pub fn init_and_capture(
    params: WasapiInitParams,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    stop: &StopSignal,
    capture_epoch: u64,
) -> Result<CaptureExit, SpikeError> {
    let _com = spike_common::com_guard::ComApartment::new_mta()?;

    let enumerator: windows::Win32::Media::Audio::IMMDeviceEnumerator = todo!(
        "§4.4: CoCreateInstance::<IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)"
    );
    let device = match &params.device {
        DeviceSelector::Capture { id_or_default, role } => {
            resolve_capture_device(&enumerator, id_or_default, *role)?
        }
        DeviceSelector::Render { id_or_default, role } => {
            resolve_render_device(&enumerator, id_or_default, *role)?
        }
    };
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

    run_capture_loop(
        audio_client,
        capture_client,
        params.stream_id,
        None,
        capture_epoch,
        format_info,
        params.pipeline_drop_counter,
        params.callback_timeout_ms,
        tx,
        stop,
    )
}

// AUDCLNT_STREAMFLAGS_LOOPBACKはloopback_stream.rs側でextra_stream_flagsとして渡す。
#[allow(dead_code)]
const _: u32 = AUDCLNT_STREAMFLAGS_LOOPBACK;
