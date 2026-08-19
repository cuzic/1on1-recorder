//! macOS counterpart of `windows_device_watch` — same rationale (a
//! session-independent device-change watcher for e.g. `apps/desktop`'s Settings
//! screen), applied to `capture_macos::device_watch::DeviceWatch`'s CoreAudio
//! property listeners instead of `IMMNotificationClient`. See that module's doc
//! comment for what "Not yet verified against a real build" means for this file
//! too — it depends on `capture_macos::device_watch`, in the same boat.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use capture_macos::device_watch::DeviceWatch;
use capture_macos::CaptureError;

/// See `windows_device_watch::DeviceChangeWatcher`'s doc comment — identical
/// shape, backed by CoreAudio's device-list/default-device property listeners
/// instead of WASAPI's `IMMNotificationClient`.
pub struct DeviceChangeWatcher {
    shutdown_tx: crossbeam_channel::Sender<()>,
    join_handle: Option<JoinHandle<()>>,
    generation: Arc<AtomicU64>,
}

impl DeviceChangeWatcher {
    /// Blocks the calling thread until the watcher thread has either registered
    /// its CoreAudio property listeners or failed to (surfacing the same
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
        // Detach the shutdown-signal-and-join onto its own thread rather than
        // doing either synchronously here. `DeviceChangeWatcher` values are
        // commonly dropped from an async runtime worker thread — e.g. when a
        // Dioxus `use_future` holding one is cancelled on component unmount (see
        // apps/desktop/src/settings.rs's `spawn_device_change_watcher`, which
        // `spawn_blocking`s `start()` but not the eventual `drop`) — not from a
        // thread that was itself `spawn_blocking`'d. Both `shutdown_tx.send(())`
        // (a rendezvous channel — it blocks until the watcher thread's `select!`
        // picks it up) and `handle.join()` are blocking calls that don't belong on
        // that thread. See docs/adr/0005-macos-duplicate-device-enumeration-listeners.md.
        if let Some(handle) = self.join_handle.take() {
            let shutdown_tx = self.shutdown_tx.clone();
            std::thread::spawn(move || {
                let _ = shutdown_tx.send(());
                let _ = handle.join();
            });
        }
    }
}
