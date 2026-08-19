# 0004: BindingHealth::UnavailableがDeviceUnavailableとProcessNotFoundを区別しない

- Status: Deferred — 今回の実装対象からは見送った。理由: `BindingSelection::Process`が本体コードで完全に未配線(dead code)のまま、3層(`BindingHealth`→`TrackHealth`→`TrackHealthDto`)にわたる配線変更を先行投資するのは費用対効果が低いと判断。`BindingSelection::Process`が実際に配線されるタスクが発生した時点で、このADRのDecisionに沿って一緒に実装するのが妥当。0008(jq移行)は完了済みなので、着手時の前提条件は満たされている。
- Severity: 低 (2026-08-18 Opusレビューで中→低に修正: 未配線機能に対する将来の文言の話で、
  現時点でユーザー影響・リグレッションリスクともにゼロのため)
- 該当箇所: `crates/capture-api/src/rebinding.rs`(`health()`, 152行目付近)、
  `apps/desktop/src/capture_health.rs`(`describe_track`)
- 関連ADR: [0008](0008-poll-capture-health-fragile-grep-matching.md)(本ADRを実装すると
  `TrackHealthDto`のJSON形が変わり、0008のgrep照合が壊れる)

## Context

`BindingHealth::Unavailable`は、`WaitReason::DeviceUnavailable`(デバイス自体が
見つからない)と`WaitReason::ProcessNotFound`(`BindingSelection::Process`で
プロセスをピン留めしている際に対象プロセスが見つからない)の両方を同一の
`Unavailable`に潰している。`apps/desktop/src/capture_health.rs`の
`describe_track`はこれを受けて常に「デバイスが切断されました(再接続を待って
います)」という文言をUIに出す。

`BindingSelection::Process`は本体コード(`spikes/`とテストコードのみで、実際の
呼び出し箇所)では未配線であることを確認済み。将来この機能が配線された時点で
「デバイスは繋がっているのにプロセスが起動していないだけ」の状況で誤った文言が出る。

## Decision

1. `BindingHealth`(または`WaitReason`)にreasonを保持させる。
2. **(2026-08-18追記: 伝播経路の明確化)** reasonをUIまで届けるには
   `BindingHealth`(capture-api)→ `app_service::TrackHealth`
   (`crates/app-service/src/capture_health.rs:22`)→
   `control_protocol::TrackHealthDto`(`crates/control-protocol/src/lib.rs:123`)の
   **3層すべて**を変更する必要がある。当初のDecisionは`BindingHealth`と
   `describe_track`のみを挙げていたが、中間層の`TrackHealth`/`TrackHealthDto`の
   変更が抜けていたため追記する。
3. UI文言を`WaitReason`ごとに分岐させる
   (例: `ProcessNotFound` → 「対象アプリが起動していません」、
   `DeviceUnavailable` → 「デバイスが切断されました」)。
4. `TrackHealthDto`にreasonを持たせると`"remote_health":"Unavailable"`のような
   文字列表現が`"remote_health":{"Unavailable":{...}}`のような構造化表現に変わる
   可能性が高い。[0008](0008-poll-capture-health-fragile-grep-matching.md)の
   grepベース照合が壊れるため、本ADRの実装前に0008を先に(またはあわせて)対応する。
5. `BindingSelection::Process`が実際に配線されるタイミングで、この区別が
   反映されていることをリグレッションテストで確認する。

## Consequences

- 対応した場合: プロセスピン留め機能が配線された際、ユーザーに正しい原因が
  伝わり、無駄な「デバイスの抜き差し」対応をさせずに済む。
- 対応しない場合: 機能配線時に誤誘導する文言がそのまま出荷されるが、
  現時点では未配線のため緊急性は低い。
