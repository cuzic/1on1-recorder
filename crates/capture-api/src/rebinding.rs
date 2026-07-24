//! Capture Rebinding State Machine.
//!
//! A pure, OS-independent decision function for safely rebinding an audio capture
//! stream (microphone, endpoint loopback, or process loopback) whenever the device
//! it's bound to disappears, the system default device changes, or (for process
//! loopback) the target process exits or restarts.
//!
//! Following the Observation -> Admission -> Decision -> Effect pattern: whatever
//! actually talks to the OS (WASAPI, PipeWire, ScreenCaptureKit, ...) reports raw
//! facts as [`Observation`]s. [`decide`] is the *only* place state is written; it is a
//! pure function that takes no current time, no randomness, does no I/O, and spawns no
//! threads — it just turns `(DecisionState, DecisionInput)` into a list of [`Effect`]s
//! for the caller to actually execute. This makes the rebinding policy itself testable
//! without ever touching real hardware (see the scenario tests), and reusable across
//! completely different capture backends.
//!
//! [`Microphone`](BindingKind::Microphone), [`EndpointLoopback`](BindingKind::EndpointLoopback),
//! and [`ProcessLoopback`](BindingKind::ProcessLoopback) bindings are all handled by the
//! same `decide` function, but process loopback ignores endpoint-side observations
//! (`EndpointRemoved`/`DefaultEndpointChanged`) and only reacts to `ProcessExited`/
//! `ProcessRestarted`.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Identifiers and selection policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataFlow {
    Capture,
    Render,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceRole {
    Console,
    Multimedia,
    Communications,
}

/// Selection policy for a microphone/endpoint-loopback binding: either pinned to a
/// specific device, or following whatever the system's default device is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointSelection {
    FollowDefault { flow: DataFlow, role: DeviceRole },
    Pinned { endpoint_id: EndpointId },
}

/// What kind of target this binding manages. Used to route observations (e.g.
/// endpoint-side events never reach a `ProcessLoopback` binding).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingKind {
    Microphone,
    EndpointLoopback,
    ProcessLoopback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingSelection {
    Endpoint(EndpointSelection),
    /// Process loopback has no endpoint selection; `pid` is updated by the caller as
    /// process discovery (finding the target executable, watching for restarts)
    /// resolves it.
    Process { exe_name: String, pid: Option<u32> },
}

/// The concrete target `decide` has resolved a binding's selection to, passed to
/// [`Effect::StartCapture`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTarget {
    Endpoint(EndpointId),
    Process { pid: u32 },
}

// ---------------------------------------------------------------------------
// The state machine itself.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamEpoch(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectId(pub u64);

/// What to do once a pending stop finishes. Distinguishes "the device changed, so
/// resolve and restart immediately" from "the device disappeared, so wait" — a pinned
/// device that disappears must not silently fall back to a different device, and this
/// distinction is what keeps that from happening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterStop {
    ResolveAndStart,
    WaitForRecovery { reason: WaitReason },
    RemainStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReason {
    DeviceUnavailable,
    ProcessNotFound,
    RetryableFailure { attempt: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureBindingState {
    Stopped,
    Resolving {
        operation_id: OperationId,
    },
    Starting {
        operation_id: OperationId,
        proposed_epoch: StreamEpoch,
        target: ResolvedTarget,
    },
    Running {
        operation_id: OperationId,
        epoch: StreamEpoch,
        target: ResolvedTarget,
    },
    Stopping {
        operation_id: OperationId,
        epoch: StreamEpoch,
        next: AfterStop,
    },
    Waiting {
        reason: WaitReason,
    },
    Failed {
        cause: String,
    },
}

impl CaptureBindingState {
    /// Whether a frame carrying this epoch should be accepted. Frames from a stale
    /// (previous) epoch must never be treated as belonging to the current binding.
    pub fn accepts_epoch(&self, epoch: StreamEpoch) -> bool {
        matches!(self, CaptureBindingState::Running { epoch: e, .. } if *e == epoch)
    }

    /// Collapses the full lifecycle into the coarse classification a caller outside
    /// this crate (e.g. a UI) actually needs — "is this track's audio flowing right
    /// now, and if not, why." `Starting`/`Stopping`/`Resolving`/`Stopped` are all
    /// transient, expected states on the way to `Running` (or a deliberate stop), so
    /// they map to `Ok` rather than a spurious "unhealthy" flicker on every
    /// start/stop.
    pub fn health(&self) -> BindingHealth {
        match self {
            CaptureBindingState::Stopped
            | CaptureBindingState::Resolving { .. }
            | CaptureBindingState::Starting { .. }
            | CaptureBindingState::Running { .. }
            | CaptureBindingState::Stopping { .. } => BindingHealth::Ok,
            CaptureBindingState::Waiting { reason } => match reason {
                WaitReason::DeviceUnavailable | WaitReason::ProcessNotFound => BindingHealth::Unavailable,
                WaitReason::RetryableFailure { attempt } => BindingHealth::Retrying { attempt: *attempt },
            },
            CaptureBindingState::Failed { cause } => BindingHealth::Failed { reason: cause.clone() },
        }
    }
}

/// A UI-facing classification of [`CaptureBindingState`], collapsing the full
/// lifecycle into "is this binding's audio flowing, and if not, why" — see
/// [`CaptureBindingState::health`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingHealth {
    Ok,
    Unavailable,
    Retrying { attempt: u32 },
    Failed { reason: String },
}

#[derive(Debug, Clone)]
pub struct CaptureBinding {
    pub kind: BindingKind,
    pub selection: BindingSelection,
    pub lifecycle: CaptureBindingState,
    /// Consecutive-failure count. Lives on the binding itself (not inside the
    /// `lifecycle` enum) because it must survive the `Starting -> WorkerFailed ->
    /// Waiting -> retry -> Starting` cycle; storing it in a `Waiting` variant would
    /// lose the count every time `lifecycle` moves back to `Starting`.
    retry_attempt: u32,
}

impl CaptureBinding {
    pub fn new(kind: BindingKind, selection: BindingSelection) -> Self {
        Self {
            kind,
            selection,
            lifecycle: CaptureBindingState::Stopped,
            retry_attempt: 0,
        }
    }
}

/// Caps retries so an unrecoverable failure doesn't retry forever. Once exceeded, the
/// binding moves to `Failed` and `decide` stops producing effects for it.
pub const MAX_RETRY_ATTEMPTS: u32 = 5;

// ---------------------------------------------------------------------------
// DecisionState / Observation / DecisionInput / Effect
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct DecisionState {
    pub bindings: HashMap<BindingKind, CaptureBinding>,
    pub default_routes: HashMap<(DataFlow, DeviceRole), Option<EndpointId>>,
    next_operation_id: u64,
    next_stream_epoch: u64,
    next_effect_id: u64,
    pub shutdown: bool,
}

impl DecisionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_binding(mut self, binding: CaptureBinding) -> Self {
        self.bindings.insert(binding.kind, binding);
        self
    }

    fn alloc_operation_id(&mut self) -> OperationId {
        let id = OperationId(self.next_operation_id);
        self.next_operation_id += 1;
        id
    }

    fn alloc_stream_epoch(&mut self) -> StreamEpoch {
        let epoch = StreamEpoch(self.next_stream_epoch);
        self.next_stream_epoch += 1;
        epoch
    }

    fn alloc_effect_id(&mut self) -> EffectId {
        let id = EffectId(self.next_effect_id);
        self.next_effect_id += 1;
        id
    }
}

/// A raw fact reported by the OS or the worker that actually runs the capture.
/// `decide` never takes these at face value — it checks them against the current
/// lifecycle/operation_id/epoch before letting them change anything (the "Admission"
/// step: freshness and generation checks live in `decide`'s match guards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// A physical endpoint disappeared. Only `Microphone`/`EndpointLoopback` bindings
    /// react to this; `ProcessLoopback` ignores it.
    EndpointRemoved { endpoint_id: EndpointId },
    /// An endpoint (re)appeared. This is what lets a binding recover out of `Waiting`.
    EndpointAdded { endpoint_id: EndpointId },
    /// The system default device changed (`None` means "no default").
    DefaultEndpointChanged {
        flow: DataFlow,
        role: DeviceRole,
        endpoint_id: Option<EndpointId>,
    },
    /// The worker finished executing an [`Effect::StartCapture`].
    WorkerStarted {
        binding: BindingKind,
        operation_id: OperationId,
        epoch: StreamEpoch,
        target: ResolvedTarget,
    },
    /// The worker finished executing an [`Effect::StopCapture`].
    WorkerStopped {
        binding: BindingKind,
        operation_id: OperationId,
        epoch: StreamEpoch,
    },
    /// The worker hit an unrecoverable error (e.g. Windows'
    /// `AUDCLNT_E_DEVICE_INVALIDATED`, a session-disconnect callback, or the
    /// equivalent on another OS).
    WorkerFailed {
        binding: BindingKind,
        operation_id: OperationId,
        error: String,
    },
    /// `ProcessLoopback` only: the target process exited (not a restart).
    ProcessExited { pid: u32 },
    /// `ProcessLoopback` only: the target process restarted under a new PID.
    ProcessRestarted { old_pid: u32, new_pid: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserIntent {
    Start { binding: BindingKind },
    Stop { binding: BindingKind },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionInput {
    Observation(Observation),
    UserIntent(UserIntent),
    /// A timer firing for [`Effect::ScheduleRetry`]. Ignored if `retry_id` doesn't
    /// match the current state (guards against a stale timer firing late).
    RetryTimerFired { binding: BindingKind, retry_id: u64 },
    ShutdownRequested,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    StartCapture {
        effect_id: EffectId,
        binding: BindingKind,
        operation_id: OperationId,
        proposed_epoch: StreamEpoch,
        target: ResolvedTarget,
    },
    StopCapture {
        effect_id: EffectId,
        binding: BindingKind,
        operation_id: OperationId,
        epoch: StreamEpoch,
    },
    ScheduleRetry {
        effect_id: EffectId,
        binding: BindingKind,
        retry_id: u64,
        attempt: u32,
    },
}

fn resolve_target(
    selection: &BindingSelection,
    default_routes: &HashMap<(DataFlow, DeviceRole), Option<EndpointId>>,
) -> Option<ResolvedTarget> {
    match selection {
        // A pinned selection *is* the intent to use this exact ID; whether it
        // actually exists is discovered later via WorkerStarted/WorkerFailed. Never
        // silently substituting a different endpoint here is what the acceptance
        // criteria require.
        BindingSelection::Endpoint(EndpointSelection::Pinned { endpoint_id }) => {
            Some(ResolvedTarget::Endpoint(endpoint_id.clone()))
        }
        BindingSelection::Endpoint(EndpointSelection::FollowDefault { flow, role }) => {
            default_routes
                .get(&(*flow, *role))
                .cloned()
                .flatten()
                .map(ResolvedTarget::Endpoint)
        }
        BindingSelection::Process { pid, .. } => pid.map(|pid| ResolvedTarget::Process { pid }),
    }
}

/// Whether `selection` currently points at `target` (used to detect a no-op default
/// change: the binding is already connected to what just became the new default).
///
/// Must resolve `selection` the same way `resolve_target` does (including
/// `FollowDefault`, via `default_routes`) rather than only recognizing a pinned
/// id: this function's only real caller ever passes a `FollowDefault` selection
/// (`handle_observation`'s `DefaultEndpointChanged` arm filters to exactly that),
/// so an earlier version of this function that special-cased `Pinned` identity
/// alone made the "already connected to the new default" no-op permanently
/// unreachable — every redundant/duplicate default-device notification (which
/// `IMMNotificationClient` can genuinely emit for a device that hasn't actually
/// changed) caused an unnecessary stop-and-restart cycle instead of the no-op
/// this function's own doc comment promised.
fn selection_targets(
    selection: &BindingSelection,
    target: &ResolvedTarget,
    default_routes: &HashMap<(DataFlow, DeviceRole), Option<EndpointId>>,
) -> bool {
    resolve_target(selection, default_routes).as_ref() == Some(target)
}

fn resolve_target_id(selection: &BindingSelection) -> Option<EndpointId> {
    match selection {
        BindingSelection::Endpoint(EndpointSelection::Pinned { endpoint_id }) => {
            Some(endpoint_id.clone())
        }
        _ => None,
    }
}

/// The only place state is written. Takes no current time, no randomness, makes no
/// OS/hardware calls, does no file I/O, spawns no threads — every ID is allocated
/// deterministically from counters inside `state`.
pub fn decide(state: &mut DecisionState, input: DecisionInput) -> Vec<Effect> {
    match input {
        DecisionInput::ShutdownRequested => {
            state.shutdown = true;
            let mut effects = Vec::new();
            let kinds: Vec<BindingKind> = state.bindings.keys().copied().collect();
            for kind in kinds {
                let binding = state.bindings.get(&kind).unwrap();
                if let CaptureBindingState::Running {
                    operation_id, epoch, ..
                } = binding.lifecycle
                {
                    let effect_id = state.alloc_effect_id();
                    effects.push(Effect::StopCapture {
                        effect_id,
                        binding: kind,
                        operation_id,
                        epoch,
                    });
                    state.bindings.get_mut(&kind).unwrap().lifecycle = CaptureBindingState::Stopping {
                        operation_id,
                        epoch,
                        next: AfterStop::RemainStopped,
                    };
                }
            }
            effects
        }
        DecisionInput::UserIntent(UserIntent::Start { binding }) => {
            start_binding(state, binding).into_iter().collect()
        }
        DecisionInput::UserIntent(UserIntent::Stop { binding }) => {
            stop_binding(state, binding, AfterStop::RemainStopped)
                .into_iter()
                .collect()
        }
        DecisionInput::RetryTimerFired { binding, retry_id } => {
            handle_retry_timer(state, binding, retry_id)
        }
        DecisionInput::Observation(obs) => handle_observation(state, obs),
    }
}

fn start_binding(state: &mut DecisionState, kind: BindingKind) -> Option<Effect> {
    if state.shutdown {
        return None;
    }
    let binding = state.bindings.get(&kind)?;
    // Allow both a fresh start from Stopped and a retry from Waiting (device
    // recovered, or a retry timer fired). Anything else (Resolving/Starting/
    // Running/Stopping) is refused — never starting a new worker while one is still
    // stopping is guaranteed by this guard.
    if !matches!(
        binding.lifecycle,
        CaptureBindingState::Stopped | CaptureBindingState::Waiting { .. }
    ) {
        return None;
    }
    let target = resolve_target(&binding.selection, &state.default_routes);
    match target {
        Some(target) => {
            let operation_id = state.alloc_operation_id();
            let proposed_epoch = state.alloc_stream_epoch();
            let effect_id = state.alloc_effect_id();
            state.bindings.get_mut(&kind).unwrap().lifecycle = CaptureBindingState::Starting {
                operation_id,
                proposed_epoch,
                target: target.clone(),
            };
            Some(Effect::StartCapture {
                effect_id,
                binding: kind,
                operation_id,
                proposed_epoch,
                target,
            })
        }
        None => {
            state.bindings.get_mut(&kind).unwrap().lifecycle = CaptureBindingState::Waiting {
                reason: WaitReason::DeviceUnavailable,
            };
            None
        }
    }
}

/// Stops a running binding with a plan for what happens after. Only ever called from
/// a non-`Stopping` state, so "never start a new worker while stopping" holds
/// naturally.
fn stop_binding(state: &mut DecisionState, kind: BindingKind, next: AfterStop) -> Option<Effect> {
    let binding = state.bindings.get(&kind)?;
    let CaptureBindingState::Running {
        operation_id, epoch, ..
    } = binding.lifecycle
    else {
        return None;
    };
    let effect_id = state.alloc_effect_id();
    state.bindings.get_mut(&kind).unwrap().lifecycle = CaptureBindingState::Stopping {
        operation_id,
        epoch,
        next,
    };
    Some(Effect::StopCapture {
        effect_id,
        binding: kind,
        operation_id,
        epoch,
    })
}

fn handle_observation(state: &mut DecisionState, obs: Observation) -> Vec<Effect> {
    match obs {
        Observation::DefaultEndpointChanged {
            flow,
            role,
            endpoint_id,
        } => {
            state
                .default_routes
                .insert((flow, role), endpoint_id.clone());
            let mut effects = Vec::new();
            let affected: Vec<BindingKind> = state
                .bindings
                .iter()
                .filter(|(_, b)| {
                    matches!(
                        &b.selection,
                        BindingSelection::Endpoint(EndpointSelection::FollowDefault {
                            flow: f,
                            role: r,
                        }) if *f == flow && *r == role
                    )
                })
                .map(|(k, _)| *k)
                .collect();
            for kind in affected {
                let binding = state.bindings.get(&kind).unwrap();
                if let CaptureBindingState::Running { target, .. } = &binding.lifecycle {
                    if selection_targets(&binding.selection, target, &state.default_routes) {
                        continue; // Already connected to the new default; no-op.
                    }
                    if let Some(effect) = stop_binding(state, kind, AfterStop::ResolveAndStart) {
                        effects.push(effect);
                    }
                }
                // While Stopping/Starting/Waiting, just record the new default and
                // wait for WorkerStopped or a Waiting-triggered re-resolve.
            }
            effects
        }
        Observation::EndpointRemoved { endpoint_id } => {
            let mut effects = Vec::new();
            let affected: Vec<BindingKind> = state
                .bindings
                .iter()
                .filter(|(kind, b)| {
                    **kind != BindingKind::ProcessLoopback
                        && matches!(&b.lifecycle, CaptureBindingState::Running { target: ResolvedTarget::Endpoint(id), .. } if *id == endpoint_id)
                })
                .map(|(k, _)| *k)
                .collect();
            for kind in affected {
                if let Some(effect) = stop_binding(
                    state,
                    kind,
                    AfterStop::WaitForRecovery {
                        reason: WaitReason::DeviceUnavailable,
                    },
                ) {
                    effects.push(effect);
                }
            }
            effects
        }
        Observation::EndpointAdded { endpoint_id } => {
            let mut effects = Vec::new();
            let candidates: Vec<BindingKind> = state
                .bindings
                .iter()
                .filter(|(_, b)| {
                    matches!(&b.lifecycle, CaptureBindingState::Waiting { .. })
                        && resolve_target_id(&b.selection).as_ref() == Some(&endpoint_id)
                })
                .map(|(k, _)| *k)
                .collect();
            for kind in candidates {
                if let Some(effect) = start_binding(state, kind) {
                    effects.push(effect);
                }
            }
            effects
        }
        Observation::ProcessExited { pid } => {
            let mut effects = Vec::new();
            if let Some(kind) = process_binding_with_pid(state, pid) {
                if let Some(effect) = stop_binding(
                    state,
                    kind,
                    AfterStop::WaitForRecovery {
                        reason: WaitReason::ProcessNotFound,
                    },
                ) {
                    effects.push(effect);
                }
            }
            effects
        }
        Observation::ProcessRestarted { old_pid, new_pid } => {
            let mut effects = Vec::new();
            if let Some(kind) = process_binding_with_pid(state, old_pid) {
                if let BindingSelection::Process { pid, .. } =
                    &mut state.bindings.get_mut(&kind).unwrap().selection
                {
                    *pid = Some(new_pid);
                }
                if let Some(effect) = stop_binding(state, kind, AfterStop::ResolveAndStart) {
                    effects.push(effect);
                }
            }
            effects
        }
        Observation::WorkerStarted {
            binding: kind,
            operation_id,
            epoch,
            target,
        } => {
            let Some(binding) = state.bindings.get(&kind) else {
                return Vec::new();
            };
            match &binding.lifecycle {
                CaptureBindingState::Starting {
                    operation_id: expected_op,
                    proposed_epoch,
                    ..
                } if *expected_op == operation_id && *proposed_epoch == epoch => {
                    let b = state.bindings.get_mut(&kind).unwrap();
                    b.lifecycle = CaptureBindingState::Running {
                        operation_id,
                        epoch,
                        target,
                    };
                    b.retry_attempt = 0; // Reset the consecutive-failure count on success.
                    Vec::new()
                }
                // A late arrival from Stopping/a different operation is stale; discard it.
                _ => Vec::new(),
            }
        }
        Observation::WorkerStopped {
            binding: kind,
            operation_id,
            epoch,
        } => {
            let Some(binding) = state.bindings.get(&kind) else {
                return Vec::new();
            };
            let CaptureBindingState::Stopping {
                operation_id: expected_op,
                epoch: expected_epoch,
                next,
            } = &binding.lifecycle
            else {
                return Vec::new(); // Stale (already moved on to Running, etc.).
            };
            if *expected_op != operation_id || *expected_epoch != epoch {
                return Vec::new(); // Stale operation/epoch.
            }
            let next = next.clone();
            match next {
                AfterStop::RemainStopped => {
                    state.bindings.get_mut(&kind).unwrap().lifecycle = CaptureBindingState::Stopped;
                    Vec::new()
                }
                AfterStop::WaitForRecovery { reason } => {
                    state.bindings.get_mut(&kind).unwrap().lifecycle =
                        CaptureBindingState::Waiting { reason };
                    Vec::new()
                }
                AfterStop::ResolveAndStart => {
                    state.bindings.get_mut(&kind).unwrap().lifecycle = CaptureBindingState::Stopped;
                    start_binding(state, kind).into_iter().collect()
                }
            }
        }
        Observation::WorkerFailed {
            binding: kind,
            operation_id,
            error,
        } => {
            let Some(binding) = state.bindings.get(&kind) else {
                return Vec::new();
            };
            let current_op = match &binding.lifecycle {
                CaptureBindingState::Starting { operation_id, .. } => Some(*operation_id),
                CaptureBindingState::Running { operation_id, .. } => Some(*operation_id),
                _ => None,
            };
            if current_op != Some(operation_id) {
                return Vec::new(); // Stale.
            }
            // The consecutive-failure count lives on `CaptureBinding::retry_attempt`,
            // not inside `lifecycle` (see the note on that field): it must survive
            // the Starting transition.
            let attempt = binding.retry_attempt + 1;
            if attempt > MAX_RETRY_ATTEMPTS {
                let b = state.bindings.get_mut(&kind).unwrap();
                b.retry_attempt = attempt;
                b.lifecycle = CaptureBindingState::Failed {
                    cause: format!("giving up after {attempt} attempts: {error}"),
                };
                return Vec::new();
            }
            let b = state.bindings.get_mut(&kind).unwrap();
            b.retry_attempt = attempt;
            b.lifecycle = CaptureBindingState::Waiting {
                reason: WaitReason::RetryableFailure { attempt },
            };
            let effect_id = state.alloc_effect_id();
            vec![Effect::ScheduleRetry {
                effect_id,
                binding: kind,
                retry_id: attempt as u64,
                attempt,
            }]
        }
    }
}

fn process_binding_with_pid(state: &DecisionState, pid: u32) -> Option<BindingKind> {
    state
        .bindings
        .get(&BindingKind::ProcessLoopback)
        .filter(|b| matches!(&b.lifecycle, CaptureBindingState::Running { target: ResolvedTarget::Process { pid: p }, .. } if *p == pid))
        .map(|b| b.kind)
}

fn handle_retry_timer(state: &mut DecisionState, kind: BindingKind, retry_id: u64) -> Vec<Effect> {
    let Some(binding) = state.bindings.get(&kind) else {
        return Vec::new();
    };
    if !matches!(
        binding.lifecycle,
        CaptureBindingState::Waiting {
            reason: WaitReason::RetryableFailure { .. }
        }
    ) {
        return Vec::new(); // A stale timer, or already left Waiting for another reason.
    }
    if binding.retry_attempt as u64 != retry_id {
        return Vec::new(); // Stale retry_id (failed/succeeded again since then).
    }
    start_binding(state, kind).into_iter().collect()
}
