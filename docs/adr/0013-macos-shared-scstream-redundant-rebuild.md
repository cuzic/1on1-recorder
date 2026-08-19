# 0013: macOSで共有SCStream死亡時に余計なストリーム再構築が1往復挟まる

- Status: Implemented (macOS実機検証待ち) — Decision 1を、当初想定していた「イベントのデバウンス/バッチ化」ではなく、より単純で安全な形で実装: `active_stream.bindings`のうち`self.workers`にまだ残っているbinding(=StreamErrorがまだ届いていないsibling)がある間は`reconcile_active_stream()`の呼び出しを見送り、全siblingの`StreamError`が届いてから1回だけ呼ぶよう`StreamError`ハンドラを変更(チャンネルの`try_recv`で他イベントを取りこぼすリスクを避けるため、チャンネルには触れない設計にした)。Decision 2(`execute()`内の重複呼び出し解消)は実装時の詳細読解の結果、字面上の重複は存在しない(`WorkerFailed`の効果は`ScheduleRetry`のみで`binding_set_changed`を立てない)ことを確認したため対応不要と判断。Decision 3(TCCエラー経路との組み合わせテスト)は実機が必要なため見送り。`capture-macos`/`macos-supervisor`依存のためこのサンドボックスではコンパイル確認不可(レビューのみ)。
- Severity: 中(通常は無害な無駄だが、[0001](0001-macos-scstream-error-callback-unverified.md)
  が懸念するTCCエラー時の失敗経路と絡む可能性がある)
- 該当箇所: `crates/capture-macos/src/lib.rs:169-175`(共有ストリーム`Err`時に
  bindingごとに`StreamError`を送信)、`crates/app-service/src/macos_supervisor.rs:550-563`
  (`reconcile_active_stream`が2発のイベントに対して2回反応する)
- 発見経緯: [0001](0001-macos-scstream-error-callback-unverified.md)のOpusレビュー
  (2026-08-18)で発見。

## Context

macOSはMicrophoneとEndpointLoopbackの両方を1本の共有`SCStream`でキャプチャする
設計になっている(この設計自体は`macos_supervisor.rs:12-40`のdoc commentに
明記された意図的なトレードオフで、妥当性はここでは問わない)。

共有ストリームが死亡すると`lib.rs:169-175`はbindingごとに`StreamError`を送るため、
**2発のイベントが飛ぶ**。`macos_supervisor.rs:550-563`は1発目でMicrophoneの
workerを削除し、`execute` → `reconcile_active_stream`が
「desired = {EndpointLoopback}」として**新しいSCStreamを起動**する。
直後に2発目が届いてEndpointLoopbackのworkerも削除され、**起動したばかりの
ストリームが即座に破棄される**。

通常は「無駄な起動/破棄が1往復挟まる」程度の実害だが、
`start_capture`がTCCエラーで失敗する状況
([0001](0001-macos-scstream-error-callback-unverified.md)がまさに懸念している
シナリオ)では、この余分な1回の起動試行も`classify_stream_start_error`を
通って別の失敗経路に入ってしまう。「`did_stop_with_error`の下流の挙動を
心配するなら、見るべきはコールバック自体よりもこの再構築ロジック」という
のがOpusレビューの指摘。

なお同じ`execute()`内で、562行目付近で`reconcile_active_stream`が
1発目の処理内で既に呼ばれたにもかかわらず**もう一度**呼ばれている冗長も
確認されている。

## Decision

1. 共有ストリームの死亡通知(2発)を1回のreconcileにまとめる。具体的には、
   同一の死亡イベントに起因する複数binding分の`StreamError`を、
   デバウンス(短時間窓でまとめる)またはイベントのバッチ化で1回の
   `reconcile_active_stream`呼び出しに集約する。
2. `execute()`内の`reconcile_active_stream`の重複呼び出しを解消する。
3. TCCエラー等で`start_capture`が失敗する経路と、この再構築ロジックが
   絡んだ場合の挙動(失敗が握りつぶされないか、無限に再試行しないか)を
   確認するテストを追加する。

## Consequences

- 対応した場合: 共有ストリーム死亡時の無駄な起動/破棄サイクルがなくなり、
  TCCエラー時の失敗経路がより単純で予測可能になる。
- 対応しない場合: 通常時は実害軽微だが、TCCエラー等の異常系と組み合わさった
  場合に予期しない失敗パターンを生む可能性が残る。
