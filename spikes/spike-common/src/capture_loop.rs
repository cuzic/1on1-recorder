// spike-windows-01-02-detail-design.md §4.4/§10(推奨実装順序 手順6)
//
// SPIKE-01(マイク/Endpoint Loopback)とSPIKE-02(Process Loopback)は、
// IAudioClient::Initialize以降のキャプチャループ本体(イベント待ち→
// GetBuffer/ReleaseBufferループ→Stop)が完全に共通のため、両スパイクが
// 個別に動作を確認できた時点でここ(spike-common)へ抽出する。
// デバイス種別ごとのActivate/Initialize手順(SPIKE-01のwasapi_common::
// init_and_capture、SPIKE-02のprocess_loopback::activate_and_initialize_with_retry)
// は各クレート側に残す。

use crate::frame_record::{CapturedFrameRecord, StreamId};
use crate::{AudioFormatInfo, CaptureEvent, CaptureExit, SpikeError, StopSignal};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioSessionControl, IAudioSessionEvents,
    IAudioSessionEvents_Impl, AUDCLNT_E_DEVICE_INVALIDATED,
};
use windows::Win32::System::Com::IAgileObject_Impl;

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

/// `CreateEventW`で生成したハンドルをスコープ終了時に必ず`CloseHandle`する
/// RAIIラッパー。生のHANDLEのまま持ち回さない。
struct EventHandleGuard(windows::Win32::Foundation::HANDLE);

impl Drop for EventHandleGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// spike-plan.md SPIKE-09: `IAudioSessionEvents::OnSessionDisconnected`を
/// 観測する。デバイス取り外し・フォーマット変更・排他モード奪取・ログオフ等が
/// このコールバックとして通知される。コールバックはOS側のスレッドから呼ばれる
/// ため、`tx`へ送るだけに留め、`disconnected_event`(手動リセットのWin32イベント)
/// をSetEventしてキャプチャループを即座に起床させる(callback_timeout_msの
/// 満了を待たずに反応するため)。
///
/// `HANDLE`は生ポインタで`Send`/`Sync`を持たないが、SetEvent/CloseHandleは
/// どのスレッドからでも安全に呼べるため、`StopSignal`(lib.rs)と同じ理由で
/// 明示的に許可する。
struct SendableHandle(windows::Win32::Foundation::HANDLE);
unsafe impl Send for SendableHandle {}
unsafe impl Sync for SendableHandle {}

#[windows::core::implement(IAudioSessionEvents, windows::Win32::System::Com::IAgileObject)]
struct SessionEventsHandler {
    tx: crossbeam_channel::Sender<CaptureEvent>,
    stream_id: StreamId,
    disconnected_event: SendableHandle,
}

impl IAudioSessionEvents_Impl for SessionEventsHandler_Impl {
    fn OnDisplayNameChanged(
        &self,
        _newdisplayname: &windows::core::PCWSTR,
        _eventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(
        &self,
        _newiconpath: &windows::core::PCWSTR,
        _eventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(
        &self,
        _newvolume: f32,
        _newmute: windows::Win32::Foundation::BOOL,
        _eventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(
        &self,
        _channelcount: u32,
        _newchannelvolumearray: *const f32,
        _changedchannel: u32,
        _eventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(
        &self,
        _newgroupingparam: *const windows::core::GUID,
        _eventcontext: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(
        &self,
        _newstate: windows::Win32::Media::Audio::AudioSessionState,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSessionDisconnected(
        &self,
        disconnectreason: windows::Win32::Media::Audio::AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        let _ = self.tx.try_send(CaptureEvent::SessionDisconnected {
            stream: self.stream_id,
            reason_raw: disconnectreason.0,
        });
        let _ = unsafe { windows::Win32::System::Threading::SetEvent(self.disconnected_event.0) };
        Ok(())
    }
}

impl IAgileObject_Impl for SessionEventsHandler_Impl {}

/// `IAudioSessionControl::RegisterAudioSessionNotification`のRAIIラッパー。
struct SessionEventsGuard {
    session_control: IAudioSessionControl,
    handler: IAudioSessionEvents,
}

impl Drop for SessionEventsGuard {
    fn drop(&mut self) {
        let _ = unsafe {
            self.session_control
                .UnregisterAudioSessionNotification(&self.handler)
        };
    }
}

fn is_device_invalidated(e: &windows::core::Error) -> bool {
    e.code() == AUDCLNT_E_DEVICE_INVALIDATED
}

enum WaitResult {
    Signaled(usize),
    Timeout,
}

fn wait_for_multiple(
    handles: &[windows::Win32::Foundation::HANDLE],
    timeout_ms: u32,
) -> WaitResult {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::WaitForMultipleObjects;

    let result = unsafe { WaitForMultipleObjects(handles, false, timeout_ms) };
    let signaled_range = WAIT_OBJECT_0.0..(WAIT_OBJECT_0.0 + handles.len() as u32);
    if signaled_range.contains(&result.0) {
        WaitResult::Signaled((result.0 - WAIT_OBJECT_0.0) as usize)
    } else {
        if result.0 == windows::Win32::Foundation::WAIT_FAILED.0 {
            tracing::warn!(
                error = %windows::core::Error::from_win32(),
                "WaitForMultipleObjects failed; treating as timeout"
            );
        }
        WaitResult::Timeout
    }
}

/// GetBufferが返す生バッファをf32サンプル列へコピーする。GetMixFormatが返す
/// 共有モードのミックスフォーマットは実務上ほぼ常にIEEE float 32bitだが、
/// 念のためint16/int32 PCMにも対応し、未対応フォーマットは無音として扱う
/// (エラーにはしない。is_silent分岐と同様、録音自体は継続する)。
fn copy_to_f32_vec(data_ptr: *const u8, frames: u32, format_info: &AudioFormatInfo) -> Vec<f32> {
    let sample_count = frames as usize * format_info.channels as usize;
    let mut samples = Vec::with_capacity(sample_count);
    unsafe {
        if format_info.is_float && format_info.bits_per_sample == 32 {
            let src = data_ptr as *const f32;
            for i in 0..sample_count {
                samples.push(std::ptr::read_unaligned(src.add(i)));
            }
        } else if !format_info.is_float && format_info.bits_per_sample == 16 {
            let src = data_ptr as *const i16;
            for i in 0..sample_count {
                samples.push(std::ptr::read_unaligned(src.add(i)) as f32 / 32768.0);
            }
        } else if !format_info.is_float && format_info.bits_per_sample == 32 {
            let src = data_ptr as *const i32;
            for i in 0..sample_count {
                samples.push(std::ptr::read_unaligned(src.add(i)) as f32 / 2_147_483_648.0);
            }
        } else {
            tracing::warn!(
                bits_per_sample = format_info.bits_per_sample,
                is_float = format_info.is_float,
                "unsupported PCM format; emitting silence for this packet"
            );
            samples.resize(sample_count, 0.0);
        }
    }
    samples
}

/// マイク/Endpoint Loopback(SPIKE-01)・Process Loopback(SPIKE-02)共通の
/// キャプチャループ本体。呼び出し側は`IAudioClient::Initialize`まで完了させた
/// 状態で、この関数へ`IAudioClient`/`IAudioCaptureClient`を渡す。
/// `device_id`/`device_friendly_name`は、SPIKE-01ではIMMDevice::GetId()等、
/// SPIKE-02では対象プロセスのPID/モードなど、ストリームの識別に使える
/// 文字列を呼び出し側が用意する(summary.jsonのdevicesブロック用、§4.8)。
#[allow(clippy::too_many_arguments)]
pub fn run_capture_loop(
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    stream_id: StreamId,
    target_pid: Option<u32>,
    capture_epoch: u64,
    format_info: AudioFormatInfo,
    device_id: String,
    device_friendly_name: String,
    pipeline_drop_counter: Arc<AtomicU64>,
    callback_timeout_ms: u32,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    stop: &StopSignal,
) -> Result<CaptureExit, SpikeError> {
    // 自動リセット(manual_reset=false)、初期状態非シグナル。
    let audio_ready_event: windows::Win32::Foundation::HANDLE =
        unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None)? };
    // 関数を抜けるどの経路でも(早期return含め)CloseHandleされるようにする。
    let _audio_ready_event_guard = EventHandleGuard(audio_ready_event);
    unsafe { audio_client.SetEventHandle(audio_ready_event)? };

    // spike-plan.md SPIKE-09: session-level切断通知(手動リセット、SetEvent
    // されたらwait_handlesで即座に検出する)。
    let session_disconnected_event: windows::Win32::Foundation::HANDLE =
        unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None)? };
    let _session_disconnected_event_guard = EventHandleGuard(session_disconnected_event);

    // IAudioSessionEvents登録はベストエフォート。取得・登録に失敗しても
    // キャプチャ自体は継続する(致命的エラーにしない)。
    let _session_events_guard = match unsafe { audio_client.GetService::<IAudioSessionControl>() }
    {
        Ok(session_control) => {
            let handler = SessionEventsHandler {
                tx: tx.clone(),
                stream_id,
                disconnected_event: SendableHandle(session_disconnected_event),
            };
            let handler_iface: IAudioSessionEvents = handler.into();
            match unsafe { session_control.RegisterAudioSessionNotification(&handler_iface) } {
                Ok(()) => Some(SessionEventsGuard {
                    session_control,
                    handler: handler_iface,
                }),
                Err(e) => {
                    tracing::warn!(stream = ?stream_id, error = %e, "RegisterAudioSessionNotification failed");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!(stream = ?stream_id, error = %e, "IAudioSessionControl unavailable; session disconnect events won't be observed");
            None
        }
    };

    let qpc_clock = crate::timestamp::QpcClock::query()?;

    unsafe { audio_client.Start()? };
    tx.send(CaptureEvent::StreamStarted {
        stream: stream_id,
        format: format_info.clone(),
        qpc_freq_hz: qpc_clock.freq_hz(),
        device_id,
        device_friendly_name,
    })
    .ok();

    let wait_handles = [audio_ready_event, stop.handle(), session_disconnected_event];
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
                    let packet_len = match unsafe { capture_client.GetNextPacketSize() } {
                        Ok(len) => len,
                        Err(e) if is_device_invalidated(&e) => {
                            tracing::warn!(stream = ?stream_id, error = %e, "device invalidated (GetNextPacketSize)");
                            return Ok(CaptureExit::DeviceLost);
                        }
                        Err(e) => return Err(e.into()),
                    };
                    if packet_len == 0 {
                        break;
                    }

                    let mut data_ptr: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    let mut device_position_frames: u64 = 0;
                    let mut capture_qpc_100ns: u64 = 0;
                    if let Err(e) = unsafe {
                        capture_client.GetBuffer(
                            &mut data_ptr,
                            &mut frames,
                            &mut flags,
                            Some(&mut device_position_frames),
                            Some(&mut capture_qpc_100ns),
                        )
                    } {
                        if is_device_invalidated(&e) {
                            tracing::warn!(stream = ?stream_id, error = %e, "device invalidated (GetBuffer)");
                            return Ok(CaptureExit::DeviceLost);
                        }
                        return Err(e.into());
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
                        copy_to_f32_vec(data_ptr, frames, &format_info)
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
            WaitResult::Signaled(2) => {
                // session_disconnected_event。理由自体はSessionEventsHandlerが
                // 既にCaptureEvent::SessionDisconnectedとして送信済み(§SPIKE-09)。
                tracing::warn!(stream = ?stream_id, "session disconnected event observed; treating as device lost");
                let _ = unsafe { audio_client.Stop() };
                return Ok(CaptureExit::DeviceLost);
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

    unsafe {
        audio_client.Stop()?;
    }
    tracing::info!(stream = ?stream_id, idle_timeout_count, "capture loop finished");
    tx.send(CaptureEvent::IdleTimeoutObserved {
        stream: stream_id,
        idle_timeout_count,
    })
    .ok();
    Ok(exit)
}
