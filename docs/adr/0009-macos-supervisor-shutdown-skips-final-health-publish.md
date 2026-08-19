# 0009: シャットダウン時に最終health状態がpublishされない

- Status: Rejected(対応不要と確認済み。2026-08-18 Opusレビューにより起票前に
  結論が出ていたことが判明したためクローズ)
- Severity: — (実害なしと確定)
- 該当箇所: `crates/app-service/src/macos_supervisor.rs`(`run_until_shutdown`、
  shutdown分岐 340-346行が347行の`publish_health()`を経由せず`return`する)

## Context

`MacosSupervisor::run_until_shutdown`はshutdown分岐で`drain_pending_joins`後に
即座に`return Ok(())`するため、その直前の`publish_health()`を経由しない。

(2026-08-18 Opusレビューにより判明) `apps/desktop/src/status.rs:76-79`は
`ActiveRecording`が無いとき`capture_health`を`CaptureHealth::default()`
(全`Ok`)で返す実装になっている。つまり録音停止と同時にUI側のバナー表示は
`publish_health()`を経由せずとも即座にクリアされることが、既存コードを読むだけで
確定している。当初のDecisionは「UI側の挙動を確認する」としていたが、これは
起票する前に(コードリーディングだけで)答えが出ていた話であり、ADRとして
リスクを起票する必要はなかった。

## Decision

対応不要。本ADRはStatus: `Rejected`としてクローズする。将来
`status.rs:76-79`のデフォルト実装が変更され、shutdown後もhealth状態が
持ち越される設計に変わった場合にのみ、この論点を再検討する。

## Consequences

- 本件によるユーザー影響は無い。
- 教訓として、pre-mortem分析でリスクを起票する前に「既存コードを読めば
  即答できないか」を確認するステップを挟むべきだった。同種の“調査不足のまま
  起票してしまう”ケースを避けるため、以後の分析では「実害の有無」を先に
  コードで確認してから起票する運用とする。
