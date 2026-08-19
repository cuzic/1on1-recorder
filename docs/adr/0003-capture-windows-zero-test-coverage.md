# 0003: capture-windowsクレートにテストが皆無

- Status: Partially addressed — Decision 1(windows-cargo-mutants.ymlのコミット)はユーザー確認待ちで未実施。Decision 2の投資再配分方針通り、`is_device_invalidated`自体のテストは追加せず、代わりに[0010](0010-windows-devicelost-not-reaching-fsm.md)・[0011](0011-windows-callback-timeout-orphans-thread.md)・[0012](0012-accepts-epoch-guard-never-called.md)の修正と`capture-api`側のテスト追加(`DecisionState::accepts_epoch`等)で実質的にsupervisor層の防御を強化した。`windows_supervisor.rs`自体はwindows-rs依存のため単体テストが書けず(このクレートはターゲット`x86_64-pc-windows-gnu`でのクロスコンパイルチェックのみ可能、実行不可)、レビュー+クロスコンパイルチェックで検証。
- Severity: 高
- 該当箇所: `crates/capture-windows/src/capture_loop.rs`(`is_device_invalidated`,
  `CaptureExit::DeviceLost`を返す3箇所: 293/317/372行)、`device_watch.rs`、`mic_stream.rs` 他、
  `src/`配下12ファイル全てで`#[test]`ゼロ
- 関連: 未コミットの`.github/workflows/windows-cargo-mutants.yml`
- 関連ADR: [0010](0010-windows-devicelost-not-reaching-fsm.md)(本ADRが提案するテスト対象を
  実際に見直した結果発見した、より深刻な欠陥)

## Context

`capture-windows`クレート(`src/`配下12ファイル、当初「13ファイル」としていたが誤り)には
WASAPIの`AUDCLNT_E_DEVICE_INVALIDATED`判定、`IMMNotificationClient`経由のデバイス変化検知、
`CaptureExit::DeviceLost`への分岐が実装されているが、`#[test]`が1つも存在しない。

未コミットの`.github/workflows/windows-cargo-mutants.yml`はこの課題を明示的に
認識しており、コメント中に「2026-07-24時点でこのcrateには#[test]が一つも無い」
「ほぼ全mutantがmissedになるのが期待される結果であり、それ自体が調査目的そのもの」
と書かれている。つまり現状は「課題を発見するワークフローを用意した」段階で止まっており、
実際のテスト追加はまだ行われていない。

(2026-08-18 Opusレビューによる修正: 当初「macOS側は単体テスト・実機E2Eが整備された一方
Windowsだけ皆無」という非対称な構図で説明していたが、これは誇張だった。実際には
`capture-macos`の`#[test]`も`timestamp.rs`の7件のみで、`sc_stream.rs`/`device_watch.rs`/
`device_select.rs`はゼロ。さらに`app-service`の`windows_supervisor.rs` /
`macos_supervisor.rs` / `windows_session.rs` / `macos_session.rs`は**全て`#[test]`ゼロ**。
実態は「`capture-api`の純粋FSMだけが厚く、両OSのアダプタ層とsupervisor層が等しく薄い」
であって、OS間の非対称ではない。)

## Decision

1. まず未コミットの`.github/workflows/windows-cargo-mutants.yml`をコミットし、
   `workflow_dispatch`で一度実行してmissed mutantの実際の内容を確認する。
2. **(2026-08-18修正: 投資先の再配分)** `is_device_invalidated`(実装は
   `e.code() == AUDCLNT_E_DEVICE_INVALIDATED`の1行)の単体テストは、書けるものが
   トートロジーになりcargo-mutantsのmissedは潰せても防御力はほぼゼロ。実際に
   穴が空いているのは**OS非依存でテスト可能な`app-service`のsupervisor層**
   (`windows_supervisor.rs`が`CaptureExit::DeviceLost`/`StreamError`をどう解釈し
   FSMへどう伝播させるか)であり、ここに投資を振り替える。具体的には
   [0010](0010-windows-devicelost-not-reaching-fsm.md)・
   [0011](0011-windows-callback-timeout-orphans-thread.md)で発見した欠陥の
   リグレッションテストを先に書く。
3. `capture_loop.rs`側で単体テスト化できない実機依存部分(実際のWASAPIコールバック等)は、
   `cargo-ci-gcp-spot-instance`スキルのWindows Spot VM経由でE2E相当の検証を検討する。

## Consequences

- 対応した場合: Windows固有のデバイス切断復旧ロジックのうち、実際に不具合の温床である
  supervisor層の解釈ミスを機械的に検知できるようになる。
- 対応しない場合: [0010](0010-windows-devicelost-not-reaching-fsm.md)・
  [0011](0011-windows-callback-timeout-orphans-thread.md)のような欠陥が、
  実機を持つユーザーの報告でしか発覚しない。
