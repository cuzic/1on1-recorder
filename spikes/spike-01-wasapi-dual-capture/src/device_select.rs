// spike-windows-01-02-detail-design.md §4.3
//
// enumerate_*系は「一度きりの短命な問い合わせ」用であり、呼び出したスレッドで
// ComApartment::new_mta()を張って完結させてよい。一方、実際にキャプチャで使う
// IMMDevice/IAudioClientは、その値を取得したスレッドとは別のスレッドへ渡さない
// (P0-3)。resolve_*_device以降のActivate/Initialize/GetService/キャプチャ
// ループ/Stop/解放は、すべて同一のcapture MTAスレッド内で完結させる(wasapi_common.rs)。

use windows::Win32::Media::Audio::{IMMDevice, IMMDeviceEnumerator};

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

pub fn enumerate_capture_devices() -> windows::core::Result<Vec<DeviceInfo>> {
    todo!("§4.3: IMMDeviceEnumerator::EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)")
}

pub fn enumerate_render_devices() -> windows::core::Result<Vec<DeviceInfo>> {
    todo!("§4.3: IMMDeviceEnumerator::EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)")
}

pub fn resolve_capture_device(
    enumerator: &IMMDeviceEnumerator,
    id_or_default: &str, // "default" または DeviceInfo.id
    role: DeviceRole,
) -> windows::core::Result<IMMDevice> {
    todo!("§4.3: id_or_default==\"default\"ならGetDefaultAudioEndpoint(eCapture, role)、それ以外はGetDevice(id)")
}

pub fn resolve_render_device(
    enumerator: &IMMDeviceEnumerator,
    id_or_default: &str,
    role: DeviceRole,
) -> windows::core::Result<IMMDevice> {
    todo!("§4.3: id_or_default==\"default\"ならGetDefaultAudioEndpoint(eRender, role)、それ以外はGetDevice(id)")
}
