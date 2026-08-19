//! Executes `capture_api::rebinding::decide()`'s effects against real
//! `capture-macos` capture threads. Only compiled with the `macos-supervisor`
//! feature and only meaningful on macOS (`capture-macos` itself is macOS-only, and
//! has never been compiled — see its crate doc comment for why). Structurally
//! mirrors `windows_supervisor.rs`, but with one significant divergence — see
//! "Shared-stream reconciliation" below.
//!
//! Scope: same as `windows_supervisor.rs` — this module only manages
//! capture-worker *lifecycle* (start/stop/rebind). Feeding captured frames into
//! `audio-timeline`/`segment-store` is `macos_frame_collector`'s job.
//!
//! ## Shared-stream reconciliation (the one real structural divergence from Windows)
//!
//! `capture_api::rebinding::decide()` treats `Microphone` and
//! `EndpointLoopback`/`ProcessLoopback` as fully independent bindings, each with
//! its own `StartCapture`/`StopCapture` effects — a WASAPI-shaped assumption that
//! holds for Windows (two independent `IAudioClient` streams) but not for
//! ScreenCaptureKit, where **one** `SCStream` serves both outputs at once (see
//! `capture-macos::sc_stream`'s module doc comment). This module bridges that gap:
//! instead of one OS thread per binding (`workers: HashMap<BindingKind,
//! WorkerHandle>` owning its own `stop`/`join_handle`, as `windows_supervisor.rs`
//! does), bookkeeping (`operation_id`/`epoch`/`target`) is tracked per binding in
//! [`self.workers`](MacosSupervisor::workers), but the *actual* OS thread lives in
//! [`self.active_stream`](MacosSupervisor::active_stream) — one shared
//! [`SharedStream`] that can serve multiple bindings at once.
//!
//! [`reconcile_active_stream`](MacosSupervisor::reconcile_active_stream) is called
//! after every batch of `StartCapture`/`StopCapture` effects and decides whether
//! the current shared stream already covers exactly the desired binding set. When
//! it doesn't (a binding is starting or stopping while the other keeps running),
//! **the whole shared stream is torn down and rebuilt** — there is no way to add or
//! remove an output on an already-running `SCStream`. This means a binding that
//! didn't ask to rebind can still suffer a brief interruption if the *other*
//! binding needs to rebind — a real cost of ScreenCaptureKit's one-stream-two-
//! outputs design that WASAPI's two-independent-streams model doesn't have. The
//! survivor's bookkeeping (`operation_id`/`epoch`) is left untouched across this
//! internal restart — `decide()` is never told that binding stopped, since from its
//! perspective it never asked to. This is the single most novel, least-verified
//! part of this crate's design and deserves real scrutiny once a Mac is available
//! to observe actual `SCStream` teardown/restart timing.
//!
//! design.md §16.5's "never silently follow later OS default-device changes"
//! policy is preserved identically to Windows: [`resolve_current_defaults`]/
//! [`pin_devices`] resolve once at session start and pin, `start_all` never
//! constructs a `FollowDefault` binding.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use capture_api::rebinding::{
    decide, BindingKind, BindingSelection, CaptureBinding, DecisionInput, DecisionState,
    DeviceRole, Effect, EndpointId, EndpointSelection, Observation, OperationId, ResolvedTarget,
    StreamEpoch, UserIntent,
};
use crate::capture_health::CaptureHealth;
use capture_macos::device_watch::DeviceWatchEvent;
use capture_macos::sc_stream::{ScreenCaptureKitStream, StreamOutputs};
use capture_macos::{
    spawn_capture_thread, CaptureError, CaptureEvent, CaptureThreadOutcome, StopSignal,
};
use crossbeam_channel::{Receiver, Select, Sender};

/// Per-binding bookkeeping `decide()` needs echoed back as `Observation`s — the
/// macOS analogue of `windows_supervisor::WorkerHandle`, minus the OS thread handle
/// itself (which now lives on [`SharedStream`], not per binding).
struct WorkerHandle {
    operation_id: OperationId,
    epoch: StreamEpoch,
    target: ResolvedTarget,
}

/// The one real OS-level `SCStream` worker thread, possibly serving more than one
/// [`BindingKind`] at once. See this module's doc comment.
struct SharedStream {
    bindings: Vec<BindingKind>,
    /// Snapshot, at the moment this `SharedStream` was built, of which
    /// `operation_id` each of `bindings` was being served under — see
    /// `handle_capture_event`'s `StreamError` arm, which uses this (not just
    /// "is this binding's key still present in `self.workers`") to tell "a sibling
    /// this exact dead stream was serving hasn't reported its `StreamError` yet"
    /// apart from "that key now holds an unrelated, already-retried worker from a
    /// completely different failure". Without this distinction, a retry racing
    /// ahead of a slower sibling's `StreamError` could make the wait-for-siblings
    /// check below block forever.
    operation_ids: HashMap<BindingKind, OperationId>,
    stop: Arc<StopSignal>,
    join_handle: std::thread::JoinHandle<CaptureThreadOutcome>,
}

struct JoinResult {
    outcome: CaptureThreadOutcome,
}

/// Mirrors `windows_supervisor::FrameSinkEvent` exactly — see that type's doc
/// comment for why this is a forwarding sink, not a second receiver on the same
/// channel.
pub enum FrameSinkEvent {
    StreamStarted {
        binding: BindingKind,
        sample_rate: u32,
        channels: u16,
        nominal_frame_interval_ns: u64,
    },
    Frame {
        record: capture_macos::CapturedFrameRecord,
        samples: Vec<f32>,
    },
}

pub struct MacosSupervisor {
    state: DecisionState,
    workers: HashMap<BindingKind, WorkerHandle>,
    active_stream: Option<SharedStream>,
    capture_tx: Sender<CaptureEvent>,
    capture_rx: Receiver<CaptureEvent>,
    join_result_tx: Sender<JoinResult>,
    join_result_rx: Receiver<JoinResult>,
    retry_tx: Sender<(BindingKind, u64)>,
    retry_rx: Receiver<(BindingKind, u64)>,
    sample_rate_hz: u32,
    channels: u16,
    pending_joins: usize,
    frame_tx: Option<Sender<FrameSinkEvent>>,
    health_sink: Option<Arc<Mutex<CaptureHealth>>>,
    /// Last-seen union of capture+render device ids, seeded in `start_all` and
    /// rolled forward by `reconcile_device_list` on every
    /// `DeviceWatchEvent::DeviceListChanged` — see that method's doc comment.
    device_snapshot: capture_api::device_diff::DeviceSnapshot,
}

impl MacosSupervisor {
    /// Starts with no bindings — `pin_devices` must run before `start_all`, same
    /// contract as `WindowsSupervisor::new`. Bindings are never constructed as
    /// `EndpointSelection::FollowDefault`, for the same design.md §16.5 reason
    /// documented on `WindowsSupervisor::new`.
    pub fn new(sample_rate_hz: u32, channels: u16) -> Self {
        let (capture_tx, capture_rx) = crossbeam_channel::bounded(256);
        let (join_result_tx, join_result_rx) = crossbeam_channel::unbounded();
        let (retry_tx, retry_rx) = crossbeam_channel::unbounded();
        Self {
            state: DecisionState::new(),
            workers: HashMap::new(),
            active_stream: None,
            capture_tx,
            capture_rx,
            join_result_tx,
            join_result_rx,
            retry_tx,
            retry_rx,
            sample_rate_hz,
            channels,
            pending_joins: 0,
            frame_tx: None,
            health_sink: None,
            device_snapshot: capture_api::device_diff::DeviceSnapshot::default(),
        }
    }

    /// See `windows_supervisor::WindowsSupervisor::set_frame_sink`'s doc comment —
    /// identical rationale.
    pub fn set_frame_sink(&mut self, tx: Sender<FrameSinkEvent>) {
        self.frame_tx = Some(tx);
    }

    /// See `windows_supervisor::WindowsSupervisor::set_health_sink`'s doc comment —
    /// identical rationale.
    pub fn set_health_sink(&mut self, sink: Arc<Mutex<CaptureHealth>>) {
        self.health_sink = Some(sink);
    }

    /// See `windows_supervisor::WindowsSupervisor::capture_health`'s doc comment —
    /// identical rationale.
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

    /// Resolves "whatever's currently the default" for both bindings, mirroring
    /// `WindowsSupervisor::resolve_current_defaults`. CoreAudio has no `DeviceRole`
    /// equivalent (see `capture_macos::device_select`'s module doc comment) — the
    /// returned `EndpointId`s are always resolved against the single default
    /// input/output device.
    pub fn resolve_current_defaults(&self) -> Result<(EndpointId, EndpointId), CaptureError> {
        let capture_devices = capture_macos::device_select::enumerate_capture_devices()?;
        let render_devices = capture_macos::device_select::enumerate_render_devices()?;
        let mic = capture_devices
            .into_iter()
            .find(|d| d.is_default_for_role.is_some())
            .ok_or_else(|| CaptureError::DeviceNotFound("no default input device".into()))?;
        let render = render_devices
            .into_iter()
            .find(|d| d.is_default_for_role.is_some())
            .ok_or_else(|| CaptureError::DeviceNotFound("no default output device".into()))?;
        Ok((EndpointId(mic.id), EndpointId(render.id)))
    }

    /// Pins Microphone and EndpointLoopback to specific endpoints — same contract
    /// as `WindowsSupervisor::pin_devices`.
    pub fn pin_devices(
        &mut self,
        microphone_endpoint_id: EndpointId,
        render_endpoint_id: EndpointId,
    ) {
        self.state.bindings.insert(
            BindingKind::Microphone,
            CaptureBinding::new(
                BindingKind::Microphone,
                BindingSelection::Endpoint(EndpointSelection::Pinned {
                    endpoint_id: microphone_endpoint_id,
                }),
            ),
        );
        self.state.bindings.insert(
            BindingKind::EndpointLoopback,
            CaptureBinding::new(
                BindingKind::EndpointLoopback,
                BindingSelection::Endpoint(EndpointSelection::Pinned {
                    endpoint_id: render_endpoint_id,
                }),
            ),
        );
    }

    /// Unlike `WindowsSupervisor::start_all` (which calls `decide()` once per
    /// binding, each landing in its own `execute()` call), this collects both
    /// bindings' effects into one batch before executing — so
    /// `reconcile_active_stream` sees both `StartCapture`s together and spawns
    /// exactly one shared stream covering both, instead of restarting once per
    /// binding.
    pub fn start_all(&mut self) -> Result<(), CaptureError> {
        // Seeds `device_snapshot` so the first `DeviceListChanged` after this only
        // reports what actually changed since session start, not every currently-
        // present device as freshly "added". A failed enumeration here just leaves
        // the snapshot empty (logged, not fatal) — the very next successful
        // `reconcile_device_list` self-heals it, at the cost of that one round
        // reporting spurious `EndpointAdded`s for everything already present,
        // which `decide()` no-ops for any binding not currently `Waiting`.
        match self.enumerate_device_snapshot() {
            Ok(snapshot) => self.device_snapshot = snapshot,
            Err(err) => tracing::warn!(%err, "failed to seed initial macOS device snapshot"),
        }

        let mut effects = Vec::new();
        for binding in [BindingKind::Microphone, BindingKind::EndpointLoopback] {
            effects.extend(decide(
                &mut self.state,
                DecisionInput::UserIntent(UserIntent::Start { binding }),
            ));
        }
        self.execute(effects)
    }

    /// The union of capture+render device ids right now — see `device_snapshot`'s
    /// doc comment on why one merged set (not two per-flow sets) is correct.
    fn enumerate_device_snapshot(&self) -> Result<capture_api::device_diff::DeviceSnapshot, CaptureError> {
        let capture_devices = capture_macos::device_select::enumerate_capture_devices()?;
        let render_devices = capture_macos::device_select::enumerate_render_devices()?;
        let ids = capture_devices
            .into_iter()
            .map(|d| EndpointId(d.id))
            .chain(render_devices.into_iter().map(|d| EndpointId(d.id)));
        Ok(capture_api::device_diff::DeviceSnapshot::from_ids(ids))
    }

    /// Turns a `DeviceWatchEvent::DeviceListChanged` notification (which by itself
    /// only says "something changed", not what — see that variant's doc comment)
    /// into real `EndpointAdded`/`EndpointRemoved` observations, by re-enumerating
    /// both device lists and diffing against `device_snapshot`. This is what lets
    /// a *non-default* pinned device's disconnect/reconnect recover via the same
    /// `Waiting`/retry path a default-device change already gets — the gap this
    /// module's doc comment used to flag as unimplemented. Always runs on this
    /// supervisor's own thread (never the CoreAudio callback thread), matching
    /// `DeviceWatch::start`'s "the consumer re-enumerates in response" contract.
    fn reconcile_device_list(&mut self) -> Result<(), CaptureError> {
        // A transient enumeration failure (e.g. CoreAudio queried mid-disconnect)
        // must not tear down the whole session over what's ultimately routine
        // device-list churn — same non-fatal treatment as `start_all`'s seeding.
        // Skipping this round (rather than rolling `device_snapshot` forward on a
        // partial/failed read) leaves it self-healing: the next successful
        // `DeviceListChanged` diffs against the last known-good snapshot instead
        // of a corrupted one.
        let next = match self.enumerate_device_snapshot() {
            Ok(next) => next,
            Err(err) => {
                tracing::warn!(%err, "failed to re-enumerate macOS devices after a device-list-changed notification; skipping this reconcile round");
                return Ok(());
            }
        };
        let delta = self.device_snapshot.diff_and_update(next);

        let mut effects = Vec::new();
        // Removed before added: mirrors the order a device physically
        // disappearing-then-reappearing would naturally produce, and ensures a
        // binding that's simultaneously losing one pinned device and gaining
        // another (a rare but possible single-diff scenario) processes the loss
        // first.
        for endpoint_id in delta.removed {
            effects.extend(decide(&mut self.state, DecisionInput::Observation(Observation::EndpointRemoved { endpoint_id })));
        }
        for endpoint_id in delta.added {
            effects.extend(decide(&mut self.state, DecisionInput::Observation(Observation::EndpointAdded { endpoint_id })));
        }
        self.execute(effects)
    }

    /// Same shape/contract as `WindowsSupervisor::run_until_shutdown`.
    pub fn run_until_shutdown(
        &mut self,
        device_watch_rx: &Receiver<DeviceWatchEvent>,
        shutdown_rx: &Receiver<()>,
    ) -> Result<(), CaptureError> {
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
                    self.handle_join_result(result);
                }
            } else if index == retry_idx {
                if let Ok((binding, retry_id)) = oper.recv(&self.retry_rx) {
                    let effects = decide(
                        &mut self.state,
                        DecisionInput::RetryTimerFired { binding, retry_id },
                    );
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
        let mut binding_set_changed = false;

        for effect in effects {
            match effect {
                Effect::StartCapture {
                    binding,
                    operation_id,
                    proposed_epoch,
                    target,
                    ..
                } => {
                    self.workers.insert(
                        binding,
                        WorkerHandle {
                            operation_id,
                            epoch: proposed_epoch,
                            target,
                        },
                    );
                    binding_set_changed = true;
                }
                Effect::StopCapture {
                    binding,
                    operation_id,
                    epoch,
                    ..
                } => {
                    self.workers.remove(&binding);
                    binding_set_changed = true;
                    // Unlike Windows (which waits for the real thread join before
                    // feeding WorkerStopped), this is fed immediately — see this
                    // module's doc comment on why the shared-stream reconciliation
                    // below can't wait for a per-binding join the way Windows does.
                    let effects = decide(
                        &mut self.state,
                        DecisionInput::Observation(Observation::WorkerStopped {
                            binding,
                            operation_id,
                            epoch,
                        }),
                    );
                    // Recursing here (rather than looping) is safe: `decide()`'s own
                    // response to a WorkerStopped it already expected is typically
                    // either nothing or an immediate re-`StartCapture` (rebind), never
                    // another StopCapture for the same binding — no risk of unbounded
                    // recursion in practice, matching the depth `decide()`'s own
                    // scenario tests exercise.
                    self.execute(effects)?;
                }
                Effect::ScheduleRetry {
                    binding,
                    retry_id,
                    attempt,
                    ..
                } => {
                    let retry_tx = self.retry_tx.clone();
                    let delay = backoff_for_attempt(attempt);
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        let _ = retry_tx.send((binding, retry_id));
                    });
                }
            }
        }

        if binding_set_changed {
            self.reconcile_active_stream()?;
        }
        Ok(())
    }

    /// See this module's top-level doc comment ("Shared-stream reconciliation").
    fn reconcile_active_stream(&mut self) -> Result<(), CaptureError> {
        let desired: std::collections::HashSet<BindingKind> =
            self.workers.keys().copied().collect();
        let current: std::collections::HashSet<BindingKind> = self
            .active_stream
            .as_ref()
            .map(|s| s.bindings.iter().copied().collect())
            .unwrap_or_default();

        if desired == current {
            return Ok(());
        }
        let desired: Vec<BindingKind> = desired.into_iter().collect();

        if let Some(old) = self.active_stream.take() {
            old.stop.signal();
            self.pending_joins += 1;
            let join_result_tx = self.join_result_tx.clone();
            std::thread::spawn(move || {
                let outcome = old
                    .join_handle
                    .join()
                    .expect("shared capture-macos worker thread panicked");
                let _ = join_result_tx.send(JoinResult { outcome });
            });
        }

        if desired.is_empty() {
            return Ok(());
        }

        let outputs = StreamOutputs {
            microphone: desired.contains(&BindingKind::Microphone),
            system_audio: desired.contains(&BindingKind::EndpointLoopback)
                || desired.contains(&BindingKind::ProcessLoopback),
        };
        let system_audio_binding = if desired.contains(&BindingKind::ProcessLoopback) {
            BindingKind::ProcessLoopback
        } else {
            BindingKind::EndpointLoopback
        };

        // Each binding gets *its own* epoch — `decide()` allocates a fresh
        // `StreamEpoch` per binding independently (`start_all` starting Microphone
        // then EndpointLoopback in two separate `decide()` calls means they land on
        // consecutive-but-distinct values), even though both outputs are about to
        // share one `SCStream`/thread. A single stream-wide epoch (this used to take
        // `desired`'s *maximum* worker epoch) tags every frame from *both* outputs
        // with the same value, which `capture_api::rebinding::DecisionState::accepts_epoch`
        // then checks per-binding against each binding's own `Running.epoch` — so
        // whichever binding didn't happen to hold the max would have every one of
        // its frames rejected as stale, permanently, the moment both bindings were
        // ever started in the same `reconcile_active_stream` generation (i.e. every
        // normal session with both Microphone and EndpointLoopback active). See
        // docs/adr/0012-accepts-epoch-guard-never-called.md.
        let microphone_epoch = self.workers.get(&BindingKind::Microphone).map(|w| w.epoch.0);
        let system_audio_epoch = self.workers.get(&system_audio_binding).map(|w| w.epoch.0);

        // Phase 1A only ever pins endpoints (see `pin_devices`) — `ProcessLoopback`'s
        // app-filtered SCContentFilter construction (`capture_macos::app_filter`)
        // is future work once a caller actually selects a specific application
        // (design.md §5.2 step 1/2); for now every stream uses an unfiltered filter.
        let filter = unfiltered_display_filter()?;

        // The pinned Microphone endpoint (if any) needs to reach
        // `SCStreamConfiguration::set_microphone_capture_device_id` — `decide()`'s
        // `ResolvedTarget` already carries it in `self.workers`, it just wasn't
        // being read before.
        let microphone_device_id = if outputs.microphone {
            self.workers
                .get(&BindingKind::Microphone)
                .and_then(|w| match &w.target {
                    ResolvedTarget::Endpoint(endpoint_id) => Some(endpoint_id.0.clone()),
                    ResolvedTarget::Process { .. } => None,
                })
        } else {
            None
        };

        let stream = ScreenCaptureKitStream::new(
            filter,
            self.sample_rate_hz,
            self.channels,
            outputs,
            system_audio_binding,
            microphone_epoch,
            system_audio_epoch,
            microphone_device_id,
        );
        let operation_ids = desired
            .iter()
            .filter_map(|b| self.workers.get(b).map(|w| (*b, w.operation_id)))
            .collect();
        let stop = Arc::new(StopSignal::new());
        let join_handle =
            spawn_capture_thread(Box::new(stream), self.capture_tx.clone(), stop.clone());
        self.active_stream = Some(SharedStream {
            bindings: desired,
            operation_ids,
            stop,
            join_handle,
        });
        Ok(())
    }

    fn handle_capture_event(&mut self, event: CaptureEvent) -> Result<(), CaptureError> {
        match event {
            CaptureEvent::Frame { record, samples } => {
                // See `windows_supervisor::WindowsSupervisor::handle_capture_event`'s
                // identical check — reject frames from a stale epoch (e.g. still in
                // flight from a shared SCStream `reconcile_active_stream` just tore
                // down to rebuild) rather than forwarding them as if they belonged
                // to the binding's current generation.
                if self.state.accepts_epoch(record.stream, StreamEpoch(record.capture_epoch)) {
                    if let Some(tx) = &self.frame_tx {
                        let _ = tx.send(FrameSinkEvent::Frame { record, samples });
                    }
                }
            }
            CaptureEvent::StreamStarted {
                stream,
                sample_rate,
                channels,
                nominal_frame_interval_ns,
            } => {
                if let Some(tx) = &self.frame_tx {
                    let _ = tx.send(FrameSinkEvent::StreamStarted {
                        binding: stream,
                        sample_rate,
                        channels,
                        nominal_frame_interval_ns,
                    });
                }
                if let Some(worker) = self.workers.get(&stream) {
                    let (operation_id, epoch, target) =
                        (worker.operation_id, worker.epoch, worker.target.clone());
                    let effects = decide(
                        &mut self.state,
                        DecisionInput::Observation(Observation::WorkerStarted {
                            binding: stream,
                            operation_id,
                            epoch,
                            target,
                        }),
                    );
                    self.execute(effects)?;
                }
            }
            CaptureEvent::StreamError { stream, error } => {
                if let Some(operation_id) = self.workers.get(&stream).map(|w| w.operation_id) {
                    let effects = decide(
                        &mut self.state,
                        DecisionInput::Observation(Observation::WorkerFailed {
                            binding: stream,
                            operation_id,
                            error,
                        }),
                    );
                    self.workers.remove(&stream);
                    self.execute(effects)?;

                    // The one shared `SCStream` behind `self.active_stream` dies as a
                    // whole and serves every binding in `active_stream.bindings` at
                    // once — `capture-macos`'s `lib.rs` sends one `StreamError` per
                    // binding it was serving, in quick succession (see that crate's
                    // `spawn_capture_thread` doc comment). Reconciling right after the
                    // *first* one alone would compute a transient desired set (still
                    // including the sibling binding(s) whose `StreamError` hasn't been
                    // processed yet), spawning a brand-new `SCStream` just to tear it
                    // down again moments later once the sibling's `StreamError` does
                    // arrive. Wait until every sibling this dead stream's
                    // `operation_ids` snapshot recorded has reported in before
                    // reconciling, collapsing that churn into a single rebuild.
                    //
                    // Checked against `SharedStream::operation_ids` (each sibling's
                    // operation_id *as of this stream's construction*), not merely
                    // "is the binding's key still present in `self.workers`": if a
                    // sibling's own retry (a completely independent failure/backoff
                    // timeline) raced ahead and already re-inserted a *new* worker
                    // under that same `BindingKind` key before its `StreamError` for
                    // *this* dead generation arrived, `contains_key` alone would
                    // read that as "still waiting" forever — this binding would
                    // never reconcile out of `Starting`. Comparing operation_ids
                    // tells the two apart: a re-inserted worker has a different
                    // operation_id than the one this dead stream was serving.
                    let still_awaiting_siblings = self.active_stream.as_ref().is_some_and(|s| {
                        s.operation_ids.iter().any(|(binding, operation_id)| {
                            self.workers.get(binding).map(|w| w.operation_id) == Some(*operation_id)
                        })
                    });
                    if !still_awaiting_siblings {
                        self.reconcile_active_stream()?;
                    }
                }
            }
            CaptureEvent::StreamStopped { .. } => {
                // Informational only, same rationale as
                // `windows_supervisor::WindowsSupervisor::handle_capture_event`'s
                // identical arm — the join result is authoritative, not this event.
            }
        }
        Ok(())
    }

    fn handle_device_watch_event(&mut self, event: DeviceWatchEvent) -> Result<(), CaptureError> {
        // Default-device changes carry a resolved endpoint id directly from
        // CoreAudio and are turned into an `Observation` below like Windows'
        // `IMMNotificationClient` events are. `DeviceListChanged` is different —
        // CoreAudio's listener can't say what changed, only that something did —
        // so it's routed to `reconcile_device_list`'s enumerate-and-diff instead
        // of being mapped to a single `Observation` here (see that method's doc
        // comment, and `DeviceWatchEvent::DeviceListChanged`'s).
        let observation = match event {
            DeviceWatchEvent::DefaultInputDeviceChanged { device_uid } => {
                device_uid.map(|id| Observation::DefaultEndpointChanged {
                    flow: capture_api::rebinding::DataFlow::Capture,
                    role: DeviceRole::Console,
                    endpoint_id: Some(EndpointId(id)),
                })
            }
            DeviceWatchEvent::DefaultOutputDeviceChanged { device_uid } => {
                device_uid.map(|id| Observation::DefaultEndpointChanged {
                    flow: capture_api::rebinding::DataFlow::Render,
                    role: DeviceRole::Console,
                    endpoint_id: Some(EndpointId(id)),
                })
            }
            DeviceWatchEvent::DeviceListChanged => {
                self.reconcile_device_list()?;
                None
            }
            // Not emitted by `capture_macos::device_watch` today (superseded by
            // `DeviceListChanged` above) — kept as accepted-but-inert variants
            // for symmetry with Windows' richer per-event `DeviceWatchEvent`,
            // should a future revision resolve identity inline after all.
            DeviceWatchEvent::DeviceAdded { device_uid } if !device_uid.is_empty() => {
                Some(Observation::EndpointAdded {
                    endpoint_id: EndpointId(device_uid),
                })
            }
            DeviceWatchEvent::DeviceRemoved { device_uid } if !device_uid.is_empty() => {
                Some(Observation::EndpointRemoved {
                    endpoint_id: EndpointId(device_uid),
                })
            }
            DeviceWatchEvent::DeviceAdded { .. } | DeviceWatchEvent::DeviceRemoved { .. } => None,
            DeviceWatchEvent::ApplicationTerminated { pid, .. } => {
                Some(Observation::ProcessExited { pid })
            }
            DeviceWatchEvent::ApplicationLaunched { .. } => None, // paired into ProcessRestarted by a future revision; see device_watch.rs
        };

        if let Some(observation) = observation {
            let effects = decide(&mut self.state, DecisionInput::Observation(observation));
            self.execute(effects)?;
        }
        Ok(())
    }

    fn handle_join_result(&mut self, result: JoinResult) {
        self.pending_joins = self.pending_joins.saturating_sub(1);
        match result.outcome {
            CaptureThreadOutcome::Stopped { exit } => {
                tracing::info!(?exit, "shared capture-macos worker stopped");
            }
            CaptureThreadOutcome::Errored { error } => {
                tracing::warn!(%error, "shared capture-macos worker exited with error");
            }
        }
    }

    fn drain_pending_joins(&mut self) {
        while self.pending_joins > 0 {
            match self.join_result_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(result) => self.handle_join_result(result),
                Err(_) => break,
            }
        }
    }
}

/// Phase 1A only pins endpoints, never a specific application — so every shared
/// stream this supervisor builds uses an unfiltered content filter over the
/// primary display (`capture_macos::app_filter::unfiltered`).
/// `BindingKind::ProcessLoopback` support (an app-scoped filter) is future work
/// once a caller actually selects an application.
fn unfiltered_display_filter(
) -> Result<screencapturekit::stream::content_filter::SCContentFilter, CaptureError> {
    let content = screencapturekit::shareable_content::SCShareableContent::get()
        .map_err(|err| CaptureError::ScreenCaptureKit(err.to_string()))?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or_else(|| CaptureError::DeviceNotFound("no display available".into()))?;
    Ok(capture_macos::app_filter::unfiltered(&display))
}

/// Exponential backoff for `Effect::ScheduleRetry` — identical policy to
/// `windows_supervisor::backoff_for_attempt`.
fn backoff_for_attempt(attempt: u32) -> Duration {
    let base_ms = 500u64;
    let max_ms = 30_000u64;
    Duration::from_millis(base_ms.saturating_mul(1u64 << attempt.min(6)).min(max_ms))
}
