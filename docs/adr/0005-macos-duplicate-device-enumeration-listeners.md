# 0005: macOSで設定画面のデバイス監視Dropが同期joinでUIスレッドを止めうる

- Status: Implemented (macOS実機検証待ち) — Decision 2の通り、`DeviceChangeWatcher::drop`の`shutdown_tx.send`/`handle.join()`を検知用スレッドへfire-and-forgetで退避し、`drop`自体が即座に返るよう修正。Decision 1(実機での手動確認)は未実施。`capture-macos`依存のためこのサンドボックスではコンパイル確認不可(レビューのみ)。
- Severity: 低(実害範囲は限定的だが、発生条件は日常的な操作)
- 該当箇所: `crates/app-service/src/macos_session.rs:158`(`DeviceWatch::start(watch_tx)`)、
  `crates/app-service/src/macos_device_watch.rs`(`DeviceChangeWatcher`, `drop`実装 78-85行)

## Context

(2026-08-18 Opusレビューによる大幅修正: 当初のタイトルは「録音中セッションと設定画面の
デバイス監視が二重登録され、CoreAudio問い合わせが倍になる」だったが、2点で事実誤認が
あったため全面的に書き直す。)

1. **帰属の誤り**: リスナーを持つのは`MacosSupervisor`ではなく、
   `macos_session.rs:158`の`DeviceWatch::start(watch_tx)`である。supervisorは
   `watch_rx`を受け取るだけで、CoreAudioリスナーを直接は持たない。
2. **実害説明の誤り**: 「1回のデバイス変化で`enumerate_capture_devices`/
   `enumerate_render_devices`が2系統から呼ばれる」としていたが誤り。
   `DeviceChangeWatcher`(`macos_device_watch.rs:52`)はenumerateせず`generation`
   カウンタを増やすだけで、実際の再列挙は`settings.rs:512-524`の**2秒ポーリング**が
   変化を検出したときにのみ行われる。「問い合わせが倍になる」という実害は成立しない。

代わりに、実際にコードを読んで判明したより現実的な懸念は以下:

`DeviceChangeWatcher::drop`(`macos_device_watch.rs:78-85`)は`shutdown_tx.send(())`の
後に`handle.join()`で**同期ブロック**する。この`Drop`はDioxusの`use_future`が
unmount時にfutureをdropする経路、つまり非同期ランタイムのスレッド上で走りうる。
設定画面を閉じた瞬間にUIスレッドが(リスナースレッドの終了待ちで)一時的に
止まる可能性がある。二重登録より遥かに踏みやすい操作(設定画面を開いて閉じるだけ)
で発生しうる。

## Decision

1. 「設定画面を開いて閉じる」という操作を手動テストチェックリストに追加し、
   実機でUIのフリーズ有無を確認する。
2. `handle.join()`が非同期ランタイムのスレッド上で呼ばれている場合、
   `spawn_blocking`相当の仕組みに退避するか、`join`をブロックしない
   shutdown確認方法(タイムアウト付きjoinやチャネルでの完了通知)に変更する。
3. 実害(実際のフリーズ)が確認できなければ、本ADRはStatus: `Rejected`
   (対応不要と判断してクローズ)とし、その結論を記録する。

## Consequences

- 対応した場合: 設定画面の開閉に起因するUIフリーズの懸念が解消される。
- 対応しない場合: 発生条件が日常的な操作である割に、再現性の低い
  「たまにUIが一瞬固まる」不具合として報告されにくいまま残る可能性がある。
