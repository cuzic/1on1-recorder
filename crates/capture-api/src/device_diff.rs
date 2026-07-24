//! Turns two point-in-time device enumerations into an added/removed delta.
//!
//! Exists for backends whose device-change notification can't tell you *what*
//! changed — only that *something* did (e.g. CoreAudio's
//! `kAudioHardwarePropertyDevices` listener; see `capture-macos::device_watch`'s
//! doc comment). Those backends re-enumerate in response and diff against the
//! last-seen snapshot to recover real [`crate::rebinding::Observation::EndpointAdded`]/
//! [`crate::rebinding::Observation::EndpointRemoved`] facts — the same information
//! Windows' `IMMNotificationClient` reports directly.
//!
//! One flat set of [`EndpointId`]s covers both capture and render devices: `decide`
//! matches `EndpointRemoved`/`EndpointAdded` against a binding's pinned
//! [`crate::rebinding::EndpointId`] regardless of [`crate::rebinding::DataFlow`], so
//! there is no need to track capture-side and render-side snapshots separately.

use std::collections::BTreeSet;

use crate::rebinding::EndpointId;

/// The set of device ids observed as present at some point in time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    present: BTreeSet<EndpointId>,
}

impl DeviceSnapshot {
    pub fn from_ids(ids: impl IntoIterator<Item = EndpointId>) -> Self {
        Self { present: ids.into_iter().collect() }
    }
}

/// The ids that appeared/disappeared between one [`DeviceSnapshot`] and the next.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeviceDelta {
    pub added: Vec<EndpointId>,
    pub removed: Vec<EndpointId>,
}

impl DeviceDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

impl DeviceSnapshot {
    /// Diffs `self` (the previously-seen snapshot) against `next`, then rolls
    /// `self` forward to `next` so the following call diffs from here. An
    /// empty-to-full first diff (e.g. before anything has been seen yet) only ever
    /// produces `added`, never a spurious `removed`.
    pub fn diff_and_update(&mut self, next: DeviceSnapshot) -> DeviceDelta {
        let added = next.present.difference(&self.present).cloned().collect();
        let removed = self.present.difference(&next.present).cloned().collect();
        *self = next;
        DeviceDelta { added, removed }
    }
}
