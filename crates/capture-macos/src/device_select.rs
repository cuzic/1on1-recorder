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

use capture_api::rebinding::DeviceRole;
use objc2_core_audio::{
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioDeviceID,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
};

use crate::error::CaptureError;

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
/// **Not yet verified against a real build.** `objc2_core_audio`'s exact
/// `AudioObjectGetPropertyData`/`AudioObjectGetPropertyDataSize` signatures were
/// researched from documentation only (no macOS host in this dev environment) — the
/// unsafe call sites below are expected to need small signature fixes on first real
/// compile (parameter order, pointer types, `Option`-wrapping of out-params).
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
            pid: app.process_id(),
        })
        .collect())
}

fn all_device_ids() -> Result<Vec<AudioDeviceID>, CaptureError> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let data_size = unsafe {
        AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &address, 0, std::ptr::null())
    };
    let data_size = data_size.map_err(|err| CaptureError::CoreAudio(err.to_string()))?;
    let count = data_size as usize / std::mem::size_of::<AudioDeviceID>();

    let mut device_ids = vec![AudioDeviceID::default(); count];
    let mut actual_size = data_size;
    unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &address,
            0,
            std::ptr::null(),
            &mut actual_size,
            device_ids.as_mut_ptr() as *mut _,
        )
    }
    .map_err(|err| CaptureError::CoreAudio(err.to_string()))?;

    Ok(device_ids)
}

fn default_device_uid(selector: u32) -> Result<Option<String>, CaptureError> {
    let address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut device_id = AudioDeviceID::default();
    let mut size = std::mem::size_of::<AudioDeviceID>() as u32;
    let result = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            &mut device_id as *mut _ as *mut _,
        )
    };
    match result {
        Ok(()) => Ok(Some(device_uid(device_id)?)),
        Err(_) => Ok(None),
    }
}

// device_uid/device_name/device_has_input_streams/device_has_output_streams: thin
// per-device property reads (kAudioDevicePropertyDeviceUID,
// kAudioObjectPropertyName, kAudioDevicePropertyStreams on the input/output scopes
// respectively). Left as a follow-up alongside task 3's mic-stream implementation
// (task 6's device_watch.rs needs the same property-read plumbing, so it's more
// efficient to build the shared helper once real code exercises it than to guess
// the exact CFString-marshalling calls here with no way to compile-check them).
fn device_uid(_device_id: AudioDeviceID) -> Result<String, CaptureError> {
    unimplemented!("kAudioDevicePropertyDeviceUID read — implemented alongside task 3/6")
}

fn device_name(_device_id: AudioDeviceID) -> Result<String, CaptureError> {
    unimplemented!("kAudioObjectPropertyName read — implemented alongside task 3/6")
}

fn device_has_input_streams(_device_id: AudioDeviceID) -> Result<bool, CaptureError> {
    unimplemented!("kAudioDevicePropertyStreams(input scope) read — implemented alongside task 3/6")
}

fn device_has_output_streams(_device_id: AudioDeviceID) -> Result<bool, CaptureError> {
    unimplemented!(
        "kAudioDevicePropertyStreams(output scope) read — implemented alongside task 3/6"
    )
}
