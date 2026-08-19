# 0002: macOS E2Eジョブがcontinue-on-errorで退行を検知してもCIが緑のまま

- Status: Implemented — Decision 1(失敗時のGitHub Issue自動起票、重複防止付き)・Decision 2(poll-capture-health.sh側でctl失敗/health未到達を別メッセージに分離、0008で対応)を実装。Decision 3(必須チェック化)は方針通り見送り。YAML構文は`python3 -c "import yaml"`で検証済み、実行はCI待ち。
- Severity: 中 (2026-08-18 Opusレビューで高→中に修正)
- 該当箇所: `.github/workflows/macos-build.yml`(`e2e-best-effort`ジョブ、`continue-on-error: true`)
- 関連: `scripts/ci/poll-capture-health.sh`
- 関連ADR: [0001](0001-macos-scstream-error-callback-unverified.md)、
  [0008](0008-poll-capture-health-fragile-grep-matching.md)

## Context

`e2e-best-effort`ジョブ(`macos-build.yml`)は`continue-on-error: true`指定になっている。

(2026-08-18 Opusレビューによる修正: 当初「41852e0で追加したE2Eがcontinue-on-error
指定になっている」としていたが誤り。このジョブと`continue-on-error`はcapture-macos
導入時から既に存在するインフラであり、41852e0が追加したのはジョブ内のデバイス切断/復旧
検証ステップ群のみ。「今回の機能で非ブロッキングなE2Eを新設してしまった」という書き方は
不正確だった。)

このため:

- `reconcile_device_list`やBindingHealth遷移に将来リグレッションが入り、
  `remote_health`がUnavailable→Okへ遷移しなくなっても、ジョブ全体は失敗として
  GitHub上に表示されない(少なくとも必須チェックとしてPRをブロックしない)。
- `poll-capture-health.sh`のタイムアウト(60秒/90秒)超過や、BlackHoleの
  uninstall/reinstallタイミング依存のflakeが起きても、誰かが実際にジョブの
  ログを開かない限り気づかれない。

また`macos-build.yml:103-111`には、CI環境でTCC(画面収録/マイク)権限を安定に
付与する方法が存在しない旨が既に文書化されている。ジョブ全体がTCC依存の
smoke testである以上、デバイス切断部分だけを切り出してブロッキング化することも
構造的にできない。これは見落とされていた既知の意図的判断であり、
「退行の可視化がない」という運用課題として捉え直す。

## Decision

1. `e2e-best-effort`ジョブの結果(pass/fail/timeout)を`GITHUB_STEP_SUMMARY`だけでなく、
   失敗時に判別可能な形でSlack通知またはIssue自動作成する仕組みを追加する
   (例: `if: failure()`ステップで`gh issue create`、または既存の通知チャネルがあれば利用)。
2. 通知を実装する際、`poll-capture-health.sh`が「TCC seedingが効かず録音自体が
   始まらなかった」場合と「health状態が遷移しなかった」場合を区別できるようにする
   (現状はどちらも`exit 1`で同じ`FAIL:`メッセージになる)。前者を区別しないまま
   Issue自動作成を入れると、TCC起因のflakeでノイズを量産する。
3. ~~`continue-on-error: true`を外せるだけの安定度に達したら必須チェック化を検討する~~
   → (2026-08-18修正)原因がflakeではなくTCCの構造的制約である以上、この達成条件は
   実質存在しない。必須チェック化は目標から外し、代わりに「能動的な可視化」のみを
   ゴールとする。

## Consequences

- 対応した場合: `remote_health`の遷移が壊れた場合に、CIの見た目に頼らず能動的に気づける。
- 対応しない場合: このE2Eは「テストしているつもり」のまま形骸化し、実際には
  誰も見ないログの中でしか失敗が記録されない状態が続く。
