//! Standalone, session-independent device-change notifications: unlike
//! `windows_session::run_capture_blocking`'s `DeviceWatch`, which only lives for
//! the duration of one active recording session, `DeviceChangeWatcher` here is
//! meant to run for as long as a caller with no recording session wants live
//! device-arrival/removal notifications — e.g. `apps/desktop`'s Settings screen,
//! to auto-refresh its device list when a Bluetooth headset etc. is plugged in or
//! removed while Settings is open.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use capture_windows::device_watch::DeviceWatch;
use capture_windows::CaptureError;

/// Runs `DeviceWatch` on its own dedicated OS thread — `DeviceWatch::start`
/// requires the thread that creates it to stay alive for as long as it's alive
/// (it owns the COM apartment and `IMMNotificationClient` registration) — and
/// exposes device-change activity as a monotonically increasing counter rather
/// than the raw event stream: a caller that only wants "something changed, go
/// re-enumerate" (like a Settings device list) doesn't need to interpret
/// `DeviceWatchEvent`'s add/remove/state-change/default-change variants itself.
pub struct DeviceChangeWatcher {
    shutdown_tx: crossbeam_channel::Sender<()>,
    join_handle: Option<JoinHandle<()>>,
    generation: Arc<AtomicU64>,
}

impl DeviceChangeWatcher {
    /// Blocks the calling thread until the watcher thread has either registered
    /// its `IMMNotificationClient` callback or failed to (surfacing the same
    /// `CaptureError` `DeviceWatch::start` itself would have returned) — callers
    /// on an async runtime should run this via `spawn_blocking`.
    pub fn start() -> Result<Self, CaptureError> {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded(0);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let generation = Arc::new(AtomicU64::new(0));
        let generation_for_thread = generation.clone();

        let join_handle = std::thread::Builder::new()
            .name("device-change-watch".into())
            .spawn(move || {
                let watch = match DeviceWatch::start(event_tx) {
                    Ok(watch) => {
                        let _ = ready_tx.send(Ok(()));
                        watch
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };
                loop {
                    crossbeam_channel::select! {
                        recv(event_rx) -> msg => match msg {
                            Ok(_event) => {
                                generation_for_thread.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_disconnected) => break,
                        },
                        recv(shutdown_rx) -> _ => break,
                    }
                }
                drop(watch);
            })
            .expect("failed to spawn device-change-watch thread");

        ready_rx.recv().expect("device-change-watch thread died before signaling readiness")?;

        Ok(Self { shutdown_tx, join_handle: Some(join_handle), generation })
    }

    /// Monotonically increasing count of device-change notifications observed so
    /// far. Callers should remember the value from their last check and compare —
    /// any change (not the exact count) means "re-enumerate now".
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

impl Drop for DeviceChangeWatcher {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}
