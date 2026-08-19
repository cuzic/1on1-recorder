# 0007: session_idランダム値バグの修正がリファクタコミットに暗黙に同梱

- Status: No code change needed — 実バグは既に5b3e7b0で修正済み。影響範囲(`TranscriptBuffer`のみ、永続化経路には非影響)を確認済みで、追加のコード変更は不要と判断。
- Severity: 低(2026-08-18 Opusレビューで中→低に修正: 影響範囲がUI表示専用バッファに
  限定され、永続化経路には及ばないため)
- 該当箇所: `apps/desktop/src/ui_consumer.rs`(旧`data_to_transcript_segment`)、
  コミット5b3e7b0「refactor(local-broker,transcript-event): consumer間で
  重複していた制御レーン購読とinterim/finalマージを共通化」

## Context

5b3e7b0はコミットメッセージ上「挙動は変更せず、重複コードを共通化するリファクタ」
として提出されている。しかし実際のdiffを見ると、リファクタ前は
`TranscriptSegment.session_id`に毎回`SessionId::new()`(ランダム新規ID)が
入ってしまう実バグが存在しており、リファクタ後は正しい`session_id`を
パラメータとして渡すよう修正されている。「挙動は変更せず」というコミット
メッセージと実際の差分が矛盾している。

(2026-08-18 Opusレビューによる追記: この`TranscriptSegment`は`ui_consumer`の
`TranscriptBuffer`(UI表示専用のメモリバッファ)にしか入らず、`SessionStore`への
永続化経路には流れないことを確認した。したがって実害はUI表示上の挙動に限定され、
おそらくゼロに近い。残るのは「リファクタと称してバグ修正が混入した」という
コミット衛生上の問題であり、緊急の技術的リスクではない。)

## Decision

1. `TranscriptSegment.session_id`の利用箇所(`ui_consumer`の`TranscriptBuffer`)を
   確認し、この変更による実際の表示上の差異(セッションをまたいだセグメントが
   誤って同一session_id扱いされていたのが直った、等)を1行で記録する。
2. コミット履歴を書き換えず、本ADRに事実関係を残すことで意図を明文化したものとする
   (追加の作業は不要)。
3. 今後同種の「リファクタと称してバグ修正/バグ混入が紛れる」パターンをレビューで
   見つけやすくするため、「挙動不変」を謳うコミットは差分の意味的な等価性を
   レビュー時に一言確認する運用を心がける。

## Consequences

- 対応した場合: この巻き込み修正が意図した挙動であることが記録に残る。
- 対応しない場合: 実害はほぼ無いため緊急性は低いが、同種のパターンが
  今後もレビューをすり抜ける可能性は残る。
