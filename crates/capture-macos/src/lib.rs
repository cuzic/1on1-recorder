//! ScreenCaptureKit-backed audio capture (microphone and system-audio output) for
//! macOS 15+, resilient to device/app changes via `capture-api`'s rebinding state
//! machine.
//!
//! Structural difference from `capture-windows`: WASAPI needs two independent
//! streams (mic + loopback); ScreenCaptureKit delivers both from **one** `SCStream`
//! with two output types (`SCStreamOutputType::Audio` for system audio,
//! `SCStreamOutputType::Microphone` for mic, added in macOS 15 — see design.md
//! §5.2). [`sc_stream`] therefore owns one shared stream and demultiplexes its two
//! output callbacks into two [`CaptureEvent::Frame`] streams tagged
//! `BindingKind::EndpointLoopback`/`BindingKind::ProcessLoopback` and
//! `BindingKind::Microphone` respectively, rather than one OS stream per binding
//! like `capture-windows`'s `loopback_stream.rs`/`mic_stream.rs` split.
//!
//! **Not yet verified against a real build**: this dev environment has no macOS
//! host, so nothing here has been compiled or run. Low-level FFI details (exact
//! `objc2-core-audio` function signatures, the `screencapturekit` crate's feature
//! flag names) are best-effort from documentation research and are expected to need
//! small fixes on the first real macOS build/CI run — see
//! `stt-transcription-architecture.md`'s-style disclaimers for the analogous
//! precedent of "designed from docs, verified empirically later" in this project.

pub mod app_filter;
pub mod device_select;
pub mod device_watch;
pub mod error;
pub mod frame;
pub mod permissions;
pub mod sc_stream;
pub mod timestamp;

pub use error::CaptureError;
pub use frame::CapturedFrameRecord;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use capture_api::rebinding::BindingKind;

pub enum CaptureEvent {
    Frame {
        record: CapturedFrameRecord,
        samples: Vec<f32>,
    },
    StreamStarted {
        stream: BindingKind,
        sample_rate: u32,
        channels: u16,
        /// The engine's nominal callback interval in nanoseconds — see
        /// `capture-windows::CaptureEvent::StreamStarted`'s field of the same name
        /// for why this must come from the engine's configured periodicity rather
        /// than any one frame's `frame_count`.
        nominal_frame_interval_ns: u64,
    },
    StreamError {
        stream: BindingKind,
        error: String,
    },
    StreamStopped {
        stream: BindingKind,
        exit: CaptureExit,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureExit {
    StoppedByRequest,
    DeviceLost,
    /// TCC permission was denied or revoked mid-session — a first-class, expected
    /// error mode on macOS with no Windows analogue (design.md §5.2: "権限拒否・取消
    /// 時は録音できない").
    PermissionDenied,
}

/// Stop notification shared with the one worker thread that owns the `SCStream`.
/// Unlike `capture-windows`'s `StopSignal` (a Win32 event `HANDLE`, chosen so
/// `WaitForMultipleObjects` can multiplex it with other OS handles), no OS handle
/// needs wrapping here — a plain atomic flag + condvar is enough since nothing on
/// the macOS side needs that kind of multi-handle wait.
pub struct StopSignal {
    stopped: AtomicBool,
    condvar: Condvar,
    mutex: Mutex<()>,
}

impl StopSignal {
    pub fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            condvar: Condvar::new(),
            mutex: Mutex::new(()),
        }
    }

    pub fn signal(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.condvar.notify_all();
    }

    pub fn is_signaled(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Blocks the calling thread until [`signal`](Self::signal) is called or
    /// `timeout` elapses. Returns `true` if woken by `signal`, `false` on timeout.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        if self.is_signaled() {
            return true;
        }
        let guard = self.mutex.lock().unwrap();
        let (_guard, result) = self
            .condvar
            .wait_timeout_while(guard, timeout, |()| !self.is_signaled())
            .unwrap();
        !result.timed_out()
    }
}

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
}

pub trait CaptureStream: Send {
    /// The bindings this stream delivers frames for — plural, because a single
    /// `SCStream` covers both `Microphone` and `EndpointLoopback`/`ProcessLoopback`
    /// at once (see module doc comment). `capture-windows::CaptureStream::stream_id`
    /// is singular since WASAPI needs one OS stream per binding.
    fn bindings(&self) -> Vec<BindingKind>;

    /// Blocks the calling thread, continuing capture until `stop` is signaled or an
    /// unrecoverable error occurs.
    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, CaptureError>;
}

/// The return value of the `JoinHandle` produced by [`spawn_capture_thread`]. The
/// caller should treat this — not `CaptureEvent::StreamStopped` delivered over the
/// shared channel — as the source of truth for rebinding decisions (mirrors
/// `capture-windows::CaptureThreadOutcome`'s same rationale).
pub enum CaptureThreadOutcome {
    Stopped { exit: CaptureExit },
    Errored { error: CaptureError },
}

pub fn spawn_capture_thread(
    stream: Box<dyn CaptureStream>,
    tx: crossbeam_channel::Sender<CaptureEvent>,
    stop: std::sync::Arc<StopSignal>,
) -> std::thread::JoinHandle<CaptureThreadOutcome> {
    let bindings = stream.bindings();
    std::thread::Builder::new()
        .name(format!("capture-macos-{bindings:?}"))
        .spawn(move || match stream.run(&tx, &stop) {
            Ok(exit) => {
                for binding in &bindings {
                    let _ = tx.send(CaptureEvent::StreamStopped {
                        stream: *binding,
                        exit,
                    });
                }
                CaptureThreadOutcome::Stopped { exit }
            }
            Err(error) => {
                for binding in &bindings {
                    let _ = tx.send(CaptureEvent::StreamError {
                        stream: *binding,
                        error: error.to_string(),
                    });
                }
                CaptureThreadOutcome::Errored { error }
            }
        })
        .expect("failed to spawn capture thread")
}
