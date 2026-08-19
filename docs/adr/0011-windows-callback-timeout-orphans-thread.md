# 0011: Windowsのcallback timeout(500ms)が生存中スレッドを孤児化し、二重キャプチャを誘発する

- Status: Implemented (Windows実機/CI検証待ち) — Decision 1(`CaptureEvent::StreamStalled`の新設)・Decision 2(`windows_supervisor.rs`側でStreamStalledをreapしない)を実装。`capture_loop.rs`のcallback timeout分岐が`StreamError`ではなく`StreamStalled`を送るよう変更。Decision 4(`reap_dead_worker`のoperation_idガード)は[0010](0010-windows-devicelost-not-reaching-fsm.md)と共通実装。Decision 3(`CALLBACK_TIMEOUT_MS`の実測チューニング)は実機が必要なため見送り。`cargo check --target x86_64-pc-windows-gnu`で`capture-windows`・`app-service --features windows-supervisor`両方のクロスコンパイルを確認済み。
- 該当箇所: `crates/capture-windows/src/capture_loop.rs:376-380`
  (`WaitResult::Timeout`分岐、`CaptureEvent::StreamError`送信後に`continue`)、
  `crates/app-service/src/windows_supervisor.rs:318-323`(`StreamError`アーム、
  `WorkerFailed`としてFSMに伝播 + `reap_dead_worker`呼び出し)、
  `apps/desktop/src/recording.rs:101`(`CALLBACK_TIMEOUT_MS = 500`、
  「Not yet tuned against real hardware」とコメントあり)
- 発見経緯: [0001](0001-macos-scstream-error-callback-unverified.md)のOpusレビュー
  (2026-08-18)で発見。

## Context

`capture_loop.rs:376-380`の`WaitResult::Timeout`分岐は、WASAPIコールバックが
500ms以内に来なかった場合に`CaptureEvent::StreamError { error: "callback
timeout(500ms)" }`を送信して**`continue`する**(ストリーム自体は生きたまま
継続される、非致命的な「ストール通知」のつもりの実装)。

ところが`windows_supervisor.rs:318-323`の`StreamError`アームはこれを
**致命的失敗(`WorkerFailed`)としてFSMに流し**、さらに`reap_dead_worker`を
呼ぶ。reapは`WorkerHandle`を`workers`から取り除き、**まだ動いているスレッド**
に対してjoinerを仕掛ける。しかし`worker.stop`は一度もsignalされないまま
Arcごとdropされるため、**joinerスレッドは永久に`join()`でブロックし、
`pending_joins`が減らない**。

FSMは`Retrying`に落ちて`StartCapture`を再実行するため、**同じデバイスに
対して2本目のキャプチャスレッドが立ち上がる**。孤児化した1本目のスレッドは
タイムアウトのたびに`StreamError`を送り続け、それが新しいworkerの
`operation_id`に対する`WorkerFailed`として誤って解釈されることもありうる。
`MAX_RETRY_ATTEMPTS = 5`に到達すると、trackは`Failed`状態で固定される。

`CALLBACK_TIMEOUT_MS = 500`はコメントに「Not yet tuned against real hardware」
とある通り未調整の値で、システム負荷時やデバイス切替時など現実的な条件で
踏める閾値である。

副作用として、停止時に`drain_pending_joins`が(joinが永久に返らないjoinerを
待つため)最大10秒ブロックする — 録音停止操作が最大10秒固まる可能性がある。

根本原因は、`capture_loop`側が「非致命的なストール通知」のつもりで送っている
`StreamError`イベントを、`windows_supervisor`側が「致命的な失敗」として
一律に解釈していることにある。

## Decision

1. `CaptureEvent`に`StreamStalled`(非致命的、ストリーム継続中)と
   `StreamError`(致命的、ストリーム終了)を分離する。現状の
   コールバックタイムアウトは`StreamStalled`として送るよう`capture_loop.rs`を修正する。
2. `windows_supervisor.rs`側で`StreamStalled`受信時は`reap_dead_worker`を
   呼ばない(スレッドは生きているため)。連続ストール回数に応じたヘルス表示の
   劣化(例: `Degraded`状態)は別途検討する。
3. `CALLBACK_TIMEOUT_MS = 500`を実機で計測し、負荷時の誤検知率を確認したうえで
   妥当な値にチューニングする。
4. `reap_dead_worker`がまだ生存しているスレッドに対して誤ってjoinerを
   仕掛けないよう、workerの生死判定を`stop`シグナルの有無と紐づけて
   厳密化する。
5. リグレッションテストとして、`windows_supervisor`のイベント処理ロジックに
   「コールバックタイムアウトが発生してもストリームは継続し、2本目の
   キャプチャスレッドが立たないこと」を検証するテストを追加する
   ([0003](0003-capture-windows-zero-test-coverage.md)の投資再配分方針)。

## Consequences

- 対応した場合: システム負荷時のコールバック遅延で、無関係な二重キャプチャや
  録音停止のハングが発生するリスクがなくなる。
- 対応しない場合: 実機での負荷条件下(CPU高負荷、他アプリのオーディオ処理と
  の競合など)で、ユーザーが原因不明の録音停止ハングやtrack失敗を経験する
  リスクが残る。
