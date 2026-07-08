# 音声デバイス状態管理アーキテクチャ設計書

* **文書ステータス**: Draft v0.1
* **作成日**: 2026-07-07
* **関連文書**: [design.md](design.md) §12(永続化設計)・§16(デバイス変更と障害処理)、[spike-plan.md](spike-plan.md) SPIKE-04(セグメント確定)/ SPIKE-11(Audio Endpoint Registry)/ SPIKE-12(Capture Rebinding State Machine)
* **位置づけ**: 本書は実装方式そのものであり、Spike として検証してから実装するか、アプリ本体実装時に直接組み込むかは別途判断する。判断とは独立に、方式そのものをここに設計書として残す。

---

## 1. 目的とスコープ

design.md §16.5 は「録音中のデバイス選択は既定では固定し、`Follow system default` は明示設定時のみ追随する」という運用方針を定めている。しかし、**録音中に各デバイス(マイク・スピーカー)の状態が変わった場合の管理モデル**は詳細化されていない。現状の設計は起動時に一度デバイスを解決し、その情報を `summary.json` へ残すところまでに留まる。

一方で、**列挙されたマイク・スピーカーすべてに個別の巨大な状態機械を持つ必要はない**。本書は、

* 何を三層に分けて管理すべきか(§2〜4)
* それをどのアーキテクチャ(Enum / FSM / Behavior Tree / コルーチン)で実装すべきか(§4〜5)
* OS通知やAPI結果を「現在の真実」として即座に信用しない、再生可能なイベント駆動 Decision Engine としてどう発展させるべきか(§6)
* Effect(副作用)が「命令したこと」と「実際に完了したこと」を混同しないために、完了保証・冪等性・照合をどう設計するか(§7)

を一貫した設計としてまとめる。

---

## 2. 三層モデル(全体像)

デバイス状態は次の3層に分けて管理する。1層にまとめると「デバイスがどう見えているか」「どのデバイスを使いたいか」「実際の録音streamがどうなっているか」が混線し、フラグの組み合わせ爆発を起こす。

### 2.1 デバイス一覧の観測状態

すべてのマイク・スピーカーを、endpoint IDをキーにしたレジストリで保持する。

```rust
pub struct AudioEndpointSnapshot {
    pub id: EndpointId,
    pub flow: DataFlow, // Capture | Render

    pub device_state: DeviceState,
    pub friendly_name: String,

    pub default_roles: BTreeSet<DeviceRole>,
    pub volume_scalar: Option<f32>,
    pub muted: Option<bool>,

    pub format_fingerprint: Option<AudioFormatFingerprint>,

    pub revision: u64,
    pub last_observed_at_100ns: u64,
}
```

Windowsは `IMMNotificationClient` により、以下をendpoint ID単位で通知できる。

* 追加
* 削除
* 状態変更
* プロパティ変更
* 既定デバイス変更

endpoint IDはシステム内でデバイスを識別する不透明な文字列として扱うべきである。また、通知コールバックはブロックせず、重い処理やCOMオブジェクトの解放を直接行わないことが要求されている([Microsoft Learn][1])。

デバイス状態は最低でも次を保持する。

```rust
pub enum DeviceState {
    Active,
    Disabled,
    NotPresent,
    Unplugged,
}
```

Windowsのendpoint状態もこの4種類であり、新しいストリームを開けるのは基本的に `ACTIVE` だけである([Microsoft Learn][2])。

### 2.2 ユーザーが何を使いたいかという選択状態

デバイス自身の状態とは別に、選択ポリシーを持つ(design.md §16.5 の binding mode と対応する)。

```rust
pub enum EndpointSelection {
    FollowDefault {
        flow: DataFlow,
        role: DeviceRole,
    },
    Pinned {
        endpoint_id: EndpointId,
    },
}
```

「既定の通話用マイクを追従する」と「特定のUSBマイクを使い続ける」は意味が異なる。`OnDefaultDeviceChanged` は capture/renderとConsole/Multimedia/Communicationsの組合せごとに通知され、利用可能な既定デバイスがない場合はIDがNULLになるため、既定ルートは `Option<EndpointId>` で持つ必要がある([Microsoft Learn][3])。

```rust
pub struct DefaultRouteMap {
    pub routes: HashMap<(DataFlow, DeviceRole), Option<EndpointId>>,
}
```

### 2.3 実際に動いている録音ストリームの状態

デバイス状態と録音状態は同じではない。

```rust
pub enum CaptureBindingState {
    Idle,

    Resolving {
        selection: EndpointSelection,
    },

    Activating {
        endpoint_id: EndpointId,
        stream_epoch: u64,
    },

    Capturing {
        endpoint_id: EndpointId,
        stream_epoch: u64,
        started_at_100ns: u64,
    },

    Interrupted {
        previous_endpoint_id: EndpointId,
        previous_epoch: u64,
        reason: CaptureInterruptReason,
    },

    Rebinding {
        new_endpoint_id: EndpointId,
        new_epoch: u64,
    },

    WaitingForDevice {
        selection: EndpointSelection,
    },

    Failed {
        error: String,
        retryable: bool,
    },
}
```

ここが重要である。Windows上のendpoint状態が `Disabled` に変わったからといって、すでに開いているストリームが必ず即座に無効化されるわけではない。コントロールパネルでデバイスを無効化しても、既存ストリームがそのまま動き続ける場合がある([Microsoft Learn][4])。したがって、**endpoint observed state と capture stream state を分けなければならない**。

録音ストリームの実際の切断は、次の観測から判断する。

* `IAudioSessionEvents::OnSessionDisconnected`
* WASAPI呼び出しの `AUDCLNT_E_DEVICE_INVALIDATED`
* `AUDCLNT_E_SERVICE_NOT_RUNNING`
* callback timeout
* `GetBuffer` 等の失敗

session切断では、デバイス取り外し、Audio Service停止、フォーマット変更、排他モードによる切断などの理由を受け取れる。切断後は関連する `IAudioClient` 等を解放して再生成する必要がある([Microsoft Learn][5])。

---

## 3. デバイス種別ごとの管理対象の違い

マイクとスピーカーで管理対象は少し異なる。

### 3.1 マイク

マイクでは次を管理する。

```text
endpoint存在状態
既定ロール
選択ポリシー
mute / volume
フォーマット
capture stream状態
実データの有無
無音継続時間
discontinuity
timestamp error
stream epoch
```

特に、「OS上はmuteではないが、PCMはずっと無音」という状態は普通にあり得るため、次を分離する。

```rust
control_plane_muted: bool
signal_activity: AudioActivity
```

```rust
pub enum AudioActivity {
    Unknown,
    Silent,
    Active,
}
```

endpointのmute状態は `IAudioEndpointVolume::GetMute` で取得でき、volume/mute変更はcallbackで監視できる([Microsoft Learn][6])。

### 3.2 Endpoint Loopback用スピーカー

Endpoint Loopbackは、**特定のrender endpointに結び付いている**。管理対象は次である。

```text
対象render endpoint
既定スピーカー変更
スピーカーの抜き差し
Bluetooth endpointの切替
endpoint mute / volume
フォーマット変更
loopback streamの生存状態
```

特に `FollowDefault` モードでは、既定スピーカー変更通知を受けても、既存ストリームは旧スピーカーを録音し続ける可能性がある。そのためアプリ側で明示的に、旧stream停止 → 旧epoch終了 → 新endpoint解決 → 新stream開始 → 新epoch開始、を行う必要がある。

### 3.3 Process Loopback

Process Loopbackは事情が異なる。Microsoftの公式サンプルでは、Process Loopbackは**特定の物理audio endpointに結び付かない**と説明されている。対象プロセスがどのスピーカーへ出力していても、対象プロセスツリーの音声を取得するための仕組みである([GitHub][7])。したがって、Process Loopbackについては**個別スピーカーの状態によってProcess Loopbackを再起動する設計にはしない**。

キャプチャ源を明確に分ける。

```rust
pub enum CaptureSource {
    Microphone {
        selection: EndpointSelection,
    },

    EndpointLoopback {
        selection: EndpointSelection,
    },

    ProcessLoopback {
        target: ProcessTarget,
    },
}
```

Process Loopbackでもスピーカー一覧はUI・診断用途には必要だが、録音bindingの主キーではない。

---

## 4. 状態管理方式の評価

このケースには次の組み合わせが最も適している。

> **不変スナップショット + 純粋Reducer + 型付き階層FSM + Actor/専用スレッドによるEffect実行**

| 対象                   | 採用方式                                    |
| --------------------- | ---------------------------------------- |
| 全マイク・全スピーカーの現在値      | `HashMap<EndpointId, EndpointSnapshot>`  |
| ユーザーの選択方針            | 単純な `enum`                              |
| 各録音ソースの開始・停止・復旧      | 階層FSM                                    |
| Windows通知の受信         | callback → event queue                   |
| リトライ・タイマー・オーケストレーション | コルーチン/Actor                             |
| WASAPI・COMキャプチャ      | 専用MTAスレッド                               |
| Behavior Tree        | 原則不採用                                    |

Enum、FSM、コルーチンは競合する選択肢ではない。Enumは状態を表現し、FSMは状態遷移を決め、コルーチンは時間のかかる処理を実行し、Behavior Treeは目標選択や戦略切替を表現する、という別々の役割である。

### 4.1 単なるEnum状態保持では不足する

```rust
enum CaptureState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}
```

これは必要だが、これだけでは不足する。単なるEnumだけで実装すると次のような処理が各所で増える。

```rust
if matches!(state, CaptureState::Running) {
    state = CaptureState::Stopping;
    stop();
}
```

問題は状態変更と副作用が密結合することである。

* どこからでも状態を書き換えられる
* `stop()` が完了する前に `Starting` へ進める
* 古いスレッドから遅れて届いたイベントを受理してしまう
* リトライ中に新しいデバイス変更が起きた場合の扱いが曖昧
* `Starting` 中の対象endpointが状態から分からない

したがってEnumは、**状態に必要な文脈を内包する型**として使う。

```rust
pub enum BindingState {
    Stopped,

    Resolving {
        request_id: u64,
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
        started_at: HostTime,
    },

    Stopping {
        operation_id: OperationId,
        epoch: StreamEpoch,
        target: ResolvedTarget,
        next: AfterStop,
    },

    Waiting {
        reason: WaitReason,
        retry: RetryState,
    },

    Failed {
        cause: CaptureFailure,
    },
}
```

この意味では、推奨するFSMも最終的な表現はRustのEnumである。

### 4.2 FSMが必要な範囲

録音BindingのライフサイクルにはFSMが最適である。対象となるのは、物理デバイスそのものではなく、「この録音スロットが、現在どの入力源へどう接続されているか」である。たとえば次の3つが独立したFSMになる。

```text
MicrophoneBinding
EndpointLoopbackBinding
ProcessLoopbackBinding
```

重要なのは、**PC上に存在する全マイク・全スピーカーそれぞれへFSMを作らない**ことである。USBマイクが10個列挙されていても、多くの場合は単なる観測対象であり、各endpointにはSnapshotだけを保持する(§2.1)。

```rust
pub struct EndpointRegistry {
    pub endpoints: HashMap<EndpointId, EndpointSnapshot>,
    pub defaults: DefaultRouteMap,
    pub revision: u64,
}
```

### 4.3 階層FSM

フラットなFSMだけだと状態数が増える。たとえば `RunningHealthy` / `RunningSilent` / `RunningTimestampError` / `RunningDeviceUnplugged` / `RunningDefaultChanged` / `RunningStopping` と増やすのはよくない。そこで、状態を直交する概念に分ける。

```rust
pub struct CaptureBinding {
    pub desired: DesiredBinding,
    pub lifecycle: BindingLifecycle,
    pub health: BindingHealth,
    pub revision: u64,
}
```

Lifecycle:

```rust
pub enum BindingLifecycle {
    Stopped,

    Resolving {
        cause: ResolveCause,
    },

    Starting {
        op: PendingStart,
    },

    Active {
        session: ActiveSession,
    },

    Stopping {
        session: ActiveSessionRef,
        next: AfterStop,
    },

    Waiting {
        reason: WaitReason,
        retry: RetryState,
    },

    TerminalFailure {
        cause: CaptureFailure,
    },
}
```

Health:

```rust
pub struct BindingHealth {
    pub signal: SignalHealth,
    pub timing: TimingHealth,
    pub pipeline: PipelineHealth,
}

pub enum SignalHealth {
    Unknown,
    Silent {
        since: HostTime,
    },
    Active {
        last_activity_at: HostTime,
    },
}

pub enum TimingHealth {
    Healthy,
    TimestampErrors {
        count: u64,
    },
    Discontinuities {
        count: u64,
    },
}

pub enum PipelineHealth {
    Healthy,
    Backpressured {
        queue_depth: usize,
    },
    Dropping {
        dropped_frames: u64,
    },
}
```

こうすると、`Silent` を録音ライフサイクルの失敗状態にする必要がない。「録音streamはRunning・PCMはSilent・endpointはActive・OS muteはfalse」という現実にあり得る組み合わせを自然に表せる。

### 4.4 Behavior Treeは不採用

現段階では不採用が妥当である。Behavior Treeが得意なのは、次のような目標指向の判断である。

```text
Bluetoothヘッドセットを試す
  失敗したらUSBマイク
    失敗したら既定通話用マイク
      失敗したら内蔵マイク
```

今回のデバイス選択は、現状では純粋関数で十分である。

```rust
pub fn resolve_endpoint(
    policy: &EndpointSelection,
    registry: &EndpointRegistry,
) -> ResolutionResult
```

Behavior Treeは将来、複数録音方式から動的選択する・Remote音声取得にProcess Loopback / Endpoint Loopback / 会議SDKを順番に試す・権限やアプリ状態やデバイス状態や品質を総合して継続的に戦略を切り替える、ところまで発展した場合に再検討すれば十分である。

### 4.5 コルーチンは正本にしない

次のような書き方は一見読みやすい。

```rust
async fn capture_lifecycle() {
    loop {
        let endpoint = wait_until_available().await;
        let stream = start_capture(endpoint).await?;

        tokio::select! {
            _ = wait_device_lost() => {},
            _ = wait_default_changed() => {},
            _ = wait_shutdown() => return,
        }

        stop_capture(stream).await;
    }
}
```

しかし状態がコルーチンのスタックに隠れる。

* 現在どの状態か外部から検査しにくい
* 永続化しにくい
* イベント順序テストが難しい
* 古いfutureから返った結果を受理しやすい
* 別のコマンドが入ったときの中断位置が曖昧
* restart時に途中状態を再構築しにくい

さらにTokioの `select!` は、同一task上で各branchを実行するため、branch内でブロッキング処理をすると他のbranchも進まなくなる。キャンセルされたfutureが安全かどうかも個別に検討が必要である([Docs.rs][8])。

したがってコルーチンは、イベント受信・リトライタイマー・shutdown待機・workerの完了待機・UIへの状態配信・Effectの配送、に限定する。

---

## 5. 推奨アーキテクチャ(イベント駆動 Reducer)

```text
Windows callbacks
  IMMNotificationClient
  IAudioSessionEvents
  Process watcher
  Capture worker events
          │
          ▼
  ObservationEvent Queue
          │
          ▼
┌────────────────────────────┐
│ AudioCoordinator Actor     │
│                            │
│ state: AudioRuntimeState   │
│                            │
│ event                      │
│   ↓                        │
│ pure reduce(state, event)  │
│   ↓                        │
│ new state + effects        │
└─────────────┬──────────────┘
              │ Effects
              ▼
┌────────────────────────────┐
│ Effect Executor            │
│                            │
│ StartCapture               │
│ StopCapture                │
│ EnumerateEndpoints         │
│ ScheduleRetry               │
│ StartProcessWatch          │
└─────────────┬──────────────┘
              │
              ▼
       Dedicated MTA workers
```

Windowsのendpoint通知callbackは、ブロックせず、callback内で登録解除や最終COM参照の解放をしないことが要求されている。そのため、callbackはイベントをqueueへ積むだけにする構造が適切である([Microsoft Learn][1])。

> **本節の `reduce()` は §6 でさらに発展させる。** §6 では Observation と確定した Belief を分離し、Admission Gate を明示的な層として挿入する。ただし本節の Worker/FSM境界・アンチパターン・テストシナリオはそのまま有効である。

### 5.1 唯一の状態正本

```rust
pub struct AudioRuntimeState {
    pub registry: EndpointRegistry,

    pub microphone: CaptureBinding,
    pub endpoint_loopback: CaptureBinding,
    pub process_loopback: CaptureBinding,

    pub shutdown: ShutdownState,

    pub next_operation_id: u64,
    pub next_stream_epoch: u64,
}
```

ここには原則として以下を入れない。

* `IAudioClient`
* `IAudioCaptureClient`
* COM interface
* `HANDLE`
* `JoinHandle`
* `Sender`
* `Receiver`

これらはEffect ExecutorまたはWorkerのruntime resourceである。FSMの状態には、OSリソースそのものではなく識別子だけを置く。

```rust
pub struct ActiveSession {
    pub operation_id: OperationId,
    pub worker_id: WorkerId,
    pub stream_epoch: StreamEpoch,
    pub source: ResolvedTarget,
    pub format: AudioFormatSnapshot,
}
```

### 5.2 Event

すべての外部変化をEventへ正規化する。

```rust
pub enum AudioEvent {
    EndpointObserved {
        snapshot: EndpointSnapshot,
    },

    EndpointRemoved {
        endpoint_id: EndpointId,
    },

    DefaultEndpointChanged {
        flow: DataFlow,
        role: DeviceRole,
        endpoint_id: Option<EndpointId>,
    },

    StartRequested {
        binding: BindingId,
    },

    StopRequested {
        binding: BindingId,
    },

    WorkerStarted {
        binding: BindingId,
        operation_id: OperationId,
        epoch: StreamEpoch,
        worker_id: WorkerId,
        target: ResolvedTarget,
        format: AudioFormatSnapshot,
    },

    WorkerStopped {
        binding: BindingId,
        operation_id: OperationId,
        epoch: StreamEpoch,
        reason: StopReason,
    },

    WorkerFailed {
        binding: BindingId,
        operation_id: OperationId,
        epoch: Option<StreamEpoch>,
        error: CaptureFailure,
    },

    SessionDisconnected {
        binding: BindingId,
        operation_id: OperationId,
        epoch: StreamEpoch,
        reason: SessionDisconnectReason,
    },

    SignalActivityChanged {
        binding: BindingId,
        epoch: StreamEpoch,
        activity: SignalActivity,
    },

    RetryTimerFired {
        binding: BindingId,
        retry_id: RetryId,
    },

    ShutdownRequested,
}
```

Windowsのaudio session切断では、デバイス取り外し、Audio Service停止、フォーマット変更、排他モードによる切断などの理由が通知される。切断後は、閉じたstreamに関連する `IAudioClient` やservice interfaceを解放する必要がある([Microsoft Learn][5])。

### 5.3 Effect

ReducerはOS APIを呼ばず、Effectだけ返す。

```rust
pub enum AudioEffect {
    EnumerateEndpoints,

    StartCapture {
        binding: BindingId,
        operation_id: OperationId,
        proposed_epoch: StreamEpoch,
        target: ResolvedTarget,
    },

    StopCapture {
        binding: BindingId,
        worker_id: WorkerId,
        operation_id: OperationId,
        epoch: StreamEpoch,
    },

    ScheduleRetry {
        binding: BindingId,
        retry_id: RetryId,
        delay: Duration,
    },

    CancelRetry {
        retry_id: RetryId,
    },

    PublishSnapshot,

    EmitDiagnostic {
        diagnostic: AudioDiagnostic,
    },
}
```

関数形は次である。

```rust
pub fn reduce(
    state: AudioRuntimeState,
    event: AudioEvent,
) -> (AudioRuntimeState, Vec<AudioEffect>)
```

あるいはコピーを避けるなら、

```rust
pub fn reduce(
    state: &mut AudioRuntimeState,
    event: AudioEvent,
) -> Vec<AudioEffect>
```

でも構わない。重要なのは、**状態を書き換える場所をReducerだけに限定すること**である。

### 5.4 遷移例

**FollowDefaultマイクの既定デバイス変更**

現在: `Running, endpoint = Mic-A, epoch = 10, selection = FollowDefault(Communications)`

イベント:

```rust
AudioEvent::DefaultEndpointChanged {
    flow: DataFlow::Capture,
    role: DeviceRole::Communications,
    endpoint_id: Some(MicB),
}
```

Reducerは次を行う。

```text
1. DefaultRouteMapを更新
2. bindingがFollowDefaultか確認
3. 現在のMic-Aと新しいMic-Bを比較
4. 同じなら何もしない
5. 異なるならStoppingへ
6. StopCapture effectを返す
```

状態:

```rust
BindingLifecycle::Stopping {
    session: current_session,
    next: AfterStop::ResolveAndStart,
}
```

ここで即座にMic-Bを開始してはいけない。`WorkerStopped` を受けてから `Stopping → Resolving → Starting Mic-B → Running Mic-B` と進める。

**Pinnedマイクが抜かれた**

`Pinned(Mic-A)` / `Running(Mic-A)` に対し `EndpointRemoved { endpoint_id: MicA }` が来た場合、`Running → Stopping → Waiting(DeviceUnavailable)` へ進める。別の既定マイクへ自動切替してはいけない。Mic-Aが再登録されたら `Waiting → Resolving → Starting → Running` である。

**SessionDisconnected**

```rust
AudioEvent::SessionDisconnected {
    binding,
    operation_id,
    epoch,
    reason: SessionDisconnectReason::DeviceRemoval,
}
```

受信時は、まだstateが同じoperationとepochか確認する。

```rust
if active.operation_id != operation_id ||
   active.epoch != epoch
{
    return vec![]; // stale event
}
```

一致すれば `Active → Stopping または Waiting` へ遷移する。**OS callbackから直接再Activateしないことが重要である。**

### 5.5 `operation_id` と `stream_epoch` を分ける

両者は似ているが役割が違う。

* **operation_id**: 開始・停止など、非同期命令の対応付け。遅れて返った完了通知を無視するために使う。
* **stream_epoch**: PCMデータがどの録音stream世代に属するかを表す。CSV、WAV、タイムライン解析で使用する。

`spike-windows-01-02-detail-design.md` の `capture_epoch` は Process Loopback専用の再アタッチ世代識別だが、これを全Binding共通の `stream_epoch` へ一般化するのがよい。

### 5.6 コルーチン/Actorの実装方法

**AudioCoordinator** はasync taskで構わない。

```rust
pub async fn run_audio_coordinator(
    mut state: AudioRuntimeState,
    mut event_rx: mpsc::Receiver<AudioEvent>,
    effect_tx: mpsc::Sender<AudioEffect>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;

            _ = shutdown.cancelled() => {
                let effects =
                    reduce(&mut state, AudioEvent::ShutdownRequested);

                dispatch_effects(&effect_tx, effects).await;
                break;
            }

            event = event_rx.recv() => {
                let Some(event) = event else {
                    break;
                };

                let effects = reduce(&mut state, event);
                dispatch_effects(&effect_tx, effects).await;
            }
        }
    }
}
```

Tokioのchannel受信と `CancellationToken::cancelled()` はキャンセルセーフな待機として利用できる。`CancellationToken` は親子tokenも作れるが、cancelはあくまで停止要求であり、リソース解放完了の証明にはならないため、最終的にはworkerの停止通知またはjoinを待つ必要がある([Docs.rs][8])。

**CaptureWorker** はasync taskではなく、**長寿命の専用MTAスレッド**がよい。

```rust
pub enum WorkerCommand {
    Start {
        operation_id: OperationId,
        epoch: StreamEpoch,
        target: ResolvedTarget,
    },

    Stop {
        operation_id: OperationId,
        epoch: StreamEpoch,
    },

    Shutdown,
}
```

```rust
fn capture_worker_main(
    command_rx: Receiver<WorkerCommand>,
    event_tx: Sender<AudioEvent>,
) {
    let _com = ComApartment::new_mta().expect("COM init failed");

    let mut active: Option<WorkerSession> = None;

    loop {
        match command_rx.recv() {
            Ok(WorkerCommand::Start { .. }) => {
                // Resolve
                // Activate
                // Initialize
                // Capture loop
            }

            Ok(WorkerCommand::Stop { .. }) => {
                // SetEvent
                // Stop
                // Release COM resources
                // WorkerStopped
            }

            Ok(WorkerCommand::Shutdown) | Err(_) => {
                break;
            }
        }
    }
}
```

キャプチャ中にもcommandを受ける必要がある。既存設計にあるように、WASAPIのaudio-ready eventとstop eventを `WaitForMultipleObjects` で同時待機する構成を維持するのが適切であり、async化するためだけに `WaitForMultipleObjects` をTokioへ無理に統合する必要はない。

### 5.7 WorkerとFSMの境界

Workerに判断を持たせすぎないことが重要である。

**Workerが判断してよいこと**: `GetBuffer` を繰り返す、`ReleaseBuffer` する、stop eventで終了する、COM/WASAPIエラーを分類する、PCMを送る、実行結果をeventにする。

**Workerが判断してはいけないこと**: 既定デバイスへ切り替える、PinnedからDefaultへフォールバックする、リトライ回数を決める、Process Loopbackへ切り替える、ユーザー設定を変更する、epochの正当性を決める。これらはCoordinatorのFSMが担当する。

### 5.8 RegistryにもFSMは必要か

EndpointRegistry全体については、小さなFSMを置いてもよい。

```rust
pub enum RegistryLifecycle {
    Uninitialized,

    Enumerating {
        request_id: u64,
    },

    Watching,

    Resyncing {
        reason: ResyncReason,
    },

    Failed {
        retry: RetryState,
    },

    Stopped,
}
```

ただし、各endpointはFSMにせずSnapshotである(§2.1)。通知callbackが来たら、その内容だけを盲信せず、「callback受信 → event化 → 必要ならGetDevice/再列挙 → snapshot更新」とする。

### 5.9 リトライも状態として持つ

コルーチン内で `sleep(Duration::from_secs(5)).await; retry().await;` と書くだけでは、外から見ると「なぜ待っているか」「いつ再試行するか」が分からない。

```rust
pub struct RetryState {
    pub retry_id: RetryId,
    pub attempt: u32,
    pub next_at: HostTime,
    pub backoff: BackoffPolicy,
    pub last_error: CaptureFailure,
}
```

FSMへ明示的に保持する。

```rust
BindingLifecycle::Waiting {
    reason: WaitReason::RetryableFailure,
    retry: RetryState {
        retry_id,
        attempt: 3,
        next_at,
        backoff: BackoffPolicy::Exponential {
            initial_ms: 500,
            max_ms: 30_000,
        },
        last_error,
    },
}
```

タイマーtaskは時間になったら `AudioEvent::RetryTimerFired { binding, retry_id }` を返すだけである。古いタイマーなら `retry_id` 不一致で捨てられる。

### 5.10 推奨しない実装(アンチパターン)

* **`Arc<Mutex<CaptureState>>` を各所から更新**: ロックによってデータ競合は防げるが、正しい状態遷移は保証できない
* **callback内で状態遷移・再接続**: Windows callbackには非ブロッキング要件があるため不適切([Microsoft Learn][1])
* **coroutineのローカル変数を唯一の状態にする**: 外部から観測・テスト・復元できなくなる
* **全endpointごとにFSMを作る**: 列挙デバイス数に比例して無意味な状態機械が増える。個別endpointはSnapshot、利用中のBindingだけFSMが適切

### 5.11 スパイクで確認すべき実装

実際のWASAPI実装に入る前に、まずFake WorkerでFSMを完成させるべきである(spike-plan.md SPIKE-12 が対応)。

イベント列テストの例:

```rust
#[test]
fn follow_default_rebinds_after_old_worker_stops() {
    let mut state = running_on_mic_a_follow_default();

    let effects = reduce(
        &mut state,
        AudioEvent::DefaultEndpointChanged {
            flow: DataFlow::Capture,
            role: DeviceRole::Communications,
            endpoint_id: Some(MIC_B),
        },
    );

    assert!(matches!(
        state.microphone.lifecycle,
        BindingLifecycle::Stopping { .. }
    ));

    assert_eq!(effects, vec![
        AudioEffect::StopCapture { /* ... */ }
    ]);

    let effects = reduce(
        &mut state,
        AudioEvent::WorkerStopped {
            operation_id: OLD_OPERATION,
            epoch: OLD_EPOCH,
            reason: StopReason::Requested,
            /* ... */
        },
    );

    assert!(matches!(
        state.microphone.lifecycle,
        BindingLifecycle::Starting { .. }
    ));

    assert!(matches!(
        effects.as_slice(),
        [AudioEffect::StartCapture { .. }]
    ));
}
```

必須シナリオ:

| シナリオ                    | 確認点                |
| ----------------------- | ------------------ |
| Default A → B           | Stop完了前にBを開始しない    |
| Default A → B → C       | Bの開始完了をstaleとして捨てる |
| Pinned Aが消える            | Defaultへ勝手に切替しない   |
| Aが再出現                   | Waitingから再開する      |
| Start中にStopRequested    | Start完了後すぐ停止できる    |
| Stop中にStartRequested    | `next`へ意図を保持できる    |
| 古いepochからFrame到着        | 捨てられる              |
| 古いretry timer発火         | 捨てられる              |
| SessionDisconnected二重通知 | 二重停止しない            |
| callback順序が逆転           | 不正遷移しない            |
| Audio Service停止         | 全Bindingを一貫して復旧できる |
| Shutdown中にretry発火       | 新規Startしない         |

不変条件(property testに向いている):

```text
Runningなら必ずworker_idとepochがある
Stoppedならactive workerはない
同じBindingにActive workerは最大1つ
Frameを受理するepochは現在のRunning epochだけ
Stopping中は新しいworkerを開始しない
Pinned endpointをresolverが別endpointへ置換しない
Shutdown開始後はStartCapture effectを出さない
```

---

## 6. アーキテクチャの発展形: Observation → Admission → Decision → Effect Execution

§5 の `reduce(state, event) -> (state, effects)` は健全だが、まだ一段改善できる。今回の音声デバイス管理は、単なるFSMではなく、

> **Observation → Admission → Pure Decision → Effect Execution → Execution Observation**

という循環構造にするのがよい。重要なのは、**OS通知やAPI結果をそのまま「現在の真実」にしないこと**である。

```text
Windows / WASAPI / Process
        │
        ▼
Raw Observation
        │
        ▼
Pure Admission Gate
  鮮度・世代・出所・整合性を検査
        │
        ▼
Accepted Evidence
        │
        ▼
Pure Decision Engine
  Belief更新
  FSM遷移
  Effect生成
        │
        ▼
Effect Executor
  COM / WASAPI / Timer / Worker
        │
        ▼
Execution Observation
```

Decision Engineだけをドメイン状態の唯一のwriterにする。§5 の Worker/FSM境界(§5.7)・アンチパターン(§5.10)・テストシナリオ(§5.11)はそのまま本節にも適用される。

### 6.1 ObservationとBeliefを分ける

たとえばWindowsから「マイクAがRemovedになった」という通知が来ても、それは確定した現在状態ではない。考えられることがある。

* 通知後すぐ再接続された
* 通知順序が前後した
* 古いcallbackが遅れて届いた
* endpointはRemovedだが既存streamはまだ動作中
* デバイス一覧再取得結果と矛盾している

したがって、まずは観測として保存する。

```rust
pub struct ObservationEnvelope<T> {
    pub observation_id: ObservationId,
    pub source: ObservationSource,

    pub observed_at: HostTime,
    pub received_at: HostTime,

    pub source_seq: Option<u64>,
    pub operation_id: Option<OperationId>,
    pub stream_epoch: Option<StreamEpoch>,

    pub payload: T,
}
```

そしてDecisionが管理するのはObservationではなく、Observationから導出したBeliefである。

```rust
pub struct EndpointBelief {
    pub endpoint_id: EndpointId,
    pub availability: EndpointAvailability,
    pub last_evidence: EvidenceRef,
    pub revision: u64,
}
```

### 6.2 Confidenceだけでなく、信頼理由を型で持つ

すべてを `confidence: High/Medium/Low` だけで処理すると、後で曖昧になる。次の軸を明示した方がよい。

```rust
pub struct EvidenceQuality {
    pub authority: Authority,
    pub freshness: Freshness,
    pub correlation: Correlation,
    pub completeness: Completeness,
}
```

情報源の優先順位は明示できる。

```text
endpointの存在
  再列挙結果 > IMMNotificationClient単発通知

streamの生存
  workerの正常フレーム > endpoint状態通知

mute状態
  IAudioEndpointVolume::GetMute > PCMが無音

PCM activity
  実際のサンプル解析

既定デバイス
  GetDefaultAudioEndpoint再問い合わせ
    > OnDefaultDeviceChanged通知だけ
```

ただし、汎用的な点数計算にはしない方がよい。ルールを明示的に書く。

```rust
fn admit(
    state: &DecisionState,
    observation: RawObservation,
) -> AdmissionResult
```

```rust
pub enum AdmissionResult {
    Accepted(AcceptedEvidence),
    Rejected {
        reason: RejectionReason,
    },
}
```

拒否理由もログに残す。

```rust
pub enum RejectionReason {
    StaleOperation,
    StaleEpoch,
    OlderRevision,
    DuplicateObservation,
    SourceSequenceRollback,
    SubjectMismatch,
    ShutdownInProgress,
}
```

### 6.3 Decisionは完全にpureにする

中心となる関数は次である。

```rust
pub fn decide(
    state: &DecisionState,
    input: &DecisionInput,
) -> DecisionOutcome
```

```rust
pub struct DecisionOutcome {
    pub next_state: DecisionState,
    pub effects: Vec<Effect>,
    pub diagnostics: Vec<DecisionDiagnostic>,
}
```

Decision内で禁止するものは次である。

```text
現在時刻の取得
乱数生成
COM API呼び出し
ファイルI/O
スレッド生成
sleep
チャネル送信
グローバル変数参照
```

時刻が必要なら、入力イベントとして渡す。

```rust
pub enum DecisionInput {
    Observation(RawObservation),
    UserIntent(UserIntent),
    TimerFired(TimerEvent),
    ConfigurationChanged(ConfigurationSnapshot),
}
```

IDもDecision内の状態から決定的に払い出す。

```rust
pub struct DecisionState {
    pub next_effect_id: u64,
    pub next_operation_id: u64,
    pub next_stream_epoch: u64,
}
```

これにより同じ初期状態と同じ入力列なら、必ず同じ結果になる(§6.7 のReplayの前提)。

### 6.4 FSMはDecision内部の一部として使う

FSMは状態管理全体ではなく、Decisionが扱う投影の一つである。

```rust
pub struct DecisionState {
    pub endpoint_registry: EndpointRegistryBelief,

    pub microphone: CaptureBindingState,
    pub endpoint_loopback: CaptureBindingState,
    pub process_loopback: CaptureBindingState,

    pub pending_effects: BTreeMap<EffectId, PendingEffect>,
    pub shutdown: ShutdownState,
}
```

```rust
pub enum CaptureBindingState {
    Stopped,

    Resolving {
        requested_by: IntentId,
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
        retry: Option<RetryState>,
    },

    Failed {
        cause: CaptureFailure,
    },
}
```

FSMの遷移もpureである。

```rust
fn reduce_binding(
    binding: &CaptureBindingState,
    evidence: &AcceptedEvidence,
) -> BindingDecision
```

### 6.5 Effect Executorは状態を決めない

Decisionが返すEffectは、命令ではあるが、成功した事実ではない。

```rust
pub enum Effect {
    StartCapture {
        effect_id: EffectId,
        operation_id: OperationId,
        proposed_epoch: StreamEpoch,
        target: ResolvedTarget,
    },

    StopCapture {
        effect_id: EffectId,
        operation_id: OperationId,
        worker_id: WorkerId,
    },

    EnumerateEndpoints {
        effect_id: EffectId,
    },

    ScheduleTimer {
        effect_id: EffectId,
        timer_id: TimerId,
        fire_at: HostTime,
    },
}
```

Executorは実行結果をObservationとして返す。

```rust
pub enum ExecutionObservation {
    CaptureStarted {
        effect_id: EffectId,
        operation_id: OperationId,
        worker_id: WorkerId,
        epoch: StreamEpoch,
        actual_format: AudioFormatSnapshot,
    },

    CaptureStartFailed {
        effect_id: EffectId,
        operation_id: OperationId,
        error: CaptureFailure,
    },

    CaptureStopped {
        effect_id: EffectId,
        operation_id: OperationId,
        epoch: StreamEpoch,
    },
}
```

Executorが直接 `state.microphone = Running;` としてはいけない。あくまで「開始命令を実行した → 開始に成功したというObservationを返した → DecisionがRunningへ遷移した」という順序である。

ここで言う「Observationを返した」は、実際には「命令を送った」と「完了した」の間に曖昧な状態(実行はされたが完了通知だけ失われた、等)が起こり得ることを前提にする必要がある。その完了保証・冪等性・照合の設計は §7 で扱う。

### 6.6 Replayログ

Replayabilityを高めるなら、最低限次をappend-onlyで保存する。

```rust
pub enum JournalEntry {
    Input {
        seq: JournalSeq,
        input: DecisionInput,
    },

    Decision {
        seq: JournalSeq,
        previous_state_hash: StateHash,
        next_state_hash: StateHash,
        effects_hash: EffectsHash,
        diagnostics: Vec<DecisionDiagnostic>,
    },

    EffectDispatch {
        effect: Effect,
    },
}
```

基本的にはInputログだけで再実行できるが、Decision結果も保存すると、「以前の実装では何を判断したか」「現在の実装で同じログを流すと何が変わるか」を比較できる。ログには次も記録する。

```rust
pub struct ReplayHeader {
    pub schema_version: u32,
    pub reducer_version: String,
    pub application_build: String,
    pub configuration_hash: String,
}
```

### 6.7 Replayで重要なこと

**イベント順序を固定する**: 複数スレッドからObservationが来ても、Decisionへ入る前に一つの連番を付ける。

```rust
pub struct JournaledInput {
    pub journal_seq: u64,
    pub input: DecisionInput,
}
```

Decisionが扱う順番は `observed_at` ではなく `journal_seq` である。`observed_at` は鮮度判断に使うが、並び替えはしない。後から届いた古いObservationはAdmission Gateが拒否する(§6.2)。

**タイムアウトもイベントにする**: Decision内で時間を監視しない。Effect `ScheduleTimer { timer_id, fire_at }` に対し、Executorが時間になったら `DecisionInput::TimerFired { timer_id, fired_at }` を返す。これでタイマーも完全にreplayできる。

**IDを外部でランダム生成しない**: UUIDを使う場合でも、replay対象ではDecision入力として渡すか、Decision内の決定的カウンタから生成する。

### 6.8 Replayの種類

この構成なら4種類のデバッグができる。

* **Exact Replay**: 同じReducerバージョンで入力ログを再生し、state hashとeffects hashが一致するか確認する(入力列が同じ → 状態が同じ → Effect列も同じ)
* **Time-travel Debugging**: 任意の `journal_seq` まで再生し、その時点の状態を確認する(例: 「seq 1842時点でなぜMic-Aを停止しようとしていたのか」を調べられる)
* **Counterfactual Replay**: 新しいDecisionロジックに過去ログを流す(例: 「このstale callback拒否ルールを追加していたら過去の障害は回避できたか」を確認できる)
* **Fault Injection Replay**: ログを加工して重複・遅延・欠落・順序逆転・タイムアウト・古いepoch・二重完了を注入する。音声デバイス管理では特に有効

### 6.9 PCMデータはDecisionへ直接流さない

PCMフレームすべてをDecisionログに入れると量が大きくなりすぎる。制御面とデータ面を分ける。

```text
Data Plane
  PCMフレーム
  WAV
  フレームメタデータ

Control Plane
  stream started
  stream stopped
  signal active
  silence detected
  timestamp error
  discontinuity
  queue overflow
```

Decisionへは集約したObservationを送る。

```rust
pub enum SignalObservation {
    WindowAnalyzed {
        epoch: StreamEpoch,
        window_start: HostTime,
        window_end: HostTime,
        rms_milli_db: i32,
        peak_milli_db: i32,
        classified_as: SignalActivity,
    },
}
```

信号判定そのものもreplayしたいなら、別途PCMまたは特徴量ログを保存する。replayレベルを分ける。

```text
Level 1:
  制御イベントだけを再生

Level 2:
  フレームメタデータを含めて再生

Level 3:
  PCMから信号解析も含めて再生
```

### 6.10 Decision traceを残す

状態だけでなく、「なぜそう判断したか」を記録するとデバッグしやすくなる。

```rust
pub struct DecisionDiagnostic {
    pub rule_id: &'static str,
    pub input_seq: JournalSeq,
    pub binding: Option<BindingId>,
    pub message: String,
}
```

例:

```json
{
  "rule_id": "MIC_FOLLOW_DEFAULT_REBIND",
  "input_seq": 1842,
  "message": "default capture endpoint changed Mic-A -> Mic-B; stopping old epoch 12 before starting new endpoint"
}
```

拒否時も同様:

```json
{
  "rule_id": "REJECT_STALE_WORKER_STOP",
  "input_seq": 1851,
  "message": "worker stopped event epoch=11 ignored; current epoch=12"
}
```

### 6.11 State hashの注意

replayでhash比較するなら、状態のシリアライズが決定的である必要がある。

* `HashMap` ではなく `BTreeMap`
* 時刻は整数
* ppmや音量判定は可能なら固定小数点
* unorderedな集合は `BTreeSet`
* NaNを含む浮動小数点を状態へ入れない
* canonical serializationを使う

たとえば音量は、整数表現が扱いやすい。

```rust
pub struct MilliDecibel(pub i32);
```

---

## 7. Effectの完了保証と冪等性(Durable Effect Ledger)

### 7.1 なぜ必要か

ネットワークがなくても、**「実行されたが完了通知だけ失われた」「OSバッファには書けたが永続化前に落ちた」「停止命令は成功したがworker終了確認前にプロセスが落ちた」**という曖昧完了は起きる。§6 の `Effect` / `ExecutionObservation` はこの曖昧さをまだ明示的には扱っていない。

ただし、すべての副作用を一律に「再送」するのではなく、

> **永続化したEffect Intent → 実行 → Receipt → 検証/照合 → 完了確定**

にして、Effectごとに完了条件と再実行方法を定義するのがよい。

### 7.2 推奨モデル

```text
Pure Decision
    │
    │ EffectIntentを生成
    ▼
Durable Effect Ledger
    │
    │ 未完了Intentをdispatch
    ▼
Effect Executor
    │
    │ Receipt / Failure / Observation
    ▼
Verifier / Reconciler
    │
    ▼
Pure Decision
```

Effectのライフサイクルは次のようにする。

```rust
pub enum EffectStatus {
    Planned,

    Dispatched {
        attempt: u32,
    },

    Applied {
        receipt: ExecutionReceipt,
    },

    Verified {
        proof: VerificationProof,
    },

    RetryScheduled {
        attempt: u32,
        retry_at: HostTime,
        reason: RetryReason,
    },

    Indeterminate {
        reason: String,
    },

    FailedPermanent {
        error: ExecutionError,
    },
}
```

DecisionがEffectを生成した時点では、まだ成功扱いにしない。

```text
StartCaptureを送った
≠
Captureが開始された

SaveFileを送った
≠
ファイルが安全に保存された
```

### 7.3 Exactly-onceではなく、at-least-once + 冪等性 + 照合

一般に、実行側が処理を完了した直後、完了通知を記録する前にプロセスが落ちると、「実行済みなのか未実行なのか」を送信側だけでは判断できない。そのため、現実的な契約は次である。

```text
Effectは少なくとも1回実行される可能性がある
同じEffectが複数回届いても安全にする
不明な場合は現実状態を再観測して照合する
```

各Effectに一意な `effect_id` と `operation_id` を付ける。

```rust
pub struct EffectEnvelope {
    pub effect_id: EffectId,
    pub operation_id: OperationId,
    pub attempt: u32,
    pub payload: Effect,
}
```

Executor側も、処理済みEffectを認識できるようにする。

```rust
pub enum ExecutionReceipt {
    AlreadyApplied {
        effect_id: EffectId,
    },

    Applied {
        effect_id: EffectId,
        details: AppliedDetails,
    },
}
```

### 7.4 Effectごとの完了契約

#### 7.4.1 StartCapture

単に `IAudioClient::Start()` が成功しただけでは、実際にPCMが取得できることまでは保証しない。段階を分ける。

```text
StartCapture dispatch
    ↓
AudioClientActivated
    ↓
AudioClientStarted
    ↓
FirstFrameObserved
    ↓
CaptureOperational
```

状態も分けた方がよい。

```rust
pub enum StartProgress {
    Activating,
    Started,
    WaitingForFirstFrame,
    Operational,
}
```

再送時には同じ `operation_id` について新しいworkerを増殖させず、「すでに同operationのworkerが存在する → 状態を照合 → 起動済みならAlreadyApplied」とする。

#### 7.4.2 StopCapture

停止は比較的冪等にしやすい。Windowsの `IAudioClient::Stop()` は、停止に成功すれば `S_OK`、すでに停止済みなら `S_FALSE` を返す。したがって、命令を `Stop` ではなく `EnsureStopped` として扱えば再試行しやすくなる([Microsoft Learn][9])。

ただし完了条件は `Stop()` の戻り値だけではなく、

```text
audio stream停止
capture loop終了
worker thread join完了
COM resources解放
event handles解放
```

までである。

```rust
pub struct CaptureStoppedReceipt {
    pub worker_id: WorkerId,
    pub epoch: StreamEpoch,
    pub stop_result: StopResult,
    pub worker_joined: bool,
    pub resources_released: bool,
}
```

DecisionはこのReceiptを受けて初めて `Stopping → Stopped` へ進める。

#### 7.4.3 設定・JSON・summaryの保存

ファイル保存は、`write_all()` が成功しただけでは永続化完了ではない。

Rustの `File::sync_all()` は、OS内部のファイル内容とメタデータをファイルシステムへ同期するためのAPIで、`Drop` 時のcloseエラーは無視されるため、重要な保存では明示的に同期する必要がある([Rustドキュメント][10])。

Windowsでは `FlushFileBuffers` が指定ファイルのバッファをデバイスへ書き出すが、頻繁な呼び出しは高コストであることもMicrosoftが明記している([Microsoft Learn][11])。

推奨する保存手順は次である。

```text
1. 同じディレクトリにtemp fileを作る
2. 全内容を書き込む
3. flush
4. sync_all
5. tempを再オープンしてsize/hashを確認
6. ReplaceFileWまたはrenameで公開
7. 公開後のfileを再確認
8. SavedAndVerified receiptを返す
```

`ReplaceFileW` は、新ファイルへの書き込み、旧ファイルの退避、名前の置換、旧ファイル削除という複数操作をまとめて実行するWindows APIである。置換元・置換先・バックアップは同一volume上にある必要がある([Microsoft Learn][12])。

```rust
pub struct SaveArtifactEffect {
    pub artifact_id: ArtifactId,
    pub generation: u64,
    pub destination: PathBuf,
    pub expected_size: u64,
    pub expected_sha256: Sha256,
    pub bytes: Vec<u8>,
}
```

Receiptは次のようにする。

```rust
pub struct ArtifactSavedReceipt {
    pub artifact_id: ArtifactId,
    pub generation: u64,
    pub destination: PathBuf,
    pub actual_size: u64,
    pub actual_sha256: Sha256,
    pub synced: bool,
    pub atomically_published: bool,
}
```

同じEffectを再実行した場合は、「destinationに同generation・同hashが存在 → AlreadyApplied」とできる。design.md §12.2 のセグメント確定手順(`.partial`書き込み→flush→fsync→SHA-256→atomic rename→SQLite登録)は、この契約の具体例にあたる。

### 7.5 録音PCMは1パケットごとにEffect化しない

ここは重要である。すべてのPCMパケットを `Decision → Effect → Ack → Retry` へ流すと、リアルタイム録音性能を大きく損なう。制御面とデータ面を分ける。

```text
Control Plane
  StartCapture
  StopCapture
  OpenRecording
  CheckpointRecording
  FinalizeRecording

Data Plane
  PCM batch
```

PCM writerには、バッチ単位の連番を持たせる。

```rust
pub struct AudioBatch {
    pub stream_epoch: StreamEpoch,
    pub batch_seq: u64,
    pub first_packet_seq: u64,
    pub last_packet_seq: u64,
    pub frame_count: u64,
    pub checksum: u32,
    pub samples: Vec<f32>,
}
```

writerは次のhigh-watermarkを返す。

```rust
pub struct WriterProgress {
    pub accepted_through_batch: u64,
    pub durable_through_batch: u64,
    pub durable_through_packet: u64,
}
```

これにより、「チャネルへ送れた」「ファイルへ書けた」「永続化できた」を区別できる。

### 7.6 録音保存プロトコル

```text
OpenRecording
    ↓
WriterReady

AppendBatch 0
AppendBatch 1
AppendBatch 2
    ↓
Checkpoint(up_to_batch = 2)
    ↓
DurableThrough(2)

AppendBatch 3
AppendBatch 4
    ↓
Finalize(expected_last_batch = 4)
    ↓
RecordingCommitted
```

状態例である。

```rust
pub enum RecordingState {
    Closed,

    Opening {
        operation_id: OperationId,
    },

    Open {
        recording_id: RecordingId,
        accepted_through: Option<u64>,
        durable_through: Option<u64>,
    },

    Checkpointing {
        recording_id: RecordingId,
        requested_through: u64,
    },

    Finalizing {
        recording_id: RecordingId,
        expected_last_batch: u64,
    },

    Committed {
        artifact: CommittedRecording,
    },

    Failed {
        error: RecordingFailure,
    },
}
```

### 7.7 append再送の重複問題

単純なappendファイルに同じバッチを再送すると、二重書き込みになる。したがって、次のどちらかにする。

**固定offset書き込み**

```rust
pub struct BatchWrite {
    pub batch_seq: u64,
    pub expected_offset: u64,
    pub bytes: Vec<u8>,
    pub hash: Sha256,
}
```

同じoffsetに同じhashなら再実行可能である。

**chunk file方式**

```text
recording/
  chunk-00000000.pcm
  chunk-00000001.pcm
  chunk-00000002.pcm
  manifest.json
```

各chunkを、`create_new → write → sync → hash確認` で確定する。再送時に既存chunkがあればhashを確認し、「同じ → AlreadyApplied」「違う → 整合性エラー」とする。

スパイク段階では、chunk方式の方がreplay・障害注入・再送確認がしやすい。

### 7.8 WAVは最後に生成する方が安全

通常のWAVはヘッダーにデータ長を持つため、録音途中でプロセスが落ちると、ヘッダーがfinalizeされていない可能性がある。そのため内部保存は `PCM chunks / frame metadata / manifest` にし、録音完了後にWAVへ変換する方が安全である。

```text
Capture中:
  chunkとmanifestを確実に保存

Finalize後:
  committed recordingからWAVを生成
```

WAV生成も冪等Effectにできる。

```rust
Effect::BuildWav {
    source_recording_id,
    source_manifest_hash,
    destination,
}
```

### 7.9 Effect Ledger

DecisionStateに全Effectの詳細を無期限に持つ必要はないが、未完了Effectは正本として保持する。

```rust
pub struct PendingEffect {
    pub effect: Effect,
    pub status: PendingEffectStatus,
    pub attempt: u32,
    pub created_at: HostTime,
    pub last_dispatched_at: Option<HostTime>,
}
```

```rust
pub enum PendingEffectStatus {
    Ready,
    InFlight,
    AwaitingVerification,
    WaitingRetry,
    Indeterminate,
}
```

アプリ起動時は、次の順で扱う。

```text
未完了Effectを読み込む
    ↓
直接再送せず、まずreconcile
    ↓
未実行なら再実行
実行済みなら完了化
判定不能なら安全な補償処理
```

### 7.10 ReconciliationをEffectごとに定義する

```rust
pub trait EffectHandler {
    fn execute(
        &mut self,
        effect: &EffectEnvelope,
    ) -> ExecutionAttempt;

    fn reconcile(
        &mut self,
        effect: &EffectEnvelope,
    ) -> ReconciliationResult;
}
```

例である。

```text
SaveArtifact
  → path、generation、hashを確認

StartCapture
  → operation_idに対応するworkerとfirst-frame有無を確認

StopCapture
  → worker生存、stream状態、resource所有を確認

ScheduleTimer
  → timer_idが登録済みか確認

FinalizeRecording
  → manifest、last_batch、artifact hashを確認
```

### 7.11 失敗分類

再試行可否をExecutorではなくDecisionが判断できるよう、エラーを分類する。

```rust
pub enum FailureClass {
    Transient,
    RetryableAfterReconcile,
    Conflict,
    Corruption,
    InvalidRequest,
    Permanent,
}
```

例である。

```text
一時的なファイルロック
  → Transient

保存成功後にReceipt記録前クラッシュ
  → RetryableAfterReconcile

既存ファイルのgenerationは同じだがhashが違う
  → Conflict / Corruption

存在しないPinned endpoint
  → 実行失敗ではなくWaiting状態

不正フォーマット
  → Permanent
```

### 7.12 どこまでやるべきか(保証レベル)

全Effectについて、最低限これは必要である。

```text
一意なeffect_id
完了Observation
タイムアウト
再試行可否
冪等性戦略
照合方法
```

ただし、全Effectに毎回disk syncやreadback hashが必要なわけではない。保証レベルを定義するとよい。

```rust
pub enum CompletionGuarantee {
    Accepted,
    Applied,
    Observed,
    Durable,
    Verified,
}
```

例である。

```text
UI状態通知
  → Acceptedでよい

StartCapture
  → Observed(first frame)まで

StopCapture
  → Observed(worker joined)まで

設定保存
  → Durable

最終録音成果物
  → Verified

途中PCM
  → Checkpoint単位でDurable
```

### 7.13 結論

**ネットワークでないから不要、ではない。** 特に本プロジェクトのように、録音データを失いたくない・デバイス切替がある・workerが複数存在し得る・replayabilityを重視する・障害後に状態を復元したい、というシステムでは、Effect完了の確認と再実行設計は有効である。

ただし設計の中心は「全部を再送する」ではない。

> **Effect Intentを保存し、完了を観測し、未確認ならまず現実と照合し、必要な場合だけ冪等に再実行する**

とするのが適切である。

---

## 8. 責務分離まとめ

**Observation Layer**: 外界から何が報告されたかを記録する。報告を真実とは見なさない。状態を書き換えない。

**Admission Layer**: 世代・鮮度・順序・出所・対象・重複をpureに検査する。

**Decision Layer**: Beliefを更新する。FSMを遷移する。Effectを発行する。唯一のdomain state writer。

**Execution Layer**: Effectを実行する。成功・失敗をObservationとして返す。domain stateを変更しない。

この構造なら、今回の設計は単なるFSMではなく、**観測を信じすぎない、再生可能なイベント駆動Decision Engine**になる。これは、以前のIME設計で採用している `Observe → Pure → Apply` を音声デバイス管理に拡張した形であり、一貫性がある。

```text
Observe:
    IMMNotificationClient
    IAudioSessionEvents
    WASAPI result
    Process watcher

Pure:
    Endpoint registry update
    Binding FSM transition
    stale event rejection
    retry policy
    (§6 では Admission Gate と Decision Engine に分割)

Apply:
    COM activation
    stream start/stop
    timer scheduling
    file writer control
```

---

## 9. 関連文書

* [design.md](design.md) §12.2 — セグメント確定手順(`.partial`→flush→fsync→SHA-256→atomic rename→DB登録)。§7.4.3/§7.6〜7.8 の完了契約・chunk方式・WAV遅延生成は、この手順を一般化・深掘りしたもの
* [design.md](design.md) §16.5 — デバイス切替方針(Fixed selected device / Follow system default / Ask before switching)の運用方針。本書はその実装アーキテクチャにあたる
* [spike-plan.md](spike-plan.md) SPIKE-04(セグメント確定とクラッシュ復旧) — §7 の完了保証・冪等性・reconciliationの検証対象と重なる
* [spike-plan.md](spike-plan.md) SPIKE-11(Audio Endpoint Registry)/ SPIKE-12(Capture Rebinding State Machine) — 本書 §2〜5 の Fake Worker 版を先に検証する実施計画
* [spike-windows-01-02-detail-design.md](spike-windows-01-02-detail-design.md) — `capture_epoch` の元となる実装(§5.5 で `stream_epoch` として一般化)

---

## 参考資料

[1]: https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient "IMMNotificationClient (mmdeviceapi.h) - Win32 apps | Microsoft Learn"
[2]: https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-immnotificationclient-ondevicestatechanged "IMMNotificationClient::OnDeviceStateChanged (mmdeviceapi.h) - Win32 apps | Microsoft Learn"
[3]: https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-immnotificationclient-ondefaultdevicechanged "IMMNotificationClient::OnDefaultDeviceChanged (mmdeviceapi.h) - Win32 apps | Microsoft Learn"
[4]: https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-state-xxx-constants "DEVICE_STATE_XXX Constants (Mmdeviceapi.h) - Win32 apps | Microsoft Learn"
[5]: https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nf-audiopolicy-iaudiosessionevents-onsessiondisconnected "IAudioSessionEvents::OnSessionDisconnected (audiopolicy.h) - Win32 apps | Microsoft Learn"
[6]: https://learn.microsoft.com/en-us/windows/win32/api/endpointvolume/nf-endpointvolume-iaudioendpointvolume-getmute "IAudioEndpointVolume::GetMute (endpointvolume.h) - Win32 apps | Microsoft Learn"
[7]: https://github.com/microsoft/Windows-classic-samples/tree/main/Samples/ApplicationLoopback "Windows-classic-samples/Samples/ApplicationLoopback at main · microsoft/Windows-classic-samples · GitHub"
[8]: https://docs.rs/tokio/latest/tokio/macro.select.html "select in tokio - Rust"
[9]: https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-stop "IAudioClient::Stop (audioclient.h) - Win32 apps | Microsoft Learn"
[10]: https://doc.rust-lang.org/std/fs/struct.File.html "File in std::fs - Rust"
[11]: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers "FlushFileBuffers function (fileapi.h) - Win32 apps | Microsoft Learn"
[12]: https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew "ReplaceFileW function (winbase.h) - Win32 apps | Microsoft Learn"
