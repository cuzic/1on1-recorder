// spike-windows-11-detail-design.md §6相当。
// spike_common::device_watch::DeviceWatchEventを受け取り、該当endpointを
// 再取得してAudioEndpointSnapshotレジストリを更新する消費側ロジック。
// 重い処理(COM呼び出しでの再列挙)はすべてここで行い、コールバック側
// (spike-common::device_watch)では絶対に行わない。

use crate::endpoint_query::query_snapshot_by_id;
use crate::snapshot::{AudioEndpointSnapshot, DataFlow, DeviceRole};
use spike_common::device_watch::DeviceWatchEvent;
use std::collections::HashMap;
use std::time::Duration;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, IMMDeviceEnumerator,
};

/// 1件のendpoint再照会(COM経由)に許す上限時間。実機検証(2026-07-11、
/// Bluetoothヘッドセット接続時)で、切断済み("NotPresent")のendpointから
/// 短時間に大量のPropertyValueChangedが発生し、1件ずつの同期的な再照会が
/// 積み重なって後続イベントの処理がdispatch_latency換算で数十秒単位まで
/// 遅延する事例が実際に観測された。再照会をタイムアウト付きの別スレッドへ
/// 逃がすことで、1つの「荒れた」endpointが他のendpointの処理を巻き込んで
/// 遅延させないようにする。
const ENDPOINT_REFRESH_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub enum RegistryChange {
    EndpointAdded {
        new: AudioEndpointSnapshot,
    },
    EndpointUpdated {
        old: AudioEndpointSnapshot,
        new: AudioEndpointSnapshot,
    },
    EndpointRemoved {
        id: String,
        last_known: Option<AudioEndpointSnapshot>,
    },
    DefaultRouteChanged {
        flow: DataFlow,
        role: DeviceRole,
        old: Option<String>,
        new: Option<String>,
    },
    /// endpoint再照会がENDPOINT_REFRESH_TIMEOUT以内に完了しなかった。
    /// レジストリはこのイベントについては更新されない(次のイベントで
    /// 改めて再照会される可能性がある)。
    RefreshTimedOut {
        id: String,
    },
}

pub struct EndpointRegistry {
    snapshots: HashMap<String, AudioEndpointSnapshot>,
    default_routes: HashMap<(DataFlow, DeviceRole), Option<String>>,
    revision_counter: u64,
}

impl EndpointRegistry {
    pub fn new(
        initial: Vec<AudioEndpointSnapshot>,
        default_routes: HashMap<(DataFlow, DeviceRole), Option<String>>,
    ) -> Self {
        let snapshots = initial.into_iter().map(|s| (s.id.clone(), s)).collect();
        Self {
            snapshots,
            default_routes,
            revision_counter: 0,
        }
    }

    pub fn snapshot_all(&self) -> Vec<AudioEndpointSnapshot> {
        let mut v: Vec<_> = self.snapshots.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    fn next_revision(&mut self) -> u64 {
        self.revision_counter += 1;
        self.revision_counter
    }

    /// `DeviceWatchEvent`を1件処理し、レジストリを更新した結果のdiffを返す。
    /// COM呼び出しに失敗した場合は`Err`を返し、呼び出し側はログに記録して
    /// 次のイベントへ進む(1件の失敗で全体を止めない)。
    pub fn apply_os_event(
        &mut self,
        event: &DeviceWatchEvent,
        enumerator: &IMMDeviceEnumerator,
    ) -> windows::core::Result<Vec<RegistryChange>> {
        match event {
            DeviceWatchEvent::DeviceAdded {
                endpoint_id,
                observed_at_100ns,
            }
            | DeviceWatchEvent::DeviceStateChanged {
                endpoint_id,
                observed_at_100ns,
                ..
            }
            | DeviceWatchEvent::PropertyValueChanged {
                endpoint_id,
                observed_at_100ns,
                ..
            } => self.refresh_endpoint(enumerator, endpoint_id, *observed_at_100ns),

            DeviceWatchEvent::DeviceRemoved {
                endpoint_id,
                observed_at_100ns: _,
            } => {
                let last_known = self.snapshots.remove(endpoint_id);
                Ok(vec![RegistryChange::EndpointRemoved {
                    id: endpoint_id.clone(),
                    last_known,
                }])
            }

            DeviceWatchEvent::DefaultDeviceChanged {
                flow_raw,
                role_raw,
                endpoint_id,
                observed_at_100ns: _,
            } => Ok(self.apply_default_changed(*flow_raw, *role_raw, endpoint_id.clone())),
        }
    }

    fn refresh_endpoint(
        &mut self,
        enumerator: &IMMDeviceEnumerator,
        endpoint_id: &str,
        observed_at_100ns: u64,
    ) -> windows::core::Result<Vec<RegistryChange>> {
        let known_flow = self.snapshots.get(endpoint_id).map(|s| s.flow);
        let revision = self.next_revision();

        let outcome = query_snapshot_with_timeout(
            enumerator,
            endpoint_id,
            known_flow,
            revision,
            observed_at_100ns,
            ENDPOINT_REFRESH_TIMEOUT,
        );

        let Some(result) = outcome else {
            tracing::warn!(
                endpoint_id,
                timeout_ms = ENDPOINT_REFRESH_TIMEOUT.as_millis() as u64,
                "endpoint refresh timed out; skipping this update to avoid blocking other events"
            );
            return Ok(vec![RegistryChange::RefreshTimedOut {
                id: endpoint_id.to_string(),
            }]);
        };
        let new_snapshot = result?;

        let change = match self.snapshots.insert(endpoint_id.to_string(), new_snapshot.clone()) {
            Some(old) => RegistryChange::EndpointUpdated {
                old,
                new: new_snapshot,
            },
            None => RegistryChange::EndpointAdded { new: new_snapshot },
        };
        Ok(vec![change])
    }

    fn apply_default_changed(
        &mut self,
        flow_raw: i32,
        role_raw: i32,
        new_endpoint_id: Option<String>,
    ) -> Vec<RegistryChange> {
        let (Some(flow), Some(role)) = (data_flow_from_raw(flow_raw), device_role_from_raw(role_raw))
        else {
            return Vec::new();
        };

        let key = (flow, role);
        let old = self.default_routes.insert(key, new_endpoint_id.clone()).flatten();

        if old == new_endpoint_id {
            return Vec::new();
        }

        if let Some(old_id) = &old {
            if let Some(s) = self.snapshots.get_mut(old_id) {
                s.default_roles.remove(&role);
            }
        }
        if let Some(new_id) = &new_endpoint_id {
            if let Some(s) = self.snapshots.get_mut(new_id) {
                s.default_roles.insert(role);
            }
        }

        vec![RegistryChange::DefaultRouteChanged {
            flow,
            role,
            old,
            new: new_endpoint_id,
        }]
    }
}

/// `query_snapshot_by_id`を専用スレッドで実行し、`timeout`以内に終わらなければ
/// `None`を返す(スレッド自体はバックグラウンドで完走し、結果は捨てられる)。
/// `IMMDeviceEnumerator`はCOM MTAオブジェクトへの参照(AddRefされたクローン)
/// なので、別スレッドから呼ぶにはそのスレッド自身もMTAへ参加させる必要がある
/// (P0-3方針: ここだけの例外としてスレッドをまたぐが、COMの規則には従う)。
/// `IMMDeviceEnumerator`は`windows`crate側でSendが実装されていない
/// (`NonNull<c_void>`を内部に持つため)。MTAで生成されたCOMオブジェクトは、
/// 呼び出し側スレッドもMTAへ参加してさえいれば任意のスレッドから呼んでよい
/// というCOMの規則に従い、`SendableHandle`(capture_loop.rs)と同じ方針で
/// 明示的に`Send`を許可するラッパーを用意する。
struct SendableEnumerator(IMMDeviceEnumerator);
unsafe impl Send for SendableEnumerator {}

fn query_snapshot_with_timeout(
    enumerator: &IMMDeviceEnumerator,
    endpoint_id: &str,
    known_flow: Option<DataFlow>,
    revision: u64,
    observed_at_100ns: u64,
    timeout: Duration,
) -> Option<windows::core::Result<AudioEndpointSnapshot>> {
    let enumerator = SendableEnumerator(enumerator.clone());
    let endpoint_id = endpoint_id.to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let enumerator = enumerator;
        let _com = spike_common::com_guard::ComApartment::new_mta();
        let result = query_snapshot_by_id(
            &enumerator.0,
            &endpoint_id,
            known_flow,
            revision,
            observed_at_100ns,
        );
        let _ = tx.send(result);
    });

    rx.recv_timeout(timeout).ok()
}

fn data_flow_from_raw(flow_raw: i32) -> Option<DataFlow> {
    if flow_raw == eCapture.0 {
        Some(DataFlow::Capture)
    } else if flow_raw == eRender.0 {
        Some(DataFlow::Render)
    } else {
        None // eAllはDefaultDeviceChangedでは飛んでこない想定
    }
}

fn device_role_from_raw(role_raw: i32) -> Option<DeviceRole> {
    if role_raw == eConsole.0 {
        Some(DeviceRole::Console)
    } else if role_raw == eMultimedia.0 {
        Some(DeviceRole::Multimedia)
    } else if role_raw == eCommunications.0 {
        Some(DeviceRole::Communications)
    } else {
        None
    }
}
