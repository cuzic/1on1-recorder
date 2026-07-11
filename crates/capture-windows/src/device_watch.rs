//! Observes `IMMNotificationClient` device-change notifications and forwards them as
//! raw events. Deliberately does not reconstruct a full endpoint snapshot or interpret
//! `DEVICE_STATE_*`/`EDataFlow`/`ERole` here — that's the consumer's job, kept off the
//! callback thread (see the module docs below).
//!
//! `IMMNotificationClient`'s callbacks run on an OS thread pool thread, not the thread
//! that registered them. They must not block or do heavy work, so each callback here
//! just does a non-blocking `try_send` onto a channel.

use crate::com_guard::ComApartment;
use crate::error::CaptureError;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, DEVICE_STATE,
    EDataFlow, ERole, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, IAgileObject_Impl, CLSCTX_ALL};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

/// A direct transcription of `IMMNotificationClient`'s five callbacks. Deliberately
/// does not attempt to interpret `DEVICE_STATE_*` etc. here — see the module docs.
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
        /// The `PROPERTYKEY` itself doesn't cross threads directly; converted to
        /// `(fmtid, pid)` instead.
        property_key_fmtid: windows::core::GUID,
        property_key_pid: u32,
        observed_at_100ns: u64,
    },
    DefaultDeviceChanged {
        flow_raw: i32,
        role_raw: i32,
        /// `None` when there is no default device (the OS can pass a null pointer).
        endpoint_id: Option<String>,
        observed_at_100ns: u64,
    },
}

/// Treats `&PCWSTR` as valid only for the duration of this call and immediately copies
/// it into an owned `String`. Never carry the pointer itself outside the callback
/// (Windows may free it once the callback returns).
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
    /// Count of `try_send` failures due to a full channel. Device-change events are
    /// far less frequent than audio frames, so this normally stays at 0.
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

/// RAII wrapper around `IMMDeviceEnumerator::RegisterEndpointNotificationCallback`.
/// Same policy as `ComApartment`: created on the thread that uses it, never handed to
/// another thread. Field order (`enumerator`, `client`, `_com`) matters: the custom
/// `Drop` below unregisters the callback first, and only then do the fields' automatic
/// drops release `enumerator`/`client` and finally call `CoUninitialize` via `_com`.
pub struct DeviceWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    _com: ComApartment,
}

impl DeviceWatch {
    /// Initializes COM on the calling thread and starts receiving device-change
    /// notifications. That thread must stay alive for as long as the returned
    /// `DeviceWatch` is alive.
    pub fn start(
        tx: crossbeam_channel::Sender<DeviceWatchEvent>,
    ) -> Result<Self, CaptureError> {
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
        // Always unregister on shutdown; a missed unregister leaks the registration.
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
        };
    }
}
