//! CoreAudio device-change notifications + running-application watch, the macOS
//! analogue of `capture-windows::device_watch`'s `IMMNotificationClient` wrapper.
//!
//! CoreAudio has no single notification-client object the way WASAPI does; instead,
//! an `AudioObjectPropertyListener` is registered per property on
//! `kAudioObjectSystemObject` for the device list and each default-device selector.
//! Separately, macOS meeting-app-restart detection (design.md §16.2's "running
//! applicationの再探索") has no CoreAudio equivalent at all — it's tracked via
//! `NSWorkspace`'s app-launch/terminate notifications instead, emitted here as the
//! same `DeviceWatchEvent` shape so `macos_supervisor` (task 7) can translate both
//! kinds of events into `capture_api::rebinding::Observation` uniformly.
//!
//! **Not yet verified against a real build** — see `lib.rs`'s top-level doc comment.

use objc2_core_audio::{
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioObjectAddPropertyListenerBlock,
    AudioObjectPropertyAddress, AudioObjectRemovePropertyListenerBlock,
};

use crate::error::CaptureError;

#[derive(Debug, Clone)]
pub enum DeviceWatchEvent {
    DeviceAdded {
        device_uid: String,
    },
    DeviceRemoved {
        device_uid: String,
    },
    DefaultInputDeviceChanged {
        device_uid: Option<String>,
    },
    DefaultOutputDeviceChanged {
        device_uid: Option<String>,
    },
    /// `capture_api::rebinding::Observation::ProcessRestarted`'s
    /// `old_pid`/`new_pid` fields need both PIDs, which a plain launch/terminate
    /// notification pair doesn't directly correlate — `macos_supervisor` (task 7)
    /// is responsible for pairing a `ApplicationTerminated` with a later
    /// `ApplicationLaunched` for the same bundle identifier into a
    /// `ProcessRestarted` observation, the same way it would treat an unpaired
    /// termination as `ProcessExited`.
    ApplicationLaunched {
        bundle_identifier: String,
        pid: u32,
    },
    ApplicationTerminated {
        bundle_identifier: String,
        pid: u32,
    },
}

/// RAII registration of the CoreAudio device-change listeners. Must be kept alive
/// for as long as device-change events are wanted; dropping it unregisters the
/// listener block, mirroring `capture-windows::device_watch::DeviceWatch`'s RAII
/// shape.
pub struct DeviceWatch {
    tx: crossbeam_channel::Sender<DeviceWatchEvent>,
}

impl DeviceWatch {
    /// Registers `AudioObjectPropertyListenerBlock`s for the device list and both
    /// default-device selectors, forwarding CoreAudio's raw notifications onto `tx`
    /// as [`DeviceWatchEvent`]s. Deliberately does not resolve device UIDs inside
    /// the listener block itself (CoreAudio property listener blocks fire on an
    /// internal dispatch queue with no guarantee about which thread calls
    /// `AudioObjectGetPropertyData` safely) — the block only signals *that*
    /// something changed; resolving current state happens via
    /// `device_select::enumerate_capture_devices`/`enumerate_render_devices` calls
    /// made by the consumer in response, the same "notification is a trigger to
    /// re-enumerate, not a payload to trust" pattern
    /// `capture-windows::device_watch::DeviceWatchEvent` already documents for its
    /// own `PropertyValueChanged`/`DeviceStateChanged` variants.
    pub fn start(tx: crossbeam_channel::Sender<DeviceWatchEvent>) -> Result<Self, CaptureError> {
        register_listener(kAudioHardwarePropertyDevices, &tx)?;
        register_listener(kAudioHardwarePropertyDefaultInputDevice, &tx)?;
        register_listener(kAudioHardwarePropertyDefaultOutputDevice, &tx)?;
        Ok(Self { tx })
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        for selector in [
            kAudioHardwarePropertyDevices,
            kAudioHardwarePropertyDefaultInputDevice,
            kAudioHardwarePropertyDefaultOutputDevice,
        ] {
            let address = AudioObjectPropertyAddress {
                mSelector: selector,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            // Best-effort unregistration on drop; a failure here just leaks the
            // listener block rather than being something a Drop impl can surface.
            unsafe {
                let _ = AudioObjectRemovePropertyListenerBlock(
                    kAudioObjectSystemObject,
                    &address,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
        }
    }
}

fn register_listener(
    selector: u32,
    tx: &crossbeam_channel::Sender<DeviceWatchEvent>,
) -> Result<(), CaptureError> {
    let address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let tx = tx.clone();
    // TODO(verify on real build): AudioObjectAddPropertyListenerBlock's exact
    // signature (queue parameter, block type) needs confirming — this is written
    // against best-effort documentation research, no macOS host available to
    // compile-check it (see lib.rs's top-level doc comment).
    let result = unsafe {
        AudioObjectAddPropertyListenerBlock(
            kAudioObjectSystemObject,
            &address,
            std::ptr::null_mut(), // dispatch_queue_t: run on CoreAudio's default queue
            &mut move |_number_addresses, _addresses| {
                let event = match selector {
                    s if s == kAudioHardwarePropertyDevices => {
                        // The listener alone can't tell added from removed; the
                        // consumer re-enumerates and diffs (see doc comment above).
                        // Reported as `DeviceAdded` with an empty UID as a
                        // re-enumerate-me trigger — refine once real behavior is
                        // observed on a Mac.
                        DeviceWatchEvent::DeviceAdded {
                            device_uid: String::new(),
                        }
                    }
                    s if s == kAudioHardwarePropertyDefaultInputDevice => {
                        DeviceWatchEvent::DefaultInputDeviceChanged { device_uid: None }
                    }
                    _ => DeviceWatchEvent::DefaultOutputDeviceChanged { device_uid: None },
                };
                let _ = tx.send(event);
            } as *mut _,
        )
    };
    result.map_err(|err| CaptureError::CoreAudio(err.to_string()))
}

/// Watches for meeting-app launch/terminate via `NSWorkspace` notifications.
/// **Stubbed for now** — `NSWorkspace` notification observation needs an
/// Objective-C runtime binding (`objc2`/`objc2-app-kit`) this crate doesn't
/// currently depend on; adding it is scoped to whenever `macos_supervisor` (task 7)
/// actually needs `ApplicationLaunched`/`ApplicationTerminated` events wired up for
/// real, rather than speculatively now.
pub struct ApplicationWatch;

impl ApplicationWatch {
    pub fn start(_tx: crossbeam_channel::Sender<DeviceWatchEvent>) -> Result<Self, CaptureError> {
        Ok(Self)
    }
}
