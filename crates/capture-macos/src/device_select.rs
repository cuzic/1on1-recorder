//! CoreAudio device enumeration and running-application enumeration.
//!
//! `capture-windows::device_select`'s `DeviceInfo`/`enumerate_capture_devices`/
//! `enumerate_render_devices` shape is mirrored here, but CoreAudio has no exact
//! equivalent of WASAPI's `DeviceRole` (`Console`/`Multimedia`/`Communications`) —
//! there is just one default input device and one default output device. Like
//! Windows Phase 1A (which already hardcodes `DeviceRole::Console` everywhere, see
//! `windows_supervisor.rs`), this module always reports `DeviceRole::Console` and
//! leaves `Multimedia`/`Communications` unused rather than inventing a mapping.
//!
//! Running-application enumeration (for the "select a meeting app" flow, design.md
//! §5.2 step 1, and the app-relaunch reconciliation of §16.2) is exposed here too,
//! via `screencapturekit`'s `SCShareableContent` — ScreenCaptureKit is the only API
//! this crate uses that already knows about running applications, so it makes more
//! sense to enumerate them through it than to add a second, separate
//! process-enumeration dependency.

use std::ffi::c_void;
use std::ptr::NonNull;

use capture_api::rebinding::DeviceRole;
use objc2_core_audio::{
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain, kAudioObjectPropertyName,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, AudioDeviceID,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress,
};
use objc2_core_foundation::{CFRetained, CFString};

use crate::error::CaptureError;

/// `objc2-core-audio`'s functions return a raw `OSStatus` (`i32`, `0` = success)
/// rather than a `Result` — this crate's own `Result<_, CaptureError>` convention
/// is applied at this one boundary rather than at every call site.
fn check_status(status: i32, context: &str) -> Result<(), CaptureError> {
    if status != 0 {
        return Err(CaptureError::CoreAudio(format!(
            "{context} failed with OSStatus {status}"
        )));
    }
    Ok(())
}

/// `kAudioObjectSystemObject` is declared as `c_int` (`i32`) in `objc2-core-audio`,
/// but every function that takes an object ID expects `AudioObjectID` (`u32`) — a
/// deliberately narrow, named cast (rather than sprinkling `as AudioObjectID` at
/// every call site) so the one intentional bit-reinterpretation is documented once.
fn system_object() -> AudioObjectID {
    kAudioObjectSystemObject as AudioObjectID
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// CoreAudio's `kAudioDevicePropertyDeviceUID` — a string that stays stable
    /// across reboots, unlike `AudioDeviceID` (which is only stable for the current
    /// boot session). This is the value that should be wrapped in
    /// `capture_api::rebinding::EndpointId`, matching how Windows uses
    /// `IMMDevice::GetId()`'s persistent string ID for the same purpose.
    pub id: String,
    pub friendly_name: String,
    pub is_default_for_role: Option<DeviceRole>,
}

#[derive(Debug, Clone)]
pub struct RunningApplicationInfo {
    /// The value to put in `capture_api::rebinding::BindingSelection::Process`'s
    /// `exe_name` field (macOS's closest equivalent is the bundle identifier, e.g.
    /// "us.zoom.xos" — process-name matching alone is less reliable on macOS than
    /// on Windows since app bundles can share a display name).
    pub bundle_identifier: String,
    pub display_name: String,
    pub pid: u32,
}

/// Enumerates every capture (input) device known to CoreAudio.
///
/// **Not yet run against real hardware.** `objc2_core_audio`'s
/// `AudioObjectGetPropertyData`/`AudioObjectGetPropertyDataSize` signatures were
/// researched from documentation only (no macOS host in this dev environment) —
/// they compile and pass this crate's own CI (`macos-build.yml`), but a real
/// device enumeration pass is still only verified by that workflow's
/// `e2e-best-effort` job, not locally.
pub fn enumerate_capture_devices() -> Result<Vec<DeviceInfo>, CaptureError> {
    let default_input = default_device_uid(kAudioHardwarePropertyDefaultInputDevice)?;
    let device_ids = all_device_ids()?;

    let mut devices = Vec::with_capacity(device_ids.len());
    for device_id in device_ids {
        if !device_has_input_streams(device_id)? {
            continue;
        }
        let id = device_uid(device_id)?;
        let friendly_name = device_name(device_id)?;
        let is_default_for_role = if Some(&id) == default_input.as_ref() {
            Some(DeviceRole::Console)
        } else {
            None
        };
        devices.push(DeviceInfo {
            id,
            friendly_name,
            is_default_for_role,
        });
    }
    Ok(devices)
}

/// Enumerates every render (output) device known to CoreAudio — needed so a chosen
/// "system audio" default output can be resolved and pinned the same way
/// `capture-windows::device_select::enumerate_render_devices` does for WASAPI
/// endpoint loopback.
pub fn enumerate_render_devices() -> Result<Vec<DeviceInfo>, CaptureError> {
    let default_output = default_device_uid(kAudioHardwarePropertyDefaultOutputDevice)?;
    let device_ids = all_device_ids()?;

    let mut devices = Vec::with_capacity(device_ids.len());
    for device_id in device_ids {
        if !device_has_output_streams(device_id)? {
            continue;
        }
        let id = device_uid(device_id)?;
        let friendly_name = device_name(device_id)?;
        let is_default_for_role = if Some(&id) == default_output.as_ref() {
            Some(DeviceRole::Console)
        } else {
            None
        };
        devices.push(DeviceInfo {
            id,
            friendly_name,
            is_default_for_role,
        });
    }
    Ok(devices)
}

/// Enumerates currently running applications via ScreenCaptureKit's shareable
/// content, for the meeting-app selection flow (design.md §5.2 step 1) and the
/// app-relaunch reconciliation `device_watch.rs` needs (design.md §16.2).
pub fn enumerate_running_applications() -> Result<Vec<RunningApplicationInfo>, CaptureError> {
    let content = screencapturekit::shareable_content::SCShareableContent::get()
        .map_err(|err| CaptureError::ScreenCaptureKit(err.to_string()))?;
    Ok(content
        .applications()
        .into_iter()
        .map(|app| RunningApplicationInfo {
            bundle_identifier: app.bundle_identifier(),
            display_name: app.application_name(),
            // ScreenCaptureKit reports pid_t (i32); RunningApplicationInfo.pid is
            // u32 to match capture_api::rebinding::Observation::ProcessRestarted's
            // pid fields — a real PID is always non-negative, so this is lossless.
            pid: app.process_id() as u32,
        })
        .collect())
}

fn all_device_ids() -> Result<Vec<AudioDeviceID>, CaptureError> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut data_size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            system_object(),
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut data_size),
        )
    };
    check_status(status, "AudioObjectGetPropertyDataSize")?;
    let count = data_size as usize / std::mem::size_of::<AudioDeviceID>();

    let mut device_ids = vec![AudioDeviceID::default(); count];
    let mut actual_size = data_size;
    let data_ptr = NonNull::new(device_ids.as_mut_ptr() as *mut c_void)
        .ok_or_else(|| CaptureError::CoreAudio("empty device list buffer".to_string()))?;
    let status = unsafe {
        AudioObjectGetPropertyData(
            system_object(),
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut actual_size),
            data_ptr,
        )
    };
    check_status(status, "AudioObjectGetPropertyData")?;

    Ok(device_ids)
}

fn default_device_uid(selector: u32) -> Result<Option<String>, CaptureError> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut device_id = AudioDeviceID::default();
    let mut size = std::mem::size_of::<AudioDeviceID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            system_object(),
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut device_id).cast(),
        )
    };
    if status != 0 {
        return Ok(None);
    }
    Ok(Some(device_uid(device_id)?))
}

// device_uid/device_name/device_has_input_streams/device_has_output_streams: thin
// per-device property reads (kAudioDevicePropertyDeviceUID,
// kAudioObjectPropertyName, kAudioDevicePropertyStreams on the input/output scopes
// respectively), built on the two shared helpers below.

/// Reads a `CFString`-valued property (`kAudioDevicePropertyDeviceUID`,
/// `kAudioObjectPropertyName`) off any `AudioObjectID`. Both properties are
/// documented (`AudioHardware.h`) to hand the caller an owned reference — no
/// extra retain is needed before `CFRetained::from_raw`, and dropping the
/// `CFRetained` below releases it.
fn read_cfstring_property(object_id: AudioObjectID, selector: u32) -> Result<String, CaptureError> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value: *const CFString = std::ptr::null();
    let mut size = std::mem::size_of::<*const CFString>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_id,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    check_status(status, "AudioObjectGetPropertyData(CFString)")?;
    let ptr = NonNull::new(value as *mut CFString)
        .ok_or_else(|| CaptureError::CoreAudio("CoreAudio returned a null CFString".to_string()))?;
    let cf_string = unsafe { CFRetained::from_raw(ptr) };
    Ok(cf_string.to_string())
}

/// Whether `device_id` has any streams in the given scope
/// (`kAudioObjectPropertyScopeInput`/`kAudioObjectPropertyScopeOutput`) —
/// `kAudioDevicePropertyStreams`'s data size is `0` when there are none, so the
/// actual stream list never needs to be fetched.
fn device_has_streams(device_id: AudioDeviceID, scope: u32) -> Result<bool, CaptureError> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut data_size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device_id,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut data_size),
        )
    };
    check_status(status, "AudioObjectGetPropertyDataSize(Streams)")?;
    Ok(data_size > 0)
}

fn device_uid(device_id: AudioDeviceID) -> Result<String, CaptureError> {
    read_cfstring_property(device_id, kAudioDevicePropertyDeviceUID)
}

fn device_name(device_id: AudioDeviceID) -> Result<String, CaptureError> {
    read_cfstring_property(device_id, kAudioObjectPropertyName)
}

fn device_has_input_streams(device_id: AudioDeviceID) -> Result<bool, CaptureError> {
    device_has_streams(device_id, kAudioObjectPropertyScopeInput)
}

fn device_has_output_streams(device_id: AudioDeviceID) -> Result<bool, CaptureError> {
    device_has_streams(device_id, kAudioObjectPropertyScopeOutput)
}
