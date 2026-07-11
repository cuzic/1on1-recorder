//! Device enumeration and resolution.
//!
//! The `enumerate_*` functions are for one-shot queries and may open their own MTA
//! apartment on the calling thread. The `IMMDevice`/`IAudioClient` actually used for
//! capture, on the other hand, must never leave the thread that resolved them —
//! `Activate`/`Initialize`/`GetService`/the capture loop/`Stop`/release all happen on
//! the same capture MTA thread (see `wasapi_common.rs`).

use capture_api::rebinding::DeviceRole;
use windows::core::HSTRING;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole, IMMDevice,
    IMMDeviceEnumerator, DEVICE_STATE_ACTIVE, MMDeviceEnumerator,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

pub struct DeviceInfo {
    pub id: String, // IMMDevice::GetId()
    pub friendly_name: String,
    pub is_default_for_role: Option<DeviceRole>,
}

fn role_to_erole(role: DeviceRole) -> ERole {
    match role {
        DeviceRole::Console => eConsole,
        DeviceRole::Multimedia => eMultimedia,
        DeviceRole::Communications => eCommunications,
    }
}

/// Copies the CoTaskMemAlloc'd `PWSTR` returned by `IMMDevice::GetId()` into an owned
/// Rust `String`, freeing the original memory with `CoTaskMemFree` (the caller's
/// responsibility).
pub fn read_device_id(device: &IMMDevice) -> windows::core::Result<String> {
    unsafe {
        let pwstr = device.GetId()?;
        let id = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(id)
    }
}

/// Reads `PKEY_Device_FriendlyName` via `IPropertyStore`. Some (e.g. virtual) devices
/// don't have one, so failure here isn't treated as fatal.
pub fn read_friendly_name(device: &IMMDevice) -> windows::core::Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let propvar = store.GetValue(&PKEY_Device_FriendlyName)?;
        let pwstr = PropVariantToStringAlloc(&propvar)?;
        let name = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(name)
    }
}

/// Checks whether `device_id` matches the default device for any of
/// Console/Multimedia/Communications, returning the first role that matches (a single
/// device is commonly default for more than one role, but `DeviceInfo::
/// is_default_for_role` only needs one representative value for display purposes).
fn find_default_role(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
    device_id: &str,
) -> Option<DeviceRole> {
    for role in [
        DeviceRole::Console,
        DeviceRole::Multimedia,
        DeviceRole::Communications,
    ] {
        let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(flow, role_to_erole(role)) }
            .ok()
            .and_then(|device| read_device_id(&device).ok());
        if default_id.as_deref() == Some(device_id) {
            return Some(role);
        }
    }
    None
}

fn collect_devices(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
) -> windows::core::Result<Vec<DeviceInfo>> {
    unsafe {
        let collection = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        let mut devices = Vec::with_capacity(count as usize);
        for i in 0..count {
            let device = collection.Item(i)?;
            let id = read_device_id(&device)?;
            let friendly_name = read_friendly_name(&device).unwrap_or_default();
            let is_default_for_role = find_default_role(enumerator, flow, &id);
            devices.push(DeviceInfo {
                id,
                friendly_name,
                is_default_for_role,
            });
        }
        Ok(devices)
    }
}

pub fn enumerate_capture_devices() -> windows::core::Result<Vec<DeviceInfo>> {
    let _com = crate::com_guard::ComApartment::new_mta()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    collect_devices(&enumerator, eCapture)
}

pub fn enumerate_render_devices() -> windows::core::Result<Vec<DeviceInfo>> {
    let _com = crate::com_guard::ComApartment::new_mta()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    collect_devices(&enumerator, eRender)
}

pub fn resolve_capture_device(
    enumerator: &IMMDeviceEnumerator,
    id_or_default: &str, // "default" or a DeviceInfo.id
    role: DeviceRole,
) -> windows::core::Result<IMMDevice> {
    unsafe {
        if id_or_default == "default" {
            enumerator.GetDefaultAudioEndpoint(eCapture, role_to_erole(role))
        } else {
            enumerator.GetDevice(&HSTRING::from(id_or_default))
        }
    }
}

pub fn resolve_render_device(
    enumerator: &IMMDeviceEnumerator,
    id_or_default: &str,
    role: DeviceRole,
) -> windows::core::Result<IMMDevice> {
    unsafe {
        if id_or_default == "default" {
            enumerator.GetDefaultAudioEndpoint(eRender, role_to_erole(role))
        } else {
            enumerator.GetDevice(&HSTRING::from(id_or_default))
        }
    }
}
