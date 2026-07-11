//! Four event-sequence scenarios plus invariant checks for the rebinding state
//! machine. These call `decide` directly with no real WASAPI/PipeWire/CoreAudio
//! involved at all — a "fake worker" style test of the policy alone.

use capture_api::rebinding::*;

fn endpoint(id: &str) -> EndpointId {
    EndpointId(id.to_string())
}

// ---------------------------------------------------------------------------
// (a) A FollowDefault microphone rebinds to the new default (A -> B) only after
//     the old worker's stop completes.
// ---------------------------------------------------------------------------
#[test]
fn follow_default_mic_rebinds_after_old_worker_stops() {
    let mut state = DecisionState::new().with_binding(CaptureBinding::new(
        BindingKind::Microphone,
        BindingSelection::Endpoint(EndpointSelection::FollowDefault {
            flow: DataFlow::Capture,
            role: DeviceRole::Console,
        }),
    ));

    decide(
        &mut state,
        DecisionInput::Observation(Observation::DefaultEndpointChanged {
            flow: DataFlow::Capture,
            role: DeviceRole::Console,
            endpoint_id: Some(endpoint("MicA")),
        }),
    );

    let effects = decide(
        &mut state,
        DecisionInput::UserIntent(UserIntent::Start {
            binding: BindingKind::Microphone,
        }),
    );
    let (op_a, epoch_a) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            target,
            ..
        }] => {
            assert_eq!(*target, ResolvedTarget::Endpoint(endpoint("MicA")));
            (*operation_id, *proposed_epoch)
        }
        other => panic!("expected single StartCapture, got {other:?}"),
    };

    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::Microphone,
            operation_id: op_a,
            epoch: epoch_a,
            target: ResolvedTarget::Endpoint(endpoint("MicA")),
        }),
    );
    assert_eq!(
        state.bindings[&BindingKind::Microphone].lifecycle,
        CaptureBindingState::Running {
            operation_id: op_a,
            epoch: epoch_a,
            target: ResolvedTarget::Endpoint(endpoint("MicA")),
        }
    );

    // The default device changes from A to B.
    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::DefaultEndpointChanged {
            flow: DataFlow::Capture,
            role: DeviceRole::Console,
            endpoint_id: Some(endpoint("MicB")),
        }),
    );
    assert_eq!(
        effects,
        vec![Effect::StopCapture {
            effect_id: match effects[0] {
                Effect::StopCapture { effect_id, .. } => effect_id,
                _ => unreachable!(),
            },
            binding: BindingKind::Microphone,
            operation_id: op_a,
            epoch: epoch_a,
        }]
    );
    // Must not connect to B before A's stop has completed.
    assert!(matches!(
        state.bindings[&BindingKind::Microphone].lifecycle,
        CaptureBindingState::Stopping { .. }
    ));

    // Only once A's stop completes does StartCapture for B appear.
    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStopped {
            binding: BindingKind::Microphone,
            operation_id: op_a,
            epoch: epoch_a,
        }),
    );
    let (op_b, epoch_b) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            target,
            ..
        }] => {
            assert_eq!(*target, ResolvedTarget::Endpoint(endpoint("MicB")));
            (*operation_id, *proposed_epoch)
        }
        other => panic!("expected single StartCapture to MicB, got {other:?}"),
    };
    assert_ne!(op_a, op_b, "a new operation_id must be allocated");
    assert_ne!(epoch_a, epoch_b, "a new stream_epoch must be allocated");

    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::Microphone,
            operation_id: op_b,
            epoch: epoch_b,
            target: ResolvedTarget::Endpoint(endpoint("MicB")),
        }),
    );
    assert_eq!(
        state.bindings[&BindingKind::Microphone].lifecycle,
        CaptureBindingState::Running {
            operation_id: op_b,
            epoch: epoch_b,
            target: ResolvedTarget::Endpoint(endpoint("MicB")),
        }
    );
}

// ---------------------------------------------------------------------------
// (b) A pinned microphone disconnecting moves to Waiting rather than silently
//     falling back to the default device, and recovers on reconnection.
// ---------------------------------------------------------------------------
#[test]
fn pinned_mic_waits_for_recovery_without_falling_back_to_default() {
    let mut state = DecisionState::new().with_binding(CaptureBinding::new(
        BindingKind::Microphone,
        BindingSelection::Endpoint(EndpointSelection::Pinned {
            endpoint_id: endpoint("MicA"),
        }),
    ));
    // The default device is B (distinct from A), specifically to prove that A
    // disappearing never falls back to B.
    state
        .default_routes
        .insert((DataFlow::Capture, DeviceRole::Console), Some(endpoint("MicB")));

    let effects = decide(
        &mut state,
        DecisionInput::UserIntent(UserIntent::Start {
            binding: BindingKind::Microphone,
        }),
    );
    let (op0, epoch0) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            target,
            ..
        }] => {
            assert_eq!(*target, ResolvedTarget::Endpoint(endpoint("MicA")));
            (*operation_id, *proposed_epoch)
        }
        other => panic!("expected StartCapture to MicA, got {other:?}"),
    };
    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::Microphone,
            operation_id: op0,
            epoch: epoch0,
            target: ResolvedTarget::Endpoint(endpoint("MicA")),
        }),
    );

    // A is physically unplugged.
    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::EndpointRemoved {
            endpoint_id: endpoint("MicA"),
        }),
    );
    assert!(matches!(effects.as_slice(), [Effect::StopCapture { .. }]));

    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStopped {
            binding: BindingKind::Microphone,
            operation_id: op0,
            epoch: epoch0,
        }),
    );
    // Moves to Waiting; no effect attempting to connect to B is ever produced.
    assert_eq!(effects, vec![]);
    assert_eq!(
        state.bindings[&BindingKind::Microphone].lifecycle,
        CaptureBindingState::Waiting {
            reason: WaitReason::DeviceUnavailable,
        }
    );

    // A is reconnected.
    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::EndpointAdded {
            endpoint_id: endpoint("MicA"),
        }),
    );
    let (op1, epoch1) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            target,
            ..
        }] => {
            assert_eq!(*target, ResolvedTarget::Endpoint(endpoint("MicA")));
            (*operation_id, *proposed_epoch)
        }
        other => panic!("expected StartCapture back to MicA, got {other:?}"),
    };
    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::Microphone,
            operation_id: op1,
            epoch: epoch1,
            target: ResolvedTarget::Endpoint(endpoint("MicA")),
        }),
    );
    assert_eq!(
        state.bindings[&BindingKind::Microphone].lifecycle,
        CaptureBindingState::Running {
            operation_id: op1,
            epoch: epoch1,
            target: ResolvedTarget::Endpoint(endpoint("MicA")),
        },
        "must recover to MicA (not fall back to MicB)"
    );
}

// ---------------------------------------------------------------------------
// (c) A FollowDefault speaker-loopback binding stops the old stream and starts
//     the new endpoint when the default output changes.
// ---------------------------------------------------------------------------
#[test]
fn follow_default_endpoint_loopback_rebinds_after_old_worker_stops() {
    let mut state = DecisionState::new().with_binding(CaptureBinding::new(
        BindingKind::EndpointLoopback,
        BindingSelection::Endpoint(EndpointSelection::FollowDefault {
            flow: DataFlow::Render,
            role: DeviceRole::Console,
        }),
    ));
    decide(
        &mut state,
        DecisionInput::Observation(Observation::DefaultEndpointChanged {
            flow: DataFlow::Render,
            role: DeviceRole::Console,
            endpoint_id: Some(endpoint("SpeakerA")),
        }),
    );
    let effects = decide(
        &mut state,
        DecisionInput::UserIntent(UserIntent::Start {
            binding: BindingKind::EndpointLoopback,
        }),
    );
    let (op_a, epoch_a) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            target,
            ..
        }] => {
            assert_eq!(*target, ResolvedTarget::Endpoint(endpoint("SpeakerA")));
            (*operation_id, *proposed_epoch)
        }
        other => panic!("unexpected effects: {other:?}"),
    };
    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::EndpointLoopback,
            operation_id: op_a,
            epoch: epoch_a,
            target: ResolvedTarget::Endpoint(endpoint("SpeakerA")),
        }),
    );

    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::DefaultEndpointChanged {
            flow: DataFlow::Render,
            role: DeviceRole::Console,
            endpoint_id: Some(endpoint("SpeakerB")),
        }),
    );
    assert!(matches!(effects.as_slice(), [Effect::StopCapture { .. }]));

    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStopped {
            binding: BindingKind::EndpointLoopback,
            operation_id: op_a,
            epoch: epoch_a,
        }),
    );
    let (op_b, epoch_b) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            target,
            ..
        }] => {
            assert_eq!(*target, ResolvedTarget::Endpoint(endpoint("SpeakerB")));
            (*operation_id, *proposed_epoch)
        }
        other => panic!("unexpected effects: {other:?}"),
    };
    assert_ne!(op_a, op_b);
    assert_ne!(epoch_a, epoch_b);

    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::EndpointLoopback,
            operation_id: op_b,
            epoch: epoch_b,
            target: ResolvedTarget::Endpoint(endpoint("SpeakerB")),
        }),
    );
    assert_eq!(
        state.bindings[&BindingKind::EndpointLoopback].lifecycle,
        CaptureBindingState::Running {
            operation_id: op_b,
            epoch: epoch_b,
            target: ResolvedTarget::Endpoint(endpoint("SpeakerB")),
        }
    );
}

// ---------------------------------------------------------------------------
// (d) Process loopback keeps running through default-output changes/endpoint
//     removal, and only re-resolves the PID (with a new epoch) when the target
//     process itself restarts.
// ---------------------------------------------------------------------------
#[test]
fn process_loopback_ignores_endpoint_events_but_rebinds_on_process_restart() {
    let mut state = DecisionState::new().with_binding(CaptureBinding::new(
        BindingKind::ProcessLoopback,
        BindingSelection::Process {
            exe_name: "Zoom.exe".to_string(),
            pid: Some(1111),
        },
    ));

    let effects = decide(
        &mut state,
        DecisionInput::UserIntent(UserIntent::Start {
            binding: BindingKind::ProcessLoopback,
        }),
    );
    let (op0, epoch0) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            target,
            ..
        }] => {
            assert_eq!(*target, ResolvedTarget::Process { pid: 1111 });
            (*operation_id, *proposed_epoch)
        }
        other => panic!("unexpected effects: {other:?}"),
    };
    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::ProcessLoopback,
            operation_id: op0,
            epoch: epoch0,
            target: ResolvedTarget::Process { pid: 1111 },
        }),
    );

    // Default output changes, endpoint removed: process loopback must not react.
    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::DefaultEndpointChanged {
            flow: DataFlow::Render,
            role: DeviceRole::Console,
            endpoint_id: Some(endpoint("SpeakerX")),
        }),
    );
    assert_eq!(effects, vec![]);
    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::EndpointRemoved {
            endpoint_id: endpoint("SpeakerX"),
        }),
    );
    assert_eq!(effects, vec![]);
    assert_eq!(
        state.bindings[&BindingKind::ProcessLoopback].lifecycle,
        CaptureBindingState::Running {
            operation_id: op0,
            epoch: epoch0,
            target: ResolvedTarget::Process { pid: 1111 },
        },
        "endpoint-only events must not affect process loopback"
    );

    // The target app restarts under a new PID.
    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::ProcessRestarted {
            old_pid: 1111,
            new_pid: 2222,
        }),
    );
    assert!(matches!(effects.as_slice(), [Effect::StopCapture { .. }]));

    let effects = decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStopped {
            binding: BindingKind::ProcessLoopback,
            operation_id: op0,
            epoch: epoch0,
        }),
    );
    let (op1, epoch1) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            target,
            ..
        }] => {
            assert_eq!(*target, ResolvedTarget::Process { pid: 2222 });
            (*operation_id, *proposed_epoch)
        }
        other => panic!("unexpected effects: {other:?}"),
    };
    assert_ne!(op0, op1);
    assert_ne!(epoch0, epoch1, "stream_epoch must update on PID re-resolution");

    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::ProcessLoopback,
            operation_id: op1,
            epoch: epoch1,
            target: ResolvedTarget::Process { pid: 2222 },
        }),
    );
    assert_eq!(
        state.bindings[&BindingKind::ProcessLoopback].lifecycle,
        CaptureBindingState::Running {
            operation_id: op1,
            epoch: epoch1,
            target: ResolvedTarget::Process { pid: 2222 },
        }
    );
}

// ---------------------------------------------------------------------------
// Invariants: stale events are rejected, no double-stop, no infinite retry.
// ---------------------------------------------------------------------------

#[test]
fn stale_worker_started_is_rejected() {
    let mut state = DecisionState::new().with_binding(CaptureBinding::new(
        BindingKind::Microphone,
        BindingSelection::Endpoint(EndpointSelection::Pinned {
            endpoint_id: endpoint("MicA"),
        }),
    ));
    let effects = decide(
        &mut state,
        DecisionInput::UserIntent(UserIntent::Start {
            binding: BindingKind::Microphone,
        }),
    );
    let (op0, epoch0) = match effects.as_slice() {
        [Effect::StartCapture {
            operation_id,
            proposed_epoch,
            ..
        }] => (*operation_id, *proposed_epoch),
        other => panic!("unexpected effects: {other:?}"),
    };
    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::Microphone,
            operation_id: op0,
            epoch: epoch0,
            target: ResolvedTarget::Endpoint(endpoint("MicA")),
        }),
    );

    // A duplicate arrival with a stale (nonexistent) operation_id/epoch must not
    // disturb the Running state.
    decide(
        &mut state,
        DecisionInput::Observation(Observation::WorkerStarted {
            binding: BindingKind::Microphone,
            operation_id: OperationId(op0.0 + 100),
            epoch: StreamEpoch(epoch0.0 + 100),
            target: ResolvedTarget::Endpoint(endpoint("MicA")),
        }),
    );
    assert_eq!(
        state.bindings[&BindingKind::Microphone].lifecycle,
        CaptureBindingState::Running {
            operation_id: op0,
            epoch: epoch0,
            target: ResolvedTarget::Endpoint(endpoint("MicA")),
        }
    );
}

#[test]
fn worker_failed_gives_up_after_max_retry_attempts() {
    let mut state = DecisionState::new().with_binding(CaptureBinding::new(
        BindingKind::Microphone,
        BindingSelection::Endpoint(EndpointSelection::Pinned {
            endpoint_id: endpoint("MicA"),
        }),
    ));
    let mut last_effects = decide(
        &mut state,
        DecisionInput::UserIntent(UserIntent::Start {
            binding: BindingKind::Microphone,
        }),
    );

    for _ in 0..(MAX_RETRY_ATTEMPTS + 1) {
        let operation_id = match last_effects.as_slice() {
            [Effect::StartCapture { operation_id, .. }] => *operation_id,
            other => panic!("expected StartCapture, got {other:?}"),
        };
        let effects = decide(
            &mut state,
            DecisionInput::Observation(Observation::WorkerFailed {
                binding: BindingKind::Microphone,
                operation_id,
                error: "AUDCLNT_E_DEVICE_INVALIDATED".to_string(),
            }),
        );
        match effects.as_slice() {
            [Effect::ScheduleRetry { retry_id, .. }] => {
                last_effects = decide(
                    &mut state,
                    DecisionInput::RetryTimerFired {
                        binding: BindingKind::Microphone,
                        retry_id: *retry_id,
                    },
                );
            }
            [] => {
                // Reached the cap and moved to Failed; no further effects.
                assert!(matches!(
                    state.bindings[&BindingKind::Microphone].lifecycle,
                    CaptureBindingState::Failed { .. }
                ));
                return;
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }
    panic!("did not reach Failed even after exceeding MAX_RETRY_ATTEMPTS");
}

#[test]
fn shutdown_prevents_new_start_capture() {
    let mut state = DecisionState::new().with_binding(CaptureBinding::new(
        BindingKind::Microphone,
        BindingSelection::Endpoint(EndpointSelection::Pinned {
            endpoint_id: endpoint("MicA"),
        }),
    ));
    decide(&mut state, DecisionInput::ShutdownRequested);
    let effects = decide(
        &mut state,
        DecisionInput::UserIntent(UserIntent::Start {
            binding: BindingKind::Microphone,
        }),
    );
    assert_eq!(effects, vec![], "must not issue StartCapture after shutdown");
}
