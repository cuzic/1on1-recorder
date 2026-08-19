# 0006: reconcile_device_list合成挙動のsupervisorレベルテストが未整備

- Status: No action needed — Decision 1・2は事実誤認(既存テストで既にカバー済み/原理的に不可能な要求)と判明したため取り下げ。Decision 3(supervisorのOS非依存リファクタ)は中長期課題として保留。
- Severity: 低(2026-08-18 Opusレビューで中→低に修正。当初の前提事実が誤りだったため)
- 該当箇所: `crates/app-service/src/macos_supervisor.rs`の`reconcile_device_list`
  (removed→addedの順で処理する設計コメントあり)、`crates/capture-api/tests/device_diff.rs`

## Context

(2026-08-18 Opusレビューによる修正: 当初「1回のdiffでデバイス消失+出現が同時発生する
複合ケースの統合テストが見当たらない」としていたが誤り。実際には
`crates/capture-api/tests/device_diff.rs:50`に
`a_different_device_appearing_does_not_mask_another_disappearing`という、
`removed`と`added`が同時に非空になるケースをちょうどカバーするテストが**既に存在する**。
検索時に`scenarios.rs`しか見ておらず、同じ`tests/`ディレクトリの`device_diff.rs`を
見落としていた。)

`diff_and_update`(`device_diff.rs:52-58`)は`BTreeSet`の差集合として実装されており、
同一`EndpointId`が`removed`と`added`の両方に入ることは定義上あり得ない
(両スナップショットに存在すれば差分は空になる)。したがって「同一デバイスの再接続を
removedとaddedの両方として観測する」ケースはそもそも発生しない。

残る真のギャップは、**`macos_supervisor.rs`レベル**で`delta.removed`と`delta.added`を
1ラウンドで`decide()`に流したときの合成挙動(処理順序に依存した副作用がないか)である。
ただし`macos_supervisor.rs`は`#[test]`ゼロであり、かつ`enumerate_device_snapshot`が
CoreAudioを直叩きしているため、テストを書くにはsupervisorをOS非依存に切り出す
リファクタが必要という構造的コストがある。`decide()`自体は観測ごとに独立処理されるため、
順序依存が問題になるのは「同一bindingが同一ラウンドでremovedとaddedの両方に該当する」
場合だけだが、上記の通りそれは発生しない。したがって実際のリスクは低い。

## Decision

1. ~~`DeviceDelta`が`removed`と`added`両方持つケースの統合テストを追加する~~
   → 既に`device_diff.rs:50`でカバー済みのため対応不要。
2. ~~削除された既存デバイスと追加された新デバイスが同一の物理デバイス(再接続)である
   ケースをテストする~~ → (2026-08-18修正)`BTreeSet`差分の定義上、この状況は
   原理的に発生しえないため、このテストは実装不可能かつ不要。
3. 優先度は下げつつ、`macos_supervisor.rs`をOS非依存にリファクタして
   supervisorレベルの合成挙動をテスト可能にすることは、
   [0003](0003-capture-windows-zero-test-coverage.md)の「supervisor層への
   投資再配分」と合わせて中長期的に検討する。

## Consequences

- 対応した場合(supervisorのリファクタを実施した場合): デバイス抜き差しの
  合成パターンでのリグレッションを機械的に検知できる。
- 対応しない場合: リスクは低いと評価しているため、当面は許容する。
