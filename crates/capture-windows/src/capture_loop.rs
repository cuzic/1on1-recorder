//! The capture loop itself: wait for a callback -> drain packets via
//! `GetBuffer`/`ReleaseBuffer` -> repeat until stopped. Everything before this (opening
//! the device, `IAudioClient::Initialize`) is backend-specific and lives in
//! `wasapi_common.rs`; this part is identical regardless of what kind of stream it is.

use crate::error::CaptureError;
use crate::frame::CapturedFrameRecord;
use crate::{AudioFormatInfo, CaptureEvent, CaptureExit, StopSignal};
use capture_api::rebinding::BindingKind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioSessionControl, IAudioSessionEvents,
    IAudioSessionEvents_Impl, AUDCLNT_E_DEVICE_INVALIDATED,
};
use windows::Win32::System::Com::IAgileObject_Impl;

/// RAII guard ensuring a packet obtained via `GetBuffer` is always released. Enforces
/// "never send to the channel before `ReleaseBuffer`" at the type level.
struct CapturePacketGuard<'a> {
    client: &'a IAudioCaptureClient,
    frames: u32,
}

impl<'a> Drop for CapturePacketGuard<'a> {
    fn drop(&mut self) {
        // Log-only on error; never panic inside Drop.
        let _ = unsafe { self.client.ReleaseBuffer(self.frames) };
    }
}

/// RAII wrapper that always `CloseHandle`s a handle created via `CreateEventW` when it
/// goes out of scope, so the raw `HANDLE` is never carried around unmanaged.
struct EventHandleGuard(windows::Win32::Foundation::HANDLE);

impl Drop for EventHandleGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Observes `IAudioSessionEvents::OnSessionDisconnected` — device removal, format
/// changes, exclusive-mode takeover, logoff, and similar events are all reported
/// through this callback. The callback runs on an OS thread, so it only forwards to
/// `tx` and signals `disconnected_event` (a manual-reset Win32 event) to wake the
/// capture loop immediately rather than waiting out `callback_timeout_ms`.
///
/// `HANDLE` is a raw pointer without `Send`/`Sync`, but `SetEvent`/`CloseHandle` are
/// safe to call from any thread, so this is explicitly allowed for the same reason as
/// `StopSignal` (lib.rs).
struct SendableHandle(windows::Win32::Foundation::HANDLE);
unsafe impl Send for SendableHandle {}
unsafe impl Sync for SendableHandle {}

#[windows::core::implement(IAudioSessionEvents, windows::Win32::System::Com::IAgileObject)]
struct SessionEventsHandler {
    tx: crossbeam_channel::Sender<CaptureEvent>,
    stream_id: BindingKind,
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

/// RAII wrapper for `IAudioSessionControl::RegisterAudioSessionNotification`.
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

/// Copies a raw buffer returned by `GetBuffer` into an `f32` sample vector.
/// `GetMixFormat`'s shared-mode mix format is almost always IEEE float 32-bit in
/// practice, but int16/int32 PCM are also handled just in case; an unsupported format
/// is treated as silence rather than an error (capture continues, same as the
/// `is_silent` branch).
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

/// The capture loop shared by every stream kind. The caller must have already
/// completed `IAudioClient::Initialize` before passing its `IAudioClient`/
/// `IAudioCaptureClient` in here. `device_id`/`device_friendly_name` should be
/// whatever string usefully identifies this stream (e.g. `IMMDevice::GetId()`).
#[allow(clippy::too_many_arguments)]
pub fn run_capture_loop(
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    stream_id: BindingKind,
    target_pid: Option<u32>,
    capture_epoch: u64,
    format_info: AudioFormatInfo,
    device_id: String,
    device_friendly_name: String,
    pipeline_drop_counter: Arc<AtomicU64>,
    callback_timeout_ms: u32,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    stop: &StopSignal,
) -> Result<CaptureExit, CaptureError> {
    // Auto-reset (manual_reset=false), initially unsignaled.
    let audio_ready_event: windows::Win32::Foundation::HANDLE =
        unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None)? };
    // Closed via this guard on every exit path, including early returns.
    let _audio_ready_event_guard = EventHandleGuard(audio_ready_event);
    unsafe { audio_client.SetEventHandle(audio_ready_event)? };

    // Session-level disconnect notification (manual-reset; detected immediately via
    // wait_handles once signaled).
    let session_disconnected_event: windows::Win32::Foundation::HANDLE =
        unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None)? };
    let _session_disconnected_event_guard = EventHandleGuard(session_disconnected_event);

    // Registering IAudioSessionEvents is best-effort; capture continues even if this
    // fails (not treated as fatal).
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

                    // Constructed right after GetBuffer; every subsequent path is
                    // guaranteed to call ReleaseBuffer. Never send to the channel
                    // while this guard is still alive.
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
                    drop(guard); // ReleaseBuffer happens here.

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
                // session_disconnected_event; SessionEventsHandler has already sent
                // the reason as CaptureEvent::SessionDisconnected.
                tracing::warn!(stream = ?stream_id, "session disconnected event observed; treating as device lost");
                let _ = unsafe { audio_client.Stop() };
                return Ok(CaptureExit::DeviceLost);
            }
            WaitResult::Signaled(_) => unreachable!(),
            WaitResult::Timeout => {
                let _ = tx.send(CaptureEvent::StreamError {
                    stream: stream_id,
                    error: format!("callback timeout({callback_timeout_ms}ms)"),
                });
                continue;
            }
        }
    };

    unsafe {
        audio_client.Stop()?;
    }
    tracing::info!(stream = ?stream_id, "capture loop finished");
    Ok(exit)
}
