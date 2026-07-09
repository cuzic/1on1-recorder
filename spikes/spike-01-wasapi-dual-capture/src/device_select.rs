// spike-windows-01-02-detail-design.md §4.3
//
// enumerate_*系は「一度きりの短命な問い合わせ」用であり、呼び出したスレッドで
// ComApartment::new_mta()を張って完結させてよい。一方、実際にキャプチャで使う
// IMMDevice/IAudioClientは、その値を取得したスレッドとは別のスレッドへ渡さない
// (P0-3)。resolve_*_device以降のActivate/Initialize/GetService/キャプチャ
// ループ/Stop/解放は、すべて同一のcapture MTAスレッド内で完結させる(wasapi_common.rs)。

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

/// WASAPIのERoleに対応。既定ではConsoleを使うが、会議アプリがeCommunications
/// ロールの既定デバイス(Bluetoothヘッドセット等)へ出力/入力している場合が
/// あるため、CLIから選べるようにする(§4.7参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DeviceRole {
    Console,
    Multimedia,
    Communications,
}

fn role_to_erole(role: DeviceRole) -> ERole {
    match role {
        DeviceRole::Console => eConsole,
        DeviceRole::Multimedia => eMultimedia,
        DeviceRole::Communications => eCommunications,
    }
}

/// IMMDevice::GetId()が返すCoTaskMemAlloc済みのPWSTRをRust Stringへコピーし、
/// 元のメモリはCoTaskMemFreeで解放する(呼び出し側の解放責務)。
///
/// pubなのは、wasapi_common::init_and_captureが実際に解決したdevice(resolve_*_device
/// の戻り値)からid/friendly_nameを取得し、summary.json(§4.8)へ記録するため。
pub fn read_device_id(device: &IMMDevice) -> windows::core::Result<String> {
    unsafe {
        let pwstr = device.GetId()?;
        let id = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(id)
    }
}

/// PKEY_Device_FriendlyNameをIPropertyStore経由で取得する。取得できない
/// デバイス(一部の仮想デバイス等)もあるため、失敗は致命的エラーにしない。
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

/// `device_id`がConsole/Multimedia/Communicationsのいずれかの既定デバイスと
/// 一致するかを調べる。一致した最初のロールを返す(1つのデバイスが複数の
/// ロールで既定になっている場合も珍しくないが、`DeviceInfo::is_default_for_role`
/// は診断表示用の代表値1つで足りるため、最初に見つかったものを採用する)。
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
    let _com = spike_common::com_guard::ComApartment::new_mta()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    collect_devices(&enumerator, eCapture)
}

pub fn enumerate_render_devices() -> windows::core::Result<Vec<DeviceInfo>> {
    let _com = spike_common::com_guard::ComApartment::new_mta()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    collect_devices(&enumerator, eRender)
}

pub fn resolve_capture_device(
    enumerator: &IMMDeviceEnumerator,
    id_or_default: &str, // "default" または DeviceInfo.id
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
