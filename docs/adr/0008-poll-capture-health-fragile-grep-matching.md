# 0008: poll-capture-health.shのgrepベースJSON照合が脆く、かつctl失敗時に無言で死ぬ

- Status: Implemented — Decision 1(jqベースの照合への置き換え)・Decision 2(ctl非ゼロ終了時の明示的エラーメッセージ)を実装。モックの`ctl`スクリプトで4パターン(成功/タイムアウト/ctl失敗/構造化JSON値)を手動実行し全て期待通り動作することを確認済み(このLinux環境で完全にローカル検証できた数少ない項目)。Decision 3(0004実装時にjq移行を先行させる)は前提条件として満たされた。
- Severity: 低
- 該当箇所: `scripts/ci/poll-capture-health.sh`
- 関連ADR: [0004](0004-binding-health-unavailable-reason-not-distinguished.md)
  (0004を実装すると本ADRのgrep照合が確実に壊れるため、実装順序を要調整)

## Context

`poll-capture-health.sh`はStatusDtoのJSON出力を`grep -q "\"${FIELD}\":${EXPECTED}"`
という正規表現ベースの部分一致で照合している。`FIELD`が`self_health`/`remote_health`
に固定されている現状では実害はない(`serde_json::to_string`のcompact出力のため
空白差異による誤検知も無いことは確認済み)。

しかし将来フィールドを増やす際、例えば`"health":"Ok"`のような別フィールドが
`"remote_health":"Ok"`の部分文字列にたまたま一致するといった脆さを持つ。特に
[0004](0004-binding-health-unavailable-reason-not-distinguished.md)を実装すると
`"remote_health":"Unavailable"`のような文字列表現が
`"remote_health":{"Unavailable":{...}}`のような構造化表現に変わる可能性が高く、
このgrep照合が確実に壊れる。

(2026-08-18 Opusレビューによる追記) さらにこのスクリプトは`set -euo pipefail`下で
`last_status="$("$CTL" --json status)"`を実行している。切断中に`desktop`プロセス自体が
落ちた場合など、`ctl`が非ゼロ終了すると**ポーリングループごと即座に異常終了**する。
これはタイムアウトによる分かりやすい`FAIL:`メッセージではなく、無言の異常終了になり、
CIログ上で原因が分かりにくい。

## Decision

1. `jq`がCI環境(macOS runner)で利用可能であれば、`jq`によるフィールド抽出に
   置き換える(`jq -e '.remote_health == "Ok"'`等)。これにより0004実装時の
   構造化表現への変更にも耐えられるようにする。
2. `ctl`呼び出し部分は`|| true`等で捕捉し、非ゼロ終了時に「ctl自体が失敗した」ことを
   明示するメッセージ(`FAIL: ctl exited non-zero`等)を出してから終了するようにする。
3. [0004](0004-binding-health-unavailable-reason-not-distinguished.md)を実装する際は、
   本ADRのjq移行を先に(またはあわせて)行う。

## Consequences

- 対応した場合: フィールド追加時の誤マッチ、およびctl異常終了時の原因不明な
  CI失敗を防げる。
- 対応しない場合: 現状のフィールド構成である限り実害はないが、0004実装や
  ctlクラッシュ時にCIが分かりにくい形で壊れるリスクが残る。
