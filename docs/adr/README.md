# ADR: デバイス切断検知・復旧機能 pre-mortem対応

対象コミット群(2026-07時点の直近13コミット、capture-api / app-service / capture-macos /
capture-windows / apps/desktop / control-protocol / local-broker / transcript-event)で
実装した「録音デバイスの切断検知・復旧・健全性可視化」機能について、pre-mortem分析
(2026-08-18実施、Sonnetによる初回分析)で洗い出した不具合リスクをADRとして記録する。
続けて同日、Opus(別モデル)による独立レビューを行い、事実誤認の修正・深刻度の較正・
新規リスクの発見(0010〜0014)を反映した。

各ADRは「このまま放置すると何が起きるか(Context)」「何をするか(Decision)」
「対応しない場合/した場合の影響(Consequences)」の形式。

> **形式についての注記**: これらは本来のADR(複数の選択肢を比較し意思決定を記録する
> もの)というよりは「リスク台帳+対応タスク」に近い内容である。Opusレビューでも
> この形式のミスマッチが指摘されたが、運用上の理由でADRファイルの形式を維持したまま
> 内容を修正する方針とした。`Status`は「対応の要否・状態」を表す運用上の意味で
> 使っている(標準的なADRの`Proposed`/`Accepted`とは意味が異なる点に注意)。

2026-08-18、上記14件のうち実装可能なものをコードに反映した。詳細は各ADRの`Status`行を参照。

| ADR | 深刻度 | Status | タイトル |
|---|---|---|---|
| [0001](0001-macos-scstream-error-callback-unverified.md) | 高 | Implemented(検証待ち) | macOS SCStreamのdid_stop_with_errorパスが実機E2Eでも未検証 |
| [0002](0002-macos-e2e-best-effort-job-silently-non-blocking.md) | 中 | Implemented | macOS E2Eジョブがcontinue-on-errorで退行を検知してもCIが緑のまま |
| [0003](0003-capture-windows-zero-test-coverage.md) | 高 | 一部対応 | capture-windowsクレートにテストが皆無 |
| [0004](0004-binding-health-unavailable-reason-not-distinguished.md) | 低 | Deferred | BindingHealth::UnavailableがDeviceUnavailableとProcessNotFoundを区別しない |
| [0005](0005-macos-duplicate-device-enumeration-listeners.md) | 低 | Implemented(検証待ち) | macOS設定画面のデバイス監視Dropが同期joinでUIスレッドを止めうる |
| [0006](0006-reconcile-device-list-composite-diff-untested.md) | 低 | 対応不要 | reconcile_device_list合成挙動のsupervisorレベルテストが未整備 |
| [0007](0007-session-id-fix-bundled-in-refactor-commit.md) | 低 | 対応不要 | session_idランダム値バグの修正がリファクタコミットに暗黙に同梱 |
| [0008](0008-poll-capture-health-fragile-grep-matching.md) | 低 | **Implemented(検証済み)** | poll-capture-health.shのgrepベースJSON照合が脆く、ctl失敗時に無言で死ぬ |
| [0009](0009-macos-supervisor-shutdown-skips-final-health-publish.md) | — | **Rejected** | シャットダウン時の最終health publish欠落(実害なしと確定、対応不要) |
| [0010](0010-windows-devicelost-not-reaching-fsm.md) | **高** | Implemented(検証待ち) | Windowsで CaptureExit::DeviceLost がrebinding FSMに一度も届かない |
| [0011](0011-windows-callback-timeout-orphans-thread.md) | **高** | Implemented(検証待ち) | Windowsのcallback timeoutが生存中スレッドを孤児化し二重キャプチャを誘発する |
| [0012](0012-accepts-epoch-guard-never-called.md) | 中 | Implemented(検証待ち) | accepts_epochガードが実装済みなのにどこからも呼ばれていない |
| [0013](0013-macos-shared-scstream-redundant-rebuild.md) | 中 | Implemented(検証待ち) | macOSで共有SCStream死亡時に余計な再構築が1往復挟まる |
| [0014](0014-macos-frame-count-stereo-doubling-suspected.md) | 中(要検証) | Implemented(検証待ち) | sc_stream.rsのframe_count計算がステレオ時に誤っている疑い |

## 総評(2026-08-18 Opusレビュー後に改訂)

初回分析(Sonnet)は「macOS側のOSコールバック層・E2E層の検証密度が低い」ことを
最大のリスクとして指摘したが、Opusによる独立レビューの結果、**より深刻なのは
Windows側**であることが判明した。0010(`CaptureExit::DeviceLost`がFSMに届かない)は、
エンドポイントが物理的に外れなくても(フォーマット変更・排他モード奪取・ドライバ
再起動など)、`Running`状態のまま無音録音が継続しうるという、この機能群の目的
そのものを損なう欠陥である。0011(孤児スレッドによる二重キャプチャ)と0012
(`accepts_epoch`ガードの未配線)は互いに絡み合い、無効なフレームが録音データに
混入する経路を作る。

`crates/capture-api`の純粋なFSM(`rebinding.rs`)はcargo-mutants 64/64を達成しており
単体としては固いが、これは「FSM自身のロジックが正しい」ことしか保証しない。
「FSMが用意した安全弁(`accepts_epoch`)が呼び出し側で実際に使われているか」
「OSイベントがFSMに正しく届いているか」という**配線の正しさ**はミューテーション
カバレッジの対象外であり、この機能群で最も踏み抜きやすい欠陥は結局そこから出ている。

次に着手すべき優先順位は概ね以下の通り: **0010・0011(Windowsの無音録音リスク) >
0012(epochガードの配線) > 0001・0003(検証基盤の整備) > 0002(可視化) >
0004〜0009(軽微・低リスク)**。0014は疑いの段階のため、まず実機検証で
真偽を確定させることが先決。

## 実装状況(2026-08-18)

上記優先順位に沿って、0010・0011・0012・0013・0014・0001(部分)・0002・0005・0008を実装し、
0004は保留、0006・0007・0009はコード変更不要と判断した。

**検証状況**: このLinuxサンドボックスでは`capture-windows`(windows-rs依存)は
`cargo check --target x86_64-pc-windows-gnu`でクロスコンパイルチェックが可能で、
実際に全変更をこの方法で確認済み。一方`capture-macos`は依存クレート`screencapturekit`
のビルドスクリプトが`swiftc`を要求するため、`cargo check`すら通せず**一切のコンパイル
確認ができていない**(macOS側の全変更はコードレビューのみでの実装)。`crates/capture-api`
(OS非依存のFSM本体)は完全にローカルでビルド・テスト可能で、`cargo test -p capture-api`は
19/19 pass。`scripts/ci/poll-capture-health.sh`はモックの`ctl`スクリプトで4パターン
(成功/タイムアウト/ctl異常終了/構造化JSON値)を実行し動作確認済み。実機・実CIでの
最終検証はいずれも未実施。

**副次的に発見した既存バグ(このADR群の対象外)**: `cargo check --target
x86_64-pc-windows-gnu -p app-service --features windows-supervisor`を実行したところ、
本ADR群とは無関係に、`crates/app-service/src/live_transcription.rs:2255`で
`LocalBroker`型が見つからないというコンパイルエラーが発生することを確認した(`git
stash`でこの実装の変更を除いた状態でも同じエラーが再現するため、5b3e7b0のlocal-broker
リファクタ以降に混入した既存の回帰と見られる)。デフォルトのLinuxビルド
(`cargo check -p app-service`、windows-supervisor機能なし)は問題なく通るため、CI上の
`windows-app-build.yml`等がこの機能組み合わせでクロスコンパイルチェックしているかは
別途確認が必要。本ADR群のスコープ外のため未修正のまま残している。

## 実装レビューと追加修正(2026-08-18、Opusによる独立レビュー)

上記の初回実装一式を、別モデル(Opus)に独立レビューさせたところ、**致命的な不具合3件・
要修正2件・軽微7件**が見つかった。特に致命的な3件は、このADR群自体が解消しようとしていた
問題を別経路で再導入してしまっていたもので、いずれも修正済み。

**致命的(修正済み)**
- **F1**: `MacosSupervisor::reconcile_active_stream`が共有ストリームのepochを
  `desired`bindingの**最大値1つ**で決めていたのに対し、`decide()`はbindingごとに
  独立したepochを払い出す(`start_all`がMicrophone→EndpointLoopbackの順に別々の
  `decide()`呼び出しで開始するため、通常E, E+1のように分かれる)。結果、
  `accepts_epoch`配線(0012)により**Micトラックの全フレームが常時破棄される**
  という新規の重大回帰が入っていた。`ScreenCaptureKitStream`/`FrameForwarder`を
  binding単位で epoch を持つよう再設計して解消(`sc_stream.rs`, `macos_supervisor.rs`)。
- **F2**: Windowsの`StreamStopped`ハンドラが、コメント上は`exit == DeviceLost`の
  ケースのみ救うと書きながら**実際にはexitの値で分岐しておらず**、正常なリバインド
  (`Effect::StopCapture`)完了時に必ず飛ぶ`StreamStopped{StoppedByRequest}`が、
  `join_result_rx`と`capture_rx`のSelect順序次第で起動直後の健全な新workerに
  誤って`WorkerFailed`判定を下すレースがあった(ADR 0011で潰した孤児スレッド/
  二重キャプチャ問題を別経路で再導入)。`exit == CaptureExit::DeviceLost`の
  フィルタを追加して解消(`windows_supervisor.rs`)。
- **F3**: `macos-build.yml`のjob-level `permissions:`に`issues: write`のみ
  列挙していたため、GitHub Actionsの仕様上`contents: read`が落ち、privateリポジトリ
  である本リポジトリでは`actions/checkout`が失敗しE2Eジョブ全体が動かなくなっていた。
  `contents: read`を明示して解消。

**要修正(修正済み)**
- **F4**: Issue自動起票で使う`e2e-best-effort-failure`ラベルがリポジトリに
  存在せず、`gh issue create --label`が失敗していた。ラベル依存をやめ、固定タイトルの
  完全一致検索による重複防止に変更。
- **F5**: `poll-capture-health.sh`で、ctlの非ゼロ終了は捕捉していたが、
  **jqのパース失敗(不正なJSON出力)は`set -e`で無言exit**していた(ADR 0008が
  排除しようとした故障モードの再来)。jq呼び出しにもエラーハンドリングを追加し
  5パターン目として実測確認済み。

**軽微(修正済み)**
F7(`workers.insert`が既存workerを無言上書きする経路への防御ログ追加)、
F8(コメントの事実誤認`host_time_ns`→`source_time_ns`の訂正)、
F9(channels値がマイク出力の実チャンネル数と一致する保証がない旨をコメントで明記)、
F10(`windows_device_watch.rs`が同じDrop問題を抱えたまま未修正だった非対称性を解消)、
F11(macOSのsibling待ち合わせが`contains_key`だけでは「未報告」と「retryで再起動済み」を
区別できなかった問題を、`SharedStream`にoperation_idのスナップショットを持たせて解消)、
F12(smoke_testが`StreamStalled`を無言で握りつぶすようになっていた点にWARN出力を追加)
をそれぞれ修正。F13(テストカバレッジのギャップ)はプラットフォーム制約上コードでは
閉じられないため、既知の限界として記録するに留めた。

再検証: `cargo test -p capture-api`(19/19 pass)、
`cargo check --target x86_64-pc-windows-gnu -p capture-windows --all-targets`、
`cargo check --target x86_64-pc-windows-gnu -p app-service --features windows-supervisor`
(既存の無関係な`LocalBroker`エラーのみ残存、本実装由来のエラーなし)、
`cargo check -p app-service`(Linuxデフォルト)を全て再実行し、
`poll-capture-health.sh`は5パターン(成功/タイムアウト/ctl異常終了/構造化JSON値/
不正JSON出力)をモックスクリプトで再実行して確認した。macOS側の変更は
このサンドボックスの制約上、今回もコンパイル未確認のまま(レビューのみ)。
