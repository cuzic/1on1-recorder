// spike-windows-11-detail-design.md §6相当。
// spike_common::device_watch::DeviceWatchEventを受け取り、該当endpointを
// 再取得してAudioEndpointSnapshotレジストリを更新する消費側ロジック。
// 重い処理(COM呼び出しでの再列挙)はすべてここで行い、コールバック側
// (spike-common::device_watch)では絶対に行わない。

use crate::endpoint_query::query_snapshot_by_id;
use crate::snapshot::{AudioEndpointSnapshot, DataFlow, DeviceRole};
use spike_common::device_watch::DeviceWatchEvent;
use std::collections::HashMap;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, IMMDeviceEnumerator,
};

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
        let new_snapshot =
            query_snapshot_by_id(enumerator, endpoint_id, known_flow, revision, observed_at_100ns)?;

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
