// spike-windows-11-detail-design.md §3.2相当。
// AudioEndpointSnapshotはaudio-device-state-architecture.md §2.1の型を
// そのまま採用する(名称・フィールドを変更しない)。EndpointIdは
// spike-common::device_watchが既に生のStringで扱っているため、ここでも
// 新しいラッパー型を導入せずStringのまま統一する(過剰な抽象化を避ける)。

use serde::Serialize;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, DEVICE_STATE_NOTPRESENT, DEVICE_STATE_UNPLUGGED,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DeviceState {
    Active,
    Disabled,
    NotPresent,
    Unplugged,
}

impl DeviceState {
    /// `IMMDevice::GetState()`/`OnDeviceStateChanged`が返す`DEVICE_STATE_*`定数から変換する。
    /// 定義済みの4値以外が来た場合は生値を保持したまま呼び出し側でエラーとして扱う。
    pub fn from_win32(raw: u32) -> Result<Self, u32> {
        match raw {
            v if v == DEVICE_STATE_ACTIVE.0 => Ok(Self::Active),
            v if v == DEVICE_STATE_DISABLED.0 => Ok(Self::Disabled),
            v if v == DEVICE_STATE_NOTPRESENT.0 => Ok(Self::NotPresent),
            v if v == DEVICE_STATE_UNPLUGGED.0 => Ok(Self::Unplugged),
            other => Err(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum DataFlow {
    Capture,
    Render,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum DeviceRole {
    Console,
    Multimedia,
    Communications,
}

impl DeviceRole {
    pub const ALL: [DeviceRole; 3] = [Self::Console, Self::Multimedia, Self::Communications];
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioEndpointSnapshot {
    pub id: String,
    pub flow: DataFlow,

    pub device_state: DeviceState,
    pub friendly_name: String,

    pub default_roles: std::collections::BTreeSet<DeviceRole>,
    pub volume_scalar: Option<f32>,
    pub muted: Option<bool>,

    pub revision: u64,
    pub last_observed_at_100ns: u64,
}
