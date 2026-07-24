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

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioObjectAddPropertyListener,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener,
};

use crate::error::CaptureError;

/// Every property this module watches, in one place so `start`/`drop` register and
/// unregister exactly the same set.
const WATCHED_SELECTORS: [u32; 3] = [
    kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice,
];

#[derive(Debug, Clone)]
pub enum DeviceWatchEvent {
    DeviceAdded {
        device_uid: String,
    },
    DeviceRemoved {
        device_uid: String,
    },
    /// `kAudioHardwarePropertyDevices` fired — the device list changed somehow,
    /// but CoreAudio's listener alone can't say what (added vs. removed, which
    /// device). The consumer re-enumerates and diffs against its own last-seen
    /// snapshot (`capture_api::device_diff`) to recover real added/removed facts —
    /// see `macos_supervisor::MacosSupervisor::reconcile_device_list`. Replaces an
    /// earlier placeholder that reported this as `DeviceAdded { device_uid:
    /// String::new() }`, which claimed an identity this listener never actually
    /// has.
    DeviceListChanged,
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
/// for as long as device-change events are wanted; dropping it unregisters every
/// listener and frees the boxed [`crossbeam_channel::Sender`] passed as CoreAudio's
/// client-data pointer, mirroring `capture-windows::device_watch::DeviceWatch`'s
/// RAII shape.
///
/// Uses the plain C-callback flavor (`AudioObjectAddPropertyListener`, taking an
/// `extern "C-unwind" fn` + a `client_data: *mut c_void`) rather than the
/// Objective-C-block flavor (`AudioObjectAddPropertyListenerBlock`) this module
/// originally used — the block variant needs a `block2`-wrapped closure, which
/// turned out not to bind the way a raw `*mut _`-cast Rust closure does (caught by
/// the first real macOS CI build). The C-callback flavor's function-pointer +
/// opaque-pointer shape is the classic, straightforward-to-bind-correctly pattern.
pub struct DeviceWatch {
    /// Owns the heap allocation `property_changed` dereferences via its
    /// `client_data` parameter on every callback. Freed in `Drop`, only after every
    /// listener referencing it has been unregistered.
    client_data: NonNull<crossbeam_channel::Sender<DeviceWatchEvent>>,
}

impl DeviceWatch {
    /// Registers one C-callback listener (`property_changed`) for the device list
    /// and both default-device selectors, forwarding CoreAudio's raw notifications
    /// onto `tx` as [`DeviceWatchEvent`]s. Deliberately does not resolve device
    /// UIDs inside the callback itself (CoreAudio property listeners fire on an
    /// internal thread/queue with no guarantee about which thread calls
    /// `AudioObjectGetPropertyData` safely) — the callback only signals *that*
    /// something changed; resolving current state happens via
    /// `device_select::enumerate_capture_devices`/`enumerate_render_devices` calls
    /// made by the consumer in response, the same "notification is a trigger to
    /// re-enumerate, not a payload to trust" pattern
    /// `capture-windows::device_watch::DeviceWatchEvent` already documents for its
    /// own `PropertyValueChanged`/`DeviceStateChanged` variants.
    pub fn start(tx: crossbeam_channel::Sender<DeviceWatchEvent>) -> Result<Self, CaptureError> {
        let client_data = NonNull::from(Box::leak(Box::new(tx)));
        for selector in WATCHED_SELECTORS {
            if let Err(err) = register_listener(selector, client_data) {
                // Unregister whatever succeeded before this failure, then free the
                // boxed sender, rather than leaking it on a partial-start failure.
                for already_registered in WATCHED_SELECTORS.iter().take_while(|s| **s != selector) {
                    let _ = unregister_listener(*already_registered, client_data);
                }
                unsafe { drop(Box::from_raw(client_data.as_ptr())) };
                return Err(err);
            }
        }
        Ok(Self { client_data })
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        for selector in WATCHED_SELECTORS {
            // Best-effort unregistration on drop; a failure here just leaks the
            // listener registration rather than being something a Drop impl can
            // surface.
            let _ = unregister_listener(selector, self.client_data);
        }
        // SAFETY: `client_data` was created via `Box::leak` in `start` and every
        // listener referencing it has just been unregistered above, so nothing can
        // call `property_changed` with this pointer again after this point.
        unsafe { drop(Box::from_raw(self.client_data.as_ptr())) };
    }
}

fn register_listener(
    selector: u32,
    client_data: NonNull<crossbeam_channel::Sender<DeviceWatchEvent>>,
) -> Result<(), CaptureError> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let status = unsafe {
        AudioObjectAddPropertyListener(
            system_object(),
            NonNull::from(&mut address),
            Some(property_changed),
            client_data.as_ptr() as *mut c_void,
        )
    };
    if status != 0 {
        return Err(CaptureError::CoreAudio(format!(
            "AudioObjectAddPropertyListener failed with OSStatus {status}"
        )));
    }
    Ok(())
}

fn unregister_listener(
    selector: u32,
    client_data: NonNull<crossbeam_channel::Sender<DeviceWatchEvent>>,
) -> Result<(), CaptureError> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let status = unsafe {
        AudioObjectRemovePropertyListener(
            system_object(),
            NonNull::from(&mut address),
            Some(property_changed),
            client_data.as_ptr() as *mut c_void,
        )
    };
    if status != 0 {
        return Err(CaptureError::CoreAudio(format!(
            "AudioObjectRemovePropertyListener failed with OSStatus {status}"
        )));
    }
    Ok(())
}

/// `kAudioObjectSystemObject` is `c_int` (`i32`) in `objc2-core-audio`, but every
/// function that takes an object ID expects `AudioObjectID` (`u32`) — see
/// `device_select::system_object`'s identical rationale (duplicated here rather
/// than shared, since the two modules are otherwise independent and this is a
/// one-line cast, not worth a cross-module dependency for).
fn system_object() -> AudioObjectID {
    kAudioObjectSystemObject as AudioObjectID
}

/// The `AudioObjectPropertyListenerProc` callback CoreAudio invokes on every
/// watched property's change. `client_data` is the `NonNull<Sender<...>>` pointer
/// `start`/`register_listener` passed in — reconstructed as a borrow (never taking
/// ownership away from `DeviceWatch`, which frees it exactly once in `Drop`).
///
/// Only inspects `in_addresses`' first entry's `mSelector` to decide which
/// `DeviceWatchEvent` to emit, even though `in_number_addresses` could in principle
/// be greater than 1 — CoreAudio is documented to always deliver one address per
/// registered listener callback in practice for this crate's usage (one listener
/// registered per selector, not one shared listener across multiple selectors at
/// once), so this is a reasonable simplification rather than a correctness gap for
/// the selectors this module watches.
unsafe extern "C-unwind" fn property_changed(
    _in_object_id: AudioObjectID,
    _in_number_addresses: u32,
    in_addresses: NonNull<AudioObjectPropertyAddress>,
    in_client_data: *mut c_void,
) -> i32 {
    let tx = unsafe { &*(in_client_data as *const crossbeam_channel::Sender<DeviceWatchEvent>) };
    let address = unsafe { in_addresses.as_ref() };

    let event = match address.mSelector {
        s if s == kAudioHardwarePropertyDevices => DeviceWatchEvent::DeviceListChanged,
        s if s == kAudioHardwarePropertyDefaultInputDevice => {
            DeviceWatchEvent::DefaultInputDeviceChanged { device_uid: None }
        }
        s if s == kAudioHardwarePropertyDefaultOutputDevice => {
            DeviceWatchEvent::DefaultOutputDeviceChanged { device_uid: None }
        }
        _ => return 0,
    };
    let _ = tx.send(event);
    0
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
