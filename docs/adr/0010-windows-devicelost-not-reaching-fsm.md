# 0010: Windowsで CaptureExit::DeviceLost がrebinding FSMに一度も届かない

- Status: Implemented (Windows実機/CI検証待ち) — Decision 1〜3を実装。`windows_supervisor.rs`の`StreamStopped`ハンドラを、`self.workers`に該当bindingが残っている(=`Effect::StopCapture`を発行していない自発終了)場合に`Observation::WorkerFailed`経由でFSMへ伝播しreapするよう修正。`reap_dead_worker`にoperation_idガードを追加(Decision 3、[0011](0011-windows-callback-timeout-orphans-thread.md)のレース対策と共通)。`cargo check --target x86_64-pc-windows-gnu -p app-service --features windows-supervisor`でクロスコンパイル確認済み(このリポジトリの既存の検証手法と同じ)、実行時の実機検証は未実施。Decision 4(リグレッションテスト)は`windows_supervisor.rs`自体がwindows-rs依存で単体テスト実行できないため見送り、[0012](0012-accepts-epoch-guard-never-called.md)経由で`capture-api`側にテストを追加した。
- 該当箇所: `crates/capture-windows/src/capture_loop.rs`(`AUDCLNT_E_DEVICE_INVALIDATED`検知、
  293/317/372行)、`crates/app-service/src/windows_supervisor.rs`(325-330行、
  `CaptureEvent::StreamStopped`をinformational onlyとして破棄)、
  `crates/capture-api/src/rebinding.rs`(659-677行、`handle_join_result`)
- 発見経緯: [0001](0001-macos-scstream-error-callback-unverified.md)のOpusレビュー
  (2026-08-18)で、Windows側の対応する経路を確認した結果として発見。

## Context

`capture_loop.rs:293/317`はWASAPIの`AUDCLNT_E_DEVICE_INVALIDATED`を検知して
`Ok(CaptureExit::DeviceLost)`を返す実装になっている。しかしこの復帰は
**誰にも観測されない**:

- `CaptureEvent::StreamStopped`は`windows_supervisor.rs:325-330`で
  「informational only」として破棄される。
- コード上「join結果が権威」とコメントされているが、joinerをspawnするのは
  `Effect::StopCapture`(266行)と`reap_dead_worker`(357行)だけである。
  **自発終了したスレッドには誰も`join()`を仕掛けないため、`JoinResult`自体が
  発生しない。**
- 仮に発生したとしても、`handle_join_result`が投げる`Observation::WorkerStopped`は
  `rebinding.rs:659-677`でbindingが`Stopping`状態でなければ即`Vec::new()`を返す。
  `Running`のままのbindingには**何も起きない**。

`IMMNotificationClient`側の`OnDeviceRemoved`/`OnDeviceStateChanged`が別途飛ぶ
ケース(物理的な抜線)は`EndpointRemoved`経由で救われる。しかし
**エンドポイントが消えずにストリームだけ無効化されるケース**
(フォーマット変更、排他モード奪取、オーディオエンジン/ドライバ再起動、
Bluetooth再ネゴシエーション)では、bindingは`Running`のまま、
`capture_health()`は`Ok`のまま、フレームは永久に来ない。

これは[0001](0001-macos-scstream-error-callback-unverified.md)がmacOSについて
懸念している「録音は続いて見えるが実際は無音」という最悪のシナリオが、
**Windowsでは権限剥奪のような特殊条件なしに、通常のドライバ挙動だけで成立する**
ことを意味する。

## Decision

1. `windows_supervisor.rs`側で、`CaptureEvent::StreamStopped`
   (自発的な`DeviceLost`終了)を「informational only」として破棄するのをやめ、
   `WorkerFailed`相当のObservationとしてFSMに伝播させる。
2. 自発終了したスレッドに対しても`join()`を仕掛ける経路を用意する
   (例: スレッド終了を検知するchannelを常時watchし、終了検知時に
   `reap_dead_worker`相当の処理を呼ぶ)。
3. `handle_join_result`が`Running`状態のbindingに対しても`WorkerStopped`を
   正しく処理できるよう、FSMの遷移条件を見直す(現状`Stopping`状態でなければ
   無視される設計が、この自発終了ケースを想定していない可能性がある)。
4. リグレッションテストとして、`app-service`のsupervisor層(OS非依存部分)に
   「エンドポイントは残ったままストリームだけ無効化される」ケースを
   模したテストを追加する([0003](0003-capture-windows-zero-test-coverage.md)の
   投資再配分方針に合致)。

## Consequences

- 対応した場合: Windowsでのドライバ起因の無音録音が、macOS側と同水準の
  検知・復旧ロジックでカバーされるようになる。
- 対応しない場合: Windowsユーザーが、フォーマット変更や排他モード奪取などの
  日常的に起こりうる条件で、気づかれないまま無音のセッションを記録し続ける
  リスクが残る。この機能群全体の目的(デバイス切断検知・復旧)が、
  Windowsの最も一般的な故障モードに対して機能していない。
