//! spike-plan.md SPIKE-12: Capture Rebinding State Machine
//!
//! audio-device-state-architecture.md §6(Observation → Admission → Pure
//! Decision → Effect Execution)の型定義・reducerを出発点にした、実WASAPIを
//! 一切呼ばないFake Worker向けの実装。ここに置くのは:
//!
//!   - Observation: OSやWorkerから届いた「生の事実」(このクレート内では
//!     [`Observation`] として表現。まだ確定した状態ではない)
//!   - Admission + Decision: [`decide`] が唯一の状態書き換え箇所。
//!     現在時刻取得・乱数・I/O・スレッド生成を一切行わない純粋関数
//!     ([`DecisionState`] と [`DecisionInput`] だけから [`Effect`] の列を返す)
//!   - Effect: 実行すべき命令(このクレートでは実行しない。呼び出し側が
//!     Fake WorkerまたはSPIKE-01/02の実WASAPIワーカーへ渡す)
//!
//! マイク(`Microphone`)・Endpoint Loopback(`EndpointLoopback`)・
//! Process Loopback(`ProcessLoopback`)の3種類のBindingを同じ`decide`関数で
//! 扱うが、Process Loopbackはendpoint側のObservation(`EndpointRemoved`/
//! `DefaultEndpointChanged`)を無視し、`ProcessExited`/`ProcessRestarted`
//! だけに反応する(spike-plan.md SPIKE-12の仮説4点目)。

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// 識別子・選択ポリシー(audio-device-state-architecture.md §2.2, §6.4)
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

/// マイク/Endpoint Loopbackの選択ポリシー。design.md §16.5のbinding modeと対応する
/// (`Pinned` = Fixed selected device、`FollowDefault` = Follow system default)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointSelection {
    FollowDefault { flow: DataFlow, role: DeviceRole },
    Pinned { endpoint_id: EndpointId },
}

/// このBindingが管理する対象の種別。`decide`のObservationルーティング
/// (endpoint系イベントをProcessLoopbackへ回さない等)に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingKind {
    Microphone,
    EndpointLoopback,
    ProcessLoopback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingSelection {
    Endpoint(EndpointSelection),
    /// Process Loopbackはendpoint selectionを持たない。`pid`はプロセス探索
    /// (SPIKE-02の`process_finder`相当)の結果を呼び出し側が随時更新する。
    Process { exe_name: String, pid: Option<u32> },
}

/// 実際に接続を試みる対象。`decide`が`resolve_target`で導出し、
/// `Effect::StartCapture`へ渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTarget {
    Endpoint(EndpointId),
    Process { pid: u32 },
}

// ---------------------------------------------------------------------------
// FSM本体(audio-device-state-architecture.md §6.4 CaptureBindingStateを
// そのまま採用。命名はspike-plan.mdの仮説(Idle/Resolving/Activating/
// Capturing/Interrupted/Rebinding/WaitingForDevice)と対応するが、
// 「直交する概念を混ぜない」という同章の方針に沿って以下のように対応させる:
//   Idle          -> Stopped
//   Resolving     -> Resolving
//   Activating    -> Starting
//   Capturing     -> Running
//   Interrupted   -> Stopping(nextに理由を保持)
//   Rebinding     -> Stopping{next: ResolveAndStart} からの再Starting
//   WaitingForDevice -> Waiting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamEpoch(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectId(pub u64);

/// Stop完了後に何をすべきか。「デバイス変更で再解決すべき」と「デバイスが
/// 消えたので待機すべき」を区別する(前者は即座にStarting、後者はいきなり
/// 再試行せずWaitingへ。Pinnedデバイス切断時に「別デバイスへ勝手に
/// フォールバックしない」という合否基準はこの区別で担保する)。
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
    /// このepochのFrameを受理してよいか。「旧epochのフレームが新epoch開始後に
    /// 出力されない」という合否基準に対応するヘルパー(データ面はこのクレート
    /// では扱わないが、判定ロジック自体は同じ形になるためここへ用意する)。
    pub fn accepts_epoch(&self, epoch: StreamEpoch) -> bool {
        matches!(self, CaptureBindingState::Running { epoch: e, .. } if *e == epoch)
    }
}

#[derive(Debug, Clone)]
pub struct CaptureBinding {
    pub kind: BindingKind,
    pub selection: BindingSelection,
    pub lifecycle: CaptureBindingState,
    /// 直近の連続失敗回数。`lifecycle`(Waiting/Starting等)を跨いで生存させる
    /// 必要があるため、enumの中ではなくbinding自身に持たせる。Starting中に
    /// この値をlifecycleへコピーせず、CaptureBinding側だけで管理する理由は、
    /// 「Starting -> WorkerFailed -> Waiting -> retry -> Starting」を繰り返す
    /// 過程でWaitingバリアントが消える(Startingへ上書きされる)ため、
    /// enum内に置くと前回の試行回数を失ってしまうため。
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

/// 無限リトライを避けるための上限(spike-plan.md SPIKE-09/12「復旧不能時に
/// 無限リトライしない」)。上限に達すると`Failed`へ遷移し、`decide`は
/// それ以上Effectを出さない。
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

/// OSまたはWorkerから届いた生の事実。`decide`はこれを鵜呑みにせず、
/// 現在のlifecycle/operation_id/epochと突き合わせてから状態を変える
/// (Admission相当の鮮度・世代チェックは`decide`内のmatchガードで行う)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// endpoint(物理デバイス)が消失した。Microphone/EndpointLoopbackのみ
    /// 反応する(ProcessLoopbackは無視、仮説4点目)。
    EndpointRemoved { endpoint_id: EndpointId },
    /// endpointが(再)出現した。WaitingForDevice相当の状態からの復帰契機。
    EndpointAdded { endpoint_id: EndpointId },
    /// 既定デバイスが変更された(既定デバイスなしはNone)。
    DefaultEndpointChanged {
        flow: DataFlow,
        role: DeviceRole,
        endpoint_id: Option<EndpointId>,
    },
    /// Effect::StartCaptureの実行がWorker側で完了した。
    WorkerStarted {
        binding: BindingKind,
        operation_id: OperationId,
        epoch: StreamEpoch,
        target: ResolvedTarget,
    },
    /// Effect::StopCaptureの実行がWorker側で完了した。
    WorkerStopped {
        binding: BindingKind,
        operation_id: OperationId,
        epoch: StreamEpoch,
    },
    /// Worker側で回復不能なエラーが起きた(AUDCLNT_E_DEVICE_INVALIDATED、
    /// IAudioSessionEvents::OnSessionDisconnected等、SPIKE-09で観測する
    /// 事象はすべてこれとして届く想定)。
    WorkerFailed {
        binding: BindingKind,
        operation_id: OperationId,
        error: String,
    },
    /// ProcessLoopback専用。対象プロセスが終了した(再起動ではない)。
    ProcessExited { pid: u32 },
    /// ProcessLoopback専用。対象プロセスが再起動しPIDが変わった。
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
    /// `Effect::ScheduleRetry`に対応するタイマー発火。`retry_id`が現在の
    /// 状態と一致しない場合は無視する(古いタイマーの遅延発火対策)。
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
        // Pinnedは「このIDを使いたい」という意図そのものであり、実際に
        // 存在するかどうかはEffect実行側(Fake WorkerまたはWASAPI)の
        // WorkerStarted/WorkerFailedで判明する。resolverが別endpointへ
        // 勝手に置き換えないことが合否基準であり、ここでは常に指定IDを返す。
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

/// 与えられたselectionが今のtargetを指しているか(既定変更が実質ノーオペか
/// どうかの判定に使う)。
fn selection_targets(selection: &BindingSelection, target: &ResolvedTarget) -> bool {
    match (selection, target) {
        (BindingSelection::Endpoint(_), ResolvedTarget::Endpoint(id)) => {
            resolve_target_id(selection).as_ref() == Some(id)
        }
        (BindingSelection::Process { pid: Some(p), .. }, ResolvedTarget::Process { pid }) => {
            p == pid
        }
        _ => false,
    }
}

fn resolve_target_id(selection: &BindingSelection) -> Option<EndpointId> {
    match selection {
        BindingSelection::Endpoint(EndpointSelection::Pinned { endpoint_id }) => {
            Some(endpoint_id.clone())
        }
        _ => None,
    }
}

/// 唯一の状態書き換え箇所。現在時刻取得・乱数生成・COM/WASAPI呼び出し・
/// ファイルI/O・スレッド生成のいずれも行わない(audio-device-state-architecture.md
/// §6.3の禁止事項どおり)。IDはすべて`state`内のカウンタから決定的に払い出す。
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
    // Stoppedからの新規開始と、Waiting(デバイス復帰/リトライタイマー)からの
    // 再試行の両方を許可する。それ以外(Resolving/Starting/Running/Stopping)
    // からは開始しない(Stopping中に新しいworkerを開始しない、という
    // 合否基準はこのガードで担保する)。
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

/// Runningなbindingを`next`という事後計画付きで停止する。Stopping中は
/// 新しいworkerを開始しない(このヘルパーがStopping以外の状態からしか
/// 呼ばれないため自然に守られる)。
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
                    if selection_targets(&binding.selection, target) {
                        continue; // 既に新しい既定デバイスへ接続済み(ノーオペ)
                    }
                    if let Some(effect) = stop_binding(state, kind, AfterStop::ResolveAndStart) {
                        effects.push(effect);
                    }
                }
                // Stopping/Starting/Waiting中は既定変更を記録するだけに留め、
                // Stop完了(WorkerStopped)またはWaitingからの再解決を待つ。
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
                    b.retry_attempt = 0; // 成功したので連続失敗回数をリセットする
                    Vec::new()
                }
                // Stopping/Running(別operation)からの遅延到着はstaleとして棄却。
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
                return Vec::new(); // stale(既にRunning等へ進んでいる)
            };
            if *expected_op != operation_id || *expected_epoch != epoch {
                return Vec::new(); // stale operation/epoch
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
                return Vec::new(); // stale
            }
            // 連続失敗回数はCaptureBinding::retry_attemptに持たせる(lifecycle
            // 内に置くとStarting遷移のたびに失われるため。§CaptureBindingの注記参照)。
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
        return Vec::new(); // 古いタイマー、または既に別要因でWaitingを離れた
    }
    if binding.retry_attempt as u64 != retry_id {
        return Vec::new(); // stale retry_id(その後さらに失敗/成功して進んでいる)
    }
    start_binding(state, kind).into_iter().collect()
}
