# 0012: accepts_epochガードが実装済みなのにどこからも呼ばれていない

- Status: Implemented (Windows/macOSともに実機検証待ち) — Decision 1を実装: `capture_api::rebinding::DecisionState`に新規public API `accepts_epoch(binding, epoch)`を追加し(既存の`CaptureBindingState::accepts_epoch`をラップ)、`windows_supervisor.rs`・`macos_supervisor.rs`双方の`CaptureEvent::Frame`ハンドラで、stale epochのフレームを`frame_tx`へ転送する前に破棄するよう配線した。Decision 2(新規テスト)は`capture-api`側で完全に実施・確認済み(`cargo test -p capture-api`で19/19 pass、`decision_state_accepts_epoch_*`の2テストを追加)。Decision 3(README総評の訂正)も反映。`windows_supervisor.rs`側はクロスコンパイル確認済み、`macos_supervisor.rs`側はこのサンドボックスでは`capture-macos`のswiftc依存によりコンパイル確認不可(レビューのみ)。
- Severity: 中(単独では潜在的欠陥だが、[0011](0011-windows-callback-timeout-orphans-thread.md)
  のような孤児スレッドが存在するシナリオで実害が顕在化する)
- 該当箇所: `crates/capture-api/src/rebinding.rs:141-143`
  (`CaptureBindingState::accepts_epoch`)、テストは`crates/capture-api/tests/scenarios.rs:613`
- 発見経緯: [0001](0001-macos-scstream-error-callback-unverified.md)のOpusレビュー
  (2026-08-18)で発見。

## Context

`CaptureBindingState::accepts_epoch`(`rebinding.rs:141-143`)は、stale epoch
(古い世代のキャプチャスレッドに由来する)フレームを受け入れないためのガードとして
実装され、単体テスト(`scenarios.rs:613`)も存在する。

しかし、実際に`capture_epoch`をチェックしてフレームをフィルタする呼び出し箇所が
`windows_supervisor.rs` / `macos_supervisor.rs` / `windows_frame_collector.rs` /
`macos_frame_collector.rs`の**どこにも存在しない**(grepで確認済み)。
`capture_epoch`はフレームに刻まれるだけで、誰もこの値でフィルタしていない。

結果として、[0011](0011-windows-callback-timeout-orphans-thread.md)のように
旧epochのスレッドが孤児化して生き残った場合、そのスレッドが送り続けるフレームが
そのままタイムラインに混入してしまう。安全弁は用意されているが配線されていない。

なお、README(pre-mortem分析の総評)で「`rebinding.rs`はcargo-mutants 64/64達成で
かなり固い」と評価していたが、これは正確には「FSM単体のロジックが固い」という
意味に過ぎず、「FSMが用意した安全弁(`accepts_epoch`)が実際に呼び出し側で
使われているか」はミューテーションカバレッジでは検出できない範囲だった。
この点、当初の総評は誤解を招く書き方だったため訂正する。

## Decision

1. `windows_frame_collector.rs`/`macos_frame_collector.rs`(または対応する
   supervisor層)で、受信したフレームの`capture_epoch`を現在のbinding状態と
   照合し、`accepts_epoch`が`false`を返す場合はフレームを破棄するよう配線する。
2. 配線後、[0011](0011-windows-callback-timeout-orphans-thread.md)の
   孤児スレッドシナリオを模したテストで、stale epochのフレームが実際に
   破棄されることを確認する。
3. README(`docs/adr/README.md`)の総評を、「FSM単体は固いが、周辺の配線
   (呼び出し側)の検証密度は別問題」という趣旨に修正する。

## Consequences

- 対応した場合: 孤児スレッドや遅延した旧epochのフレームがタイムラインに
  混入するリスクがなくなる。
- 対応しない場合: [0010](0010-windows-devicelost-not-reaching-fsm.md)・
  [0011](0011-windows-callback-timeout-orphans-thread.md)のようなシナリオで、
  無効なフレームが録音データに混入する二次被害が発生しうる。
