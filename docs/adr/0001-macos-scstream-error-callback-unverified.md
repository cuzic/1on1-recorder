# 0001: macOS SCStreamのdid_stop_with_errorパスが実機E2Eでも未検証

- Status: Implemented (macOS実機/CI検証待ち) — Decision 1(`did_stop_with_error`の単体テスト)を`sc_stream.rs`に追加。`capture-macos`クレートはこのLinuxサンドボックスでは`screencapturekit`の依存先`apple-cf`のビルドスクリプトが`swiftc`を要求するため`cargo check`すら通せず、コンパイル自体を確認できていない点に注意(型定義を読んだ上での実装であり、確度は高いが未検証)。Decision 2(tccutil実機E2E)は見送り(実現性が低いとの当初判断通り)。Decision 3・4は未着手。
- Severity: 高
- 該当箇所: `crates/capture-macos/src/sc_stream.rs`(`StreamErrorDelegate`, コミット725125f)
- 関連E2E: `.github/workflows/macos-build.yml`(41852e0で追加), `scripts/ci/poll-capture-health.sh`
- 関連ADR: [0002](0002-macos-e2e-best-effort-job-silently-non-blocking.md)(このパスをE2Eで守っても
  ジョブがcontinue-on-errorである限りCI上は何も保証されない)

## Context

725125fでScreenCaptureKitの`SCStreamDelegate`のエラー/停止コールバック
(`did_stop_with_error`)を実装した。コミットメッセージ自体に「(未検証)」と
明記されている。

7コミット後の41852e0で「macOSデバイス切断/復旧をGitHub Actions実機で検証」する
E2Eを追加したが、そのE2Eが実際にトリガーしているのはBlackHoleのuninstall/reinstall
による`kAudioHardwarePropertyDevices`変化、すなわち`DeviceListChanged`経路のみ。
`did_stop_with_error`が呼ばれるのは以下のような、CoreAudioのデバイスリスト変化とは
別の経路であり、このE2Eでは一度も踏まれていない:

- 画面収録/マイクの権限がOS側で取り消される(TCC)
- 共有中のウィンドウ/ディスプレイが閉じる
- SCStream自体がOS内部エラーで停止する

つまり「未検証」のラベルが付いた機能が、実機E2E追加後も実質的に未検証のままである。

(2026-08-18 Opusレビューによる修正: 当初「パニック、無限リトライ、health未更新のまま
無音継続」といったリスクを列挙していたが、実装(`sc_stream.rs:222-227`)を確認すると
`StreamErrorDelegate::did_stop_with_error`は`Mutex`に文字列を格納して`signal()`する
だけの4行であり、パニック経路は事実上ない。`run()`側も`sc_stream.rs:183-192`のコメントで
「`Err`のみが`StreamError`→`WorkerFailed`に届き、`Ok`はinformationalとして捨てられる」
設計意図が明示されており、むしろこの機能群の中でも比較的丁寧に設計されている箇所である。
リスクの実体は「ロジックの粗さ」ではなく「その設計意図通りに動くことが一度も実機で
確認されていない」という検証の空白そのものである。)

## Decision

1. **(優先)** `SCStreamDelegateTrait`は通常のRustトレイト実装なので、macOS実機なしでも
   今日書ける単体テストを`crates/capture-macos`に追加する:
   `did_stop_with_error`を直接呼び出し、`Mutex`への格納と`signal()`が意図通り行われるか、
   および`run()`が`Err`を返す分岐(`StreamError`→`WorkerFailed`に正しく届く経路)を検証する。
   これはtccutil案より即座に着手でき、実機非依存で恒久的にCIが守れる。
2. 実機での権限取り消し検証は補助的な位置づけとする。`tccutil reset ScreenCapture`は
   TCC変更がプロセス再起動まで反映されないケースがあり、CI再現性は期待薄。可能なら
   `macos-build.yml`のE2Eに追加を試みるが、確実性の低い調査タスクとして扱う。
3. `did_stop_with_error`受信時にどのbinding health状態に遷移するかを明文化し、
   `crates/capture-api`側のFSMテストに対応するシナリオがあるか確認する。無ければ追加する。
4. [0002](0002-macos-e2e-best-effort-job-silently-non-blocking.md)が未解決の間は、
   実機E2Eへの投資よりも項目1の単体テストを優先する — ジョブが`continue-on-error`である
   限り、E2Eだけを厚くしてもCI上の保証は増えない。

## Consequences

- 対応した場合: SCStreamの異常系(権限取り消し・自発停止)の設計意図が単体テストで
  機械的に守られ、リグレッションが実機を待たずに検出できるようになる。
- 対応しない場合: 本番で権限取り消し等が発生した際の挙動が未知数のまま残る。
