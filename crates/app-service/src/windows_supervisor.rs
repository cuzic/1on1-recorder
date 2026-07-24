//! Task #1: executes `capture_api::rebinding::decide()`'s effects against real
//! `capture-windows` capture threads. Only compiled with the `windows-supervisor`
//! feature and only meaningful on Windows (`capture-windows` itself is
//! Windows-only) — cross-compile-checked from this Linux dev environment via
//! `cargo check --target x86_64-pc-windows-gnu --features windows-supervisor`, the
//! same way `capture-windows` itself has been validated throughout this project.
//!
//! Scope: this module only manages capture-worker *lifecycle* (start/stop/rebind).
//! Feeding captured frames into `audio-timeline`/`segment-store` is app-service
//! stage 2's job (task #10) — see [`WindowsSupervisor::capture_events`].

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::capture_health::CaptureHealth;

use capture_api::rebinding::{
    decide, BindingKind, CaptureBinding, BindingSelection, DataFlow, DecisionInput, DecisionState,
    DeviceRole, Effect, EndpointId, EndpointSelection, Observation, OperationId, ResolvedTarget,
    StreamEpoch, UserIntent,
};
use capture_windows::device_watch::DeviceWatchEvent;
use capture_windows::loopback_stream::EndpointLoopbackStream;
use capture_windows::mic_stream::MicCaptureStream;
use capture_windows::{spawn_capture_thread, CaptureError, CaptureEvent, CaptureStream, CaptureThreadOutcome, StopSignal};
use crossbeam_channel::{Receiver, Select, Sender};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

/// A capture worker `decide()` currently believes is `Starting`/`Running`, tracked
/// here because neither `CaptureEvent` nor `CaptureThreadOutcome` carry
/// `operation_id`/`epoch` (they're supervisor-side bookkeeping, not something the
/// capture thread itself knows about).
struct WorkerHandle {
    operation_id: OperationId,
    epoch: StreamEpoch,
    target: ResolvedTarget,
    stop: Arc<StopSignal>,
    join_handle: std::thread::JoinHandle<CaptureThreadOutcome>,
}

struct JoinResult {
    binding: BindingKind,
    operation_id: OperationId,
    epoch: StreamEpoch,
    outcome: CaptureThreadOutcome,
}

/// What this supervisor forwards to an optional frame sink (see
/// `WindowsSupervisor::set_frame_sink`) — everything a consumer (task #10's
/// `windows_frame_collector`) needs to convert captured audio into
/// `recorder_domain::CapturedFrame`, without that consumer needing its own access
/// to `capture-windows`'s raw event channel.
///
/// This is a *forwarding* design, not a second receiver on the same channel:
/// `capture_windows::CaptureEvent`s only ever go to one consumer
/// (`run_until_shutdown`'s own `Select` loop) which re-sends the ones a frame sink
/// cares about here. Cloning `capture_rx` itself for an external consumer would
/// make it and `run_until_shutdown` competing consumers on the same MPMC channel —
/// each `Frame` event would race to land on whichever thread called `recv()` first,
/// silently dropping frames whenever the supervisor's own (frame-discarding) loop
/// won that race.
pub enum FrameSinkEvent {
    StreamStarted { binding: BindingKind, sample_rate: u32, channels: u16, nominal_frame_interval_ns: u64 },
    Frame { record: capture_windows::CapturedFrameRecord, samples: Vec<f32> },
}

pub struct WindowsSupervisor {
    state: DecisionState,
    workers: HashMap<BindingKind, WorkerHandle>,
    capture_tx: Sender<CaptureEvent>,
    capture_rx: Receiver<CaptureEvent>,
    join_result_tx: Sender<JoinResult>,
    join_result_rx: Receiver<JoinResult>,
    retry_tx: Sender<(BindingKind, u64)>,
    retry_rx: Receiver<(BindingKind, u64)>,
    pipeline_drop_counter: Arc<AtomicU64>,
    callback_timeout_ms: u32,
    /// Joiner threads spawned but not yet reported back — `run_until_shutdown` waits
    /// for this to reach 0 before returning.
    pending_joins: usize,
    frame_tx: Option<Sender<FrameSinkEvent>>,
    health_sink: Option<Arc<Mutex<CaptureHealth>>>,
}

impl WindowsSupervisor {
    /// Starts with no bindings at all — `pin_devices` (directly, or via
    /// `resolve_current_defaults` for "whatever's currently in use") must run
    /// before `start_all`. Bindings are deliberately never constructed as
    /// `EndpointSelection::FollowDefault`: `decide()`'s existing
    /// `DefaultEndpointChanged` handling would then rebind automatically on
    /// every later OS default-device change while `Running`, which is exactly
    /// what design.md §16.5 says must not happen unconditionally ("OSの既定マイクや
    /// 既定スピーカーが変わっても、無条件には追随しない") — once a recording
    /// starts, the device it started with is what it keeps, for the reasons that
    /// section gives (a silent switch would break Self/Remote's meaning
    /// mid-session).
    pub fn new(callback_timeout_ms: u32) -> Self {
        let state = DecisionState::new();
        let (capture_tx, capture_rx) = crossbeam_channel::bounded(256);
        let (join_result_tx, join_result_rx) = crossbeam_channel::unbounded();
        let (retry_tx, retry_rx) = crossbeam_channel::unbounded();
        Self {
            state,
            workers: HashMap::new(),
            capture_tx,
            capture_rx,
            join_result_tx,
            join_result_rx,
            retry_tx,
            retry_rx,
            pipeline_drop_counter: Arc::new(AtomicU64::new(0)),
            callback_timeout_ms,
            pending_joins: 0,
            frame_tx: None,
            health_sink: None,
        }
    }

    /// Registers where `StreamStarted`/`Frame` events get forwarded (see
    /// [`FrameSinkEvent`] for why this is a forwarding sink rather than a second
    /// receiver on the same channel). Task #10's `windows_frame_collector` is the
    /// intended consumer, on its own thread — dropping the receiving half (or never
    /// calling this) just means frames are discarded, which is fine if the caller
    /// only cares about capture lifecycle management.
    pub fn set_frame_sink(&mut self, tx: Sender<FrameSinkEvent>) {
        self.frame_tx = Some(tx);
    }

    /// Registers where this session's per-track health (see [`CaptureHealth`]) gets
    /// published on every `run_until_shutdown` loop iteration — `apps/desktop`'s
    /// recording screen is the intended consumer, polling it the same way it
    /// already polls `LevelSnapshot`. Not calling this just means no one observes
    /// health; capture lifecycle management itself is unaffected either way.
    pub fn set_health_sink(&mut self, sink: Arc<Mutex<CaptureHealth>>) {
        self.health_sink = Some(sink);
    }

    /// Snapshot of both tracks' current health, derived from `self.state`'s
    /// per-binding lifecycle — see `capture_api::rebinding::CaptureBindingState::health`.
    /// A binding not present in `self.state.bindings` yet (before `pin_devices`) or
    /// no longer present reads as `Ok` rather than an unhealthy state: "no binding"
    /// isn't itself an unhealthy condition here, it's a session that hasn't started.
    pub fn capture_health(&self) -> CaptureHealth {
        let self_health = self.state.bindings.get(&BindingKind::Microphone).map(|b| b.lifecycle.health().into()).unwrap_or_default();
        let remote_health = self.state.bindings.get(&BindingKind::EndpointLoopback).map(|b| b.lifecycle.health().into()).unwrap_or_default();
        CaptureHealth { self_health, remote_health }
    }

    fn publish_health(&self) {
        if let Some(sink) = &self.health_sink {
            *sink.lock().unwrap() = self.capture_health();
        }
    }

    /// Queries whatever the OS's current Console-role default capture/render
    /// endpoints are, right now — "what's currently in use" for each of
    /// Microphone and EndpointLoopback. Doesn't touch `self.state`; the caller
    /// decides what to do with the result (normally `pin_devices`).
    pub fn resolve_current_defaults(&self) -> Result<(EndpointId, EndpointId), CaptureError> {
        let _com = capture_windows::com_guard::ComApartment::new_mta()?;
        let enumerator: windows::Win32::Media::Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

        let capture_id = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)? };
        let capture_id = capture_windows::device_select::read_device_id(&capture_id)?;

        let render_id = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
        let render_id = capture_windows::device_select::read_device_id(&render_id)?;

        Ok((EndpointId(capture_id), EndpointId(render_id)))
    }

    /// Pins Phase 1A's two bindings (Microphone, EndpointLoopback) to specific
    /// endpoints — `resolve_current_defaults`'s result for "use whatever's
    /// currently in use", or a caller-chosen `EndpointId` from
    /// `capture_windows::device_select::enumerate_capture_devices`/
    /// `enumerate_render_devices` for a manual picker. Must run before
    /// `start_all`, and only while both bindings are absent/`Stopped` (i.e. before
    /// the first `start_all`, or after a full `ShutdownRequested` drain) — this
    /// does not itself stop an already-running binding to switch it.
    pub fn pin_devices(&mut self, microphone_endpoint_id: EndpointId, render_endpoint_id: EndpointId) {
        self.state.bindings.insert(
            BindingKind::Microphone,
            CaptureBinding::new(BindingKind::Microphone, BindingSelection::Endpoint(EndpointSelection::Pinned { endpoint_id: microphone_endpoint_id })),
        );
        self.state.bindings.insert(
            BindingKind::EndpointLoopback,
            CaptureBinding::new(BindingKind::EndpointLoopback, BindingSelection::Endpoint(EndpointSelection::Pinned { endpoint_id: render_endpoint_id })),
        );
    }

    pub fn start_all(&mut self) -> Result<(), CaptureError> {
        for binding in [BindingKind::Microphone, BindingKind::EndpointLoopback] {
            let effects = decide(&mut self.state, DecisionInput::UserIntent(UserIntent::Start { binding }));
            self.execute(effects)?;
        }
        Ok(())
    }

    /// Blocks, driving the FSM from real capture-worker and OS device-change events,
    /// until `shutdown_rx` receives. The caller is expected to have started a
    /// `capture_windows::device_watch::DeviceWatch` on the *same thread* that calls
    /// this (its own doc requires the creating thread to stay alive for as long as
    /// it's alive — this call blocking that thread satisfies that).
    ///
    /// Known limitation: `decide()`'s `ShutdownRequested` handling only stops
    /// bindings currently `Running` (see capture-api's rebinding module) — a binding
    /// still `Starting`/`Waiting` when shutdown arrives gets no `StopCapture` effect
    /// and this call will not wait for it.
    pub fn run_until_shutdown(&mut self, device_watch_rx: &Receiver<DeviceWatchEvent>, shutdown_rx: &Receiver<()>) -> Result<(), CaptureError> {
        loop {
            let mut sel = Select::new();
            let capture_idx = sel.recv(&self.capture_rx);
            let watch_idx = sel.recv(device_watch_rx);
            let join_idx = sel.recv(&self.join_result_rx);
            let retry_idx = sel.recv(&self.retry_rx);
            let shutdown_idx = sel.recv(shutdown_rx);
            let oper = sel.select();
            let index = oper.index();

            if index == capture_idx {
                if let Ok(event) = oper.recv(&self.capture_rx) {
                    self.handle_capture_event(event)?;
                }
            } else if index == watch_idx {
                if let Ok(event) = oper.recv(device_watch_rx) {
                    self.handle_device_watch_event(event)?;
                }
            } else if index == join_idx {
                if let Ok(result) = oper.recv(&self.join_result_rx) {
                    self.handle_join_result(result)?;
                }
            } else if index == retry_idx {
                if let Ok((binding, retry_id)) = oper.recv(&self.retry_rx) {
                    let effects = decide(&mut self.state, DecisionInput::RetryTimerFired { binding, retry_id });
                    self.execute(effects)?;
                }
            } else if index == shutdown_idx {
                let _ = oper.recv(shutdown_rx);
                let effects = decide(&mut self.state, DecisionInput::ShutdownRequested);
                self.execute(effects)?;
                self.drain_pending_joins();
                return Ok(());
            }
            self.publish_health();
        }
    }

    fn execute(&mut self, effects: Vec<Effect>) -> Result<(), CaptureError> {
        for effect in effects {
            match effect {
                Effect::StartCapture { binding, operation_id, proposed_epoch, target, .. } => {
                    let stream = build_stream(binding, &target, proposed_epoch, self.pipeline_drop_counter.clone(), self.callback_timeout_ms);
                    let stop = Arc::new(StopSignal::new()?);
                    let join_handle = spawn_capture_thread(stream, self.capture_tx.clone(), stop.clone());
                    self.workers.insert(binding, WorkerHandle { operation_id, epoch: proposed_epoch, target, stop, join_handle });
                }
                Effect::StopCapture { binding, operation_id, epoch, .. } => {
                    if let Some(worker) = self.workers.remove(&binding) {
                        worker.stop.signal()?;
                        self.spawn_joiner(binding, operation_id, epoch, worker.join_handle);
                    }
                }
                Effect::ScheduleRetry { binding, retry_id, attempt, .. } => {
                    let retry_tx = self.retry_tx.clone();
                    let delay = backoff_for_attempt(attempt);
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        let _ = retry_tx.send((binding, retry_id));
                    });
                }
            }
        }
        Ok(())
    }

    fn spawn_joiner(&mut self, binding: BindingKind, operation_id: OperationId, epoch: StreamEpoch, join_handle: std::thread::JoinHandle<CaptureThreadOutcome>) {
        self.pending_joins += 1;
        let join_result_tx = self.join_result_tx.clone();
        std::thread::spawn(move || {
            let outcome = join_handle.join().expect("capture worker thread panicked");
            let _ = join_result_tx.send(JoinResult { binding, operation_id, epoch, outcome });
        });
    }

    fn handle_capture_event(&mut self, event: CaptureEvent) -> Result<(), CaptureError> {
        match event {
            CaptureEvent::Frame { record, samples } => {
                if let Some(tx) = &self.frame_tx {
                    let _ = tx.send(FrameSinkEvent::Frame { record, samples });
                }
            }
            CaptureEvent::StreamStarted { stream, ref format, nominal_frame_interval_ns, .. } => {
                if let Some(tx) = &self.frame_tx {
                    let _ = tx.send(FrameSinkEvent::StreamStarted {
                        binding: stream,
                        sample_rate: format.sample_rate,
                        channels: format.channels,
                        nominal_frame_interval_ns,
                    });
                }
                // `self.workers` holds at most one handle per binding — the one
                // `decide()` is currently `Starting`/`Running` for — so its
                // operation_id/epoch/target are exactly what this observation needs.
                // decide()'s own admission check still applies: a stale arrival (the
                // binding already moved past `Starting`) is a safe no-op there.
                if let Some(worker) = self.workers.get(&stream) {
                    let (operation_id, epoch, target) = (worker.operation_id, worker.epoch, worker.target.clone());
                    let effects = decide(&mut self.state, DecisionInput::Observation(Observation::WorkerStarted { binding: stream, operation_id, epoch, target }));
                    self.execute(effects)?;
                }
            }
            CaptureEvent::StreamError { stream, error } => {
                if let Some(operation_id) = self.workers.get(&stream).map(|w| w.operation_id) {
                    let effects = decide(&mut self.state, DecisionInput::Observation(Observation::WorkerFailed { binding: stream, operation_id, error }));
                    self.execute(effects)?;
                }
                self.reap_dead_worker(stream);
            }
            CaptureEvent::StreamStopped { .. } => {
                // Informational only. The authoritative "this worker is completely
                // done" signal is the join() result in `handle_join_result`, not this
                // channel event — see this module's doc comment on
                // `run_until_shutdown` and Codex's review of task #1.
            }
            CaptureEvent::SessionDisconnected { stream, reason_raw } => {
                if let Some(operation_id) = self.workers.get(&stream).map(|w| w.operation_id) {
                    let effects = decide(
                        &mut self.state,
                        DecisionInput::Observation(Observation::WorkerFailed {
                            binding: stream,
                            operation_id,
                            error: format!("audio session disconnected (reason={reason_raw})"),
                        }),
                    );
                    self.execute(effects)?;
                }
                self.reap_dead_worker(stream);
            }
        }
        Ok(())
    }

    /// A worker that failed on its own (`WorkerFailed`/`SessionDisconnected`) is
    /// already dying without a `StopCapture` effect (`decide()` moves straight to
    /// `Waiting`/`Failed`) — its `JoinHandle` still needs reclaiming so it isn't
    /// leaked. The eventual join result is still fed through `decide()` (see
    /// `handle_join_result`); its own staleness guard (the binding is no longer
    /// `Stopping` by then) makes that a safe no-op.
    fn reap_dead_worker(&mut self, binding: BindingKind) {
        if let Some(worker) = self.workers.remove(&binding) {
            self.spawn_joiner(binding, worker.operation_id, worker.epoch, worker.join_handle);
        }
    }

    fn handle_device_watch_event(&mut self, event: DeviceWatchEvent) -> Result<(), CaptureError> {
        let observation = match event {
            DeviceWatchEvent::DeviceRemoved { endpoint_id, .. } => Some(Observation::EndpointRemoved { endpoint_id: EndpointId(endpoint_id) }),
            DeviceWatchEvent::DeviceAdded { endpoint_id, .. } => Some(Observation::EndpointAdded { endpoint_id: EndpointId(endpoint_id) }),
            DeviceWatchEvent::DeviceStateChanged { endpoint_id, new_state_raw, .. } => {
                if new_state_raw == DEVICE_STATE_ACTIVE.0 {
                    Some(Observation::EndpointAdded { endpoint_id: EndpointId(endpoint_id) })
                } else {
                    Some(Observation::EndpointRemoved { endpoint_id: EndpointId(endpoint_id) })
                }
            }
            DeviceWatchEvent::DefaultDeviceChanged { flow_raw, role_raw, endpoint_id, .. } => {
                let flow = if flow_raw == eCapture.0 {
                    Some(DataFlow::Capture)
                } else if flow_raw == eRender.0 {
                    Some(DataFlow::Render)
                } else {
                    None // eAll or unrecognized; decide() only tracks Capture/Render defaults
                };
                let role = if role_raw == eConsole.0 {
                    Some(DeviceRole::Console)
                } else if role_raw == eMultimedia.0 {
                    Some(DeviceRole::Multimedia)
                } else if role_raw == eCommunications.0 {
                    Some(DeviceRole::Communications)
                } else {
                    None
                };
                match (flow, role) {
                    (Some(flow), Some(role)) => Some(Observation::DefaultEndpointChanged { flow, role, endpoint_id: endpoint_id.map(EndpointId) }),
                    _ => None,
                }
            }
            DeviceWatchEvent::PropertyValueChanged { .. } => None, // not FSM-relevant
        };

        if let Some(observation) = observation {
            let effects = decide(&mut self.state, DecisionInput::Observation(observation));
            self.execute(effects)?;
        }
        Ok(())
    }

    fn handle_join_result(&mut self, result: JoinResult) -> Result<(), CaptureError> {
        self.pending_joins = self.pending_joins.saturating_sub(1);
        // Fed through decide() unconditionally rather than tracked/filtered here —
        // its own operation_id/epoch staleness guard is what makes a late or
        // already-superseded join result a safe no-op (see `reap_dead_worker`).
        let effects = decide(&mut self.state, DecisionInput::Observation(Observation::WorkerStopped { binding: result.binding, operation_id: result.operation_id, epoch: result.epoch }));
        self.execute(effects)?;

        match result.outcome {
            CaptureThreadOutcome::Stopped { exit, mmcss_applied } => {
                tracing::info!(binding = ?result.binding, ?exit, mmcss_applied, "capture worker stopped");
            }
            CaptureThreadOutcome::Errored { error, mmcss_applied } => {
                tracing::warn!(binding = ?result.binding, %error, mmcss_applied, "capture worker exited with error");
            }
        }
        Ok(())
    }

    fn drain_pending_joins(&mut self) {
        while self.pending_joins > 0 {
            match self.join_result_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(result) => {
                    let _ = self.handle_join_result(result);
                }
                Err(_) => break, // don't hang forever if a worker never reports back
            }
        }
    }
}

fn build_stream(binding: BindingKind, target: &ResolvedTarget, epoch: StreamEpoch, pipeline_drop_counter: Arc<AtomicU64>, callback_timeout_ms: u32) -> Box<dyn CaptureStream> {
    let device_id_or_default = match target {
        ResolvedTarget::Endpoint(id) => id.0.clone(),
        ResolvedTarget::Process { .. } => unreachable!("process loopback is not part of Phase 1A's bindings"),
    };
    match binding {
        BindingKind::Microphone => Box::new(MicCaptureStream {
            device_id_or_default,
            role: DeviceRole::Console,
            pipeline_drop_counter,
            callback_timeout_ms,
            capture_epoch: epoch.0,
        }),
        BindingKind::EndpointLoopback => Box::new(EndpointLoopbackStream {
            device_id_or_default,
            role: DeviceRole::Console,
            pipeline_drop_counter,
            callback_timeout_ms,
            capture_epoch: epoch.0,
        }),
        BindingKind::ProcessLoopback => unreachable!("process loopback is not part of Phase 1A's bindings"),
    }
}

/// Exponential backoff for `Effect::ScheduleRetry`, capped well below
/// `capture_api::rebinding::MAX_RETRY_ATTEMPTS`'s effective ceiling.
fn backoff_for_attempt(attempt: u32) -> Duration {
    let base_ms = 500u64;
    let max_ms = 30_000u64;
    Duration::from_millis(base_ms.saturating_mul(1u64 << attempt.min(6)).min(max_ms))
}
