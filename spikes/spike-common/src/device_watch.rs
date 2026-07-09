// spike-plan.md SPIKE-09 / spike-windows-11-detail-design.md §3-4を参照して実装。
//
// SPIKE-11(Audio Endpoint Registry)がスナップショット管理まで含めた本格的な
// レジストリを構築する予定なのに対し、ここではSPIKE-09が要求する最小限、
// 「デバイス変更イベントをそのまま観測してJSONLへ記録する」だけを提供する。
// AudioEndpointSnapshotの再構築やdefault_roles管理などはSPIKE-11の責務として
// 残す。
//
// IMMNotificationClientのコールバックは、我々の呼び出しスレッドとは別の
// OS側スレッドプールから呼ばれる。ブロックしない・重い処理をしないことが
// 要求されているため、ここでも「イベントをchannelへtry_sendするだけ」に徹する
// (spike-windows-01-02-detail-design.md §3.8のP1改善と同じ方針)。

use crate::com_guard::ComApartment;
use crate::error::SpikeError;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, DEVICE_STATE,
    EDataFlow, ERole, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, IAgileObject_Impl, CLSCTX_ALL};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

/// IMMNotificationClientの5つのコールバックをそのまま写し取った生イベント。
/// `AudioEndpointSnapshot`の再構築や`DeviceState::from_win32`相当の解釈は
/// あえて行わない(コールバック内で重い処理をしないため。§7参照)。
#[derive(Debug, Clone)]
pub enum DeviceWatchEvent {
    DeviceAdded {
        endpoint_id: String,
        observed_at_100ns: u64,
    },
    DeviceRemoved {
        endpoint_id: String,
        observed_at_100ns: u64,
    },
    DeviceStateChanged {
        endpoint_id: String,
        new_state_raw: u32,
        observed_at_100ns: u64,
    },
    PropertyValueChanged {
        endpoint_id: String,
        /// PROPERTYKEYそのものはスレッドをまたがせず、(fmtid, pid)へ変換する。
        property_key_fmtid: windows::core::GUID,
        property_key_pid: u32,
        observed_at_100ns: u64,
    },
    DefaultDeviceChanged {
        flow_raw: i32,
        role_raw: i32,
        /// 既定デバイスが存在しない場合はNone(§7-5: pwstrがNULLになりうる)。
        endpoint_id: Option<String>,
        observed_at_100ns: u64,
    },
}

/// `&PCWSTR`をこの呼び出しの間だけ有効なポインタとして扱い、所有権のある
/// `String`へ即座にコピーする。ポインタをコールバックの外へ持ち出さないこと
/// (Windows側が呼び出し後に解放しうる)。
fn endpoint_id_from_pcwstr(pwstr: &PCWSTR) -> String {
    if pwstr.is_null() {
        return String::new();
    }
    unsafe { pwstr.to_string().unwrap_or_default() }
}

fn now_100ns(qpc: &crate::timestamp::QpcClock) -> u64 {
    qpc.now_100ns()
}

#[windows::core::implement(IMMNotificationClient, windows::Win32::System::Com::IAgileObject)]
struct EndpointNotificationClient {
    tx: crossbeam_channel::Sender<DeviceWatchEvent>,
    qpc: crate::timestamp::QpcClock,
    /// try_sendがFullで失敗した回数。デバイス変更イベントは音声フレームより
    /// 遥かに低頻度なので通常は0のまま。
    drop_count: AtomicU64,
}

impl EndpointNotificationClient {
    fn send(&self, event: DeviceWatchEvent) {
        if self.tx.try_send(event).is_err() {
            self.drop_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl IMMNotificationClient_Impl for EndpointNotificationClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        pwstrdeviceid: &PCWSTR,
        dwnewstate: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.send(DeviceWatchEvent::DeviceStateChanged {
            endpoint_id: endpoint_id_from_pcwstr(pwstrdeviceid),
            new_state_raw: dwnewstate.0,
            observed_at_100ns: now_100ns(&self.qpc),
        });
        Ok(())
    }

    fn OnDeviceAdded(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        self.send(DeviceWatchEvent::DeviceAdded {
            endpoint_id: endpoint_id_from_pcwstr(pwstrdeviceid),
            observed_at_100ns: now_100ns(&self.qpc),
        });
        Ok(())
    }

    fn OnDeviceRemoved(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        self.send(DeviceWatchEvent::DeviceRemoved {
            endpoint_id: endpoint_id_from_pcwstr(pwstrdeviceid),
            observed_at_100ns: now_100ns(&self.qpc),
        });
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        pwstrdefaultdeviceid: &PCWSTR,
    ) -> windows::core::Result<()> {
        let endpoint_id = if pwstrdefaultdeviceid.is_null() {
            None
        } else {
            Some(endpoint_id_from_pcwstr(pwstrdefaultdeviceid))
        };
        self.send(DeviceWatchEvent::DefaultDeviceChanged {
            flow_raw: flow.0,
            role_raw: role.0,
            endpoint_id,
            observed_at_100ns: now_100ns(&self.qpc),
        });
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        pwstrdeviceid: &PCWSTR,
        key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        self.send(DeviceWatchEvent::PropertyValueChanged {
            endpoint_id: endpoint_id_from_pcwstr(pwstrdeviceid),
            property_key_fmtid: key.fmtid,
            property_key_pid: key.pid,
            observed_at_100ns: now_100ns(&self.qpc),
        });
        Ok(())
    }
}

impl IAgileObject_Impl for EndpointNotificationClient_Impl {}

/// `IMMDeviceEnumerator::RegisterEndpointNotificationCallback`のRAIIラッパー。
/// `ComApartment`と同様に「呼び出したスレッドで生成し、他スレッドへ渡さない」
/// (P0-3方針)。フィールド順(enumerator, client, _com)は、独自のDropで
/// UnregisterEndpointNotificationCallbackを行った後、フィールドの自動drop
/// (enumerator/client解放 → 最後に_comのCoUninitialize)という順序を保証する。
pub struct DeviceWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    _com: ComApartment,
}

impl DeviceWatch {
    /// 呼び出したスレッドでCOMを初期化し、デバイス変更通知の受信を開始する。
    /// このスレッドは`DeviceWatch`(戻り値)が生存する間、終了させないこと。
    pub fn start(
        tx: crossbeam_channel::Sender<DeviceWatchEvent>,
    ) -> Result<Self, SpikeError> {
        let _com = ComApartment::new_mta()?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let qpc = crate::timestamp::QpcClock::query()?;
        let handler = EndpointNotificationClient {
            tx,
            qpc,
            drop_count: AtomicU64::new(0),
        };
        let client: IMMNotificationClient = handler.into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client)? };
        Ok(Self {
            enumerator,
            client,
            _com,
        })
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        // 終了時に必ずUnregisterする。登録漏れ・解除漏れはハンドルリークに
        // つながる。
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
        };
    }
}
