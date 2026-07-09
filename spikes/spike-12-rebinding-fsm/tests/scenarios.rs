//! spike-plan.md SPIKE-12 検証手順2: 4つのイベント列シナリオ + 検証手順3の
//! 不変条件(property test相当)。実WASAPIを一切呼ばず、`decide`だけを
//! 呼び出すFake Worker形式のテスト。

use spike_12_rebinding_fsm::*;

fn endpoint(id: &str) -> EndpointId {
    EndpointId(id.to_string())
}

// ---------------------------------------------------------------------------
// (a) FollowDefaultマイクでの既定変更(A→B)で、A停止完了後にBへ接続すること
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

    // 既定デバイスがA -> Bへ変更される。
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
    // Stop完了より前にBへ接続してはいけない。
    assert!(matches!(
        state.bindings[&BindingKind::Microphone].lifecycle,
        CaptureBindingState::Stopping { .. }
    ));

    // Aの停止が完了して初めてBへのStartCaptureが出る。
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
    assert_ne!(op_a, op_b, "新しいoperation_idが払い出されること");
    assert_ne!(epoch_a, epoch_b, "新しいstream_epochが払い出されること");

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
// (b) Pinnedマイクの切断でWaitingForDeviceへ遷移し、既定デバイスへ自動
//     フォールバックしないこと。再接続で復帰すること。
// ---------------------------------------------------------------------------
#[test]
fn pinned_mic_waits_for_recovery_without_falling_back_to_default() {
    let mut state = DecisionState::new().with_binding(CaptureBinding::new(
        BindingKind::Microphone,
        BindingSelection::Endpoint(EndpointSelection::Pinned {
            endpoint_id: endpoint("MicA"),
        }),
    ));
    // 既定デバイスはB(Aとは別)。Aが消えてもBへは絶対にフォールバックしない
    // ことを確認するための設定。
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

    // Aが物理的に抜かれる。
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
    // WaitingForDeviceへ遷移し、Bへの接続を試みるEffectは一切出ない。
    assert_eq!(effects, vec![]);
    assert_eq!(
        state.bindings[&BindingKind::Microphone].lifecycle,
        CaptureBindingState::Waiting {
            reason: WaitReason::DeviceUnavailable,
        }
    );

    // Aが再接続される。
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
        "MicAへ復帰すること(MicBへフォールバックしていないこと)"
    );
}

// ---------------------------------------------------------------------------
// (c) FollowDefaultスピーカーLoopbackでの既定変更で、旧stream停止 -> 新
//     endpointで再開すること
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
// (d) Process Loopbackで既定スピーカー変更/対象アプリの出力先変更では継続し、
//     対象アプリ再起動時のみPID再探索・epoch更新が起きること
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

    // 既定スピーカー変更・endpoint消失: Process Loopbackは無反応であること。
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
        "endpoint系イベントだけでは継続すること"
    );

    // 対象アプリが再起動しPIDが変わる。
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
    assert_ne!(epoch0, epoch1, "PID再探索でstream_epochが更新されること");

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
// 検証手順3: stale event拒否・二重停止しない・無限リトライしないの不変条件
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

    // 古い(存在しない)operation_id/epochでの二重到着はRunning状態を壊さない。
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
                // 上限に達してFailedへ遷移し、これ以上Effectを出さない。
                assert!(matches!(
                    state.bindings[&BindingKind::Microphone].lifecycle,
                    CaptureBindingState::Failed { .. }
                ));
                return;
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }
    panic!("MAX_RETRY_ATTEMPTSを超えてもFailedへ到達しなかった");
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
    assert_eq!(effects, vec![], "shutdown後はStartCaptureを出さない");
}
