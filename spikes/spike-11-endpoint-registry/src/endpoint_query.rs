// spike-windows-11-detail-design.md §5相当。
// 「今のこの1台のendpointの完全な状態」を問い合わせる関数群。コールバック内
// では絶対に呼ばず、消費スレッド(registry.rs)からのみ呼ぶ(重い処理を
// コールバックの外へ逃がす方針)。

use crate::snapshot::{AudioEndpointSnapshot, DataFlow, DeviceRole, DeviceState};
use std::collections::{BTreeSet, HashMap};
use windows::core::HSTRING;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole,
    IMMDevice, IMMDeviceEnumerator, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    DEVICE_STATE_NOTPRESENT, DEVICE_STATE_UNPLUGGED,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL, STGM_READ};

fn role_to_erole(role: DeviceRole) -> ERole {
    match role {
        DeviceRole::Console => eConsole,
        DeviceRole::Multimedia => eMultimedia,
        DeviceRole::Communications => eCommunications,
    }
}

fn read_device_id(device: &IMMDevice) -> windows::core::Result<String> {
    unsafe {
        let pwstr = device.GetId()?;
        let id = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(id)
    }
}

fn read_friendly_name(device: &IMMDevice) -> windows::core::Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let propvar = store.GetValue(&PKEY_Device_FriendlyName)?;
        let pwstr = PropVariantToStringAlloc(&propvar)?;
        let name = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(name)
    }
}

/// 一部の仮想デバイスは`IAudioEndpointVolume`を取得できないため、
/// 失敗はエラーにせず`(None, None)`として扱う(spike-windows-11-detail-design.md §7-7)。
fn read_volume_and_mute(device: &IMMDevice) -> (Option<f32>, Option<bool>) {
    let volume: windows::core::Result<IAudioEndpointVolume> =
        unsafe { device.Activate(CLSCTX_ALL, None) };
    match volume {
        Ok(v) => {
            let scalar = unsafe { v.GetMasterVolumeLevelScalar() }.ok();
            let muted = unsafe { v.GetMute() }.ok().map(|b| b.as_bool());
            (scalar, muted)
        }
        Err(_) => (None, None),
    }
}

fn find_default_roles(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
    device_id: &str,
) -> BTreeSet<DeviceRole> {
    let mut roles = BTreeSet::new();
    for role in DeviceRole::ALL {
        let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(flow, role_to_erole(role)) }
            .ok()
            .and_then(|device| read_device_id(&device).ok());
        if default_id.as_deref() == Some(device_id) {
            roles.insert(role);
        }
    }
    roles
}

/// `device_id`が指すendpointを取得し直し、現時点の完全な`AudioEndpointSnapshot`を構築する。
/// `flow`が不明(呼び出し元がまだ知らない)な場合は、capture/renderの両方から探す。
pub fn query_snapshot_by_id(
    enumerator: &IMMDeviceEnumerator,
    device_id: &str,
    known_flow: Option<DataFlow>,
    revision: u64,
    observed_at_100ns: u64,
) -> windows::core::Result<AudioEndpointSnapshot> {
    let device = unsafe { enumerator.GetDevice(&HSTRING::from(device_id))? };
    build_snapshot(enumerator, &device, known_flow, revision, observed_at_100ns)
}

fn build_snapshot(
    enumerator: &IMMDeviceEnumerator,
    device: &IMMDevice,
    known_flow: Option<DataFlow>,
    revision: u64,
    observed_at_100ns: u64,
) -> windows::core::Result<AudioEndpointSnapshot> {
    let id = read_device_id(device)?;
    let state_raw = unsafe { device.GetState()? };
    let device_state = DeviceState::from_win32(state_raw.0).unwrap_or_else(|raw| {
        tracing::warn!(raw, "unrecognized DEVICE_STATE_* value; treating as NotPresent");
        DeviceState::NotPresent
    });
    let friendly_name = read_friendly_name(device).unwrap_or_default();
    let (volume_scalar, muted) = read_volume_and_mute(device);

    let flow = match known_flow {
        Some(f) => f,
        None => detect_flow(enumerator, &id).unwrap_or(DataFlow::Capture),
    };
    let win32_flow = match flow {
        DataFlow::Capture => eCapture,
        DataFlow::Render => eRender,
    };
    let default_roles = find_default_roles(enumerator, win32_flow, &id);

    Ok(AudioEndpointSnapshot {
        id,
        flow,
        device_state,
        friendly_name,
        default_roles,
        volume_scalar,
        muted,
        revision,
        last_observed_at_100ns: observed_at_100ns,
    })
}

/// `device_id`がcapture/render全endpoint列挙のどちらに含まれるかで
/// flowを判定する(`IMMEndpoint::GetDataFlow`より、既存の列挙結果との
/// 突き合わせの方が状態を問わない全件列挙(NotPresent等含む)と一貫するため)。
fn detect_flow(enumerator: &IMMDeviceEnumerator, device_id: &str) -> Option<DataFlow> {
    let all_states = (DEVICE_STATE_ACTIVE.0
        | DEVICE_STATE_DISABLED.0
        | DEVICE_STATE_NOTPRESENT.0
        | DEVICE_STATE_UNPLUGGED.0) as u32;
    for (flow, data_flow) in [(eCapture, DataFlow::Capture), (eRender, DataFlow::Render)] {
        let found = unsafe {
            enumerator
                .EnumAudioEndpoints(flow, windows::Win32::Media::Audio::DEVICE_STATE(all_states))
        }
        .ok()
        .and_then(|collection| {
            let count = unsafe { collection.GetCount() }.ok()?;
            for i in 0..count {
                if let Ok(d) = unsafe { collection.Item(i) } {
                    if read_device_id(&d).ok().as_deref() == Some(device_id) {
                        return Some(());
                    }
                }
            }
            None
        });
        if found.is_some() {
            return Some(data_flow);
        }
    }
    None
}

/// 起動時の初期スナップショット。状態を問わず(Active/Disabled/NotPresent/Unplugged)
/// すべてのcapture/render endpointを列挙する(合否基準「無効化デバイスの観測」のため)。
pub fn scan_all_endpoints(
    enumerator: &IMMDeviceEnumerator,
    observed_at_100ns: u64,
) -> windows::core::Result<Vec<AudioEndpointSnapshot>> {
    let all_states = windows::Win32::Media::Audio::DEVICE_STATE(
        (DEVICE_STATE_ACTIVE.0
            | DEVICE_STATE_DISABLED.0
            | DEVICE_STATE_NOTPRESENT.0
            | DEVICE_STATE_UNPLUGGED.0) as u32,
    );

    let mut out = Vec::new();
    for (flow, data_flow) in [(eCapture, DataFlow::Capture), (eRender, DataFlow::Render)] {
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, all_states)? };
        let count = unsafe { collection.GetCount()? };
        for i in 0..count {
            let device = unsafe { collection.Item(i)? };
            let snapshot = build_snapshot(enumerator, &device, Some(data_flow), 0, observed_at_100ns)?;
            out.push(snapshot);
        }
    }
    Ok(out)
}

/// (flow, role) 6通りそれぞれの既定デバイスIDを引く。既定デバイスが
/// 存在しない場合(GetDefaultAudioEndpointのエラー)はNoneとして扱う。
pub fn scan_default_routes(
    enumerator: &IMMDeviceEnumerator,
) -> HashMap<(DataFlow, DeviceRole), Option<String>> {
    let mut routes = HashMap::new();
    for (flow, data_flow) in [(eCapture, DataFlow::Capture), (eRender, DataFlow::Render)] {
        for role in DeviceRole::ALL {
            let id = unsafe { enumerator.GetDefaultAudioEndpoint(flow, role_to_erole(role)) }
                .ok()
                .and_then(|device| read_device_id(&device).ok());
            routes.insert((data_flow, role), id);
        }
    }
    routes
}
