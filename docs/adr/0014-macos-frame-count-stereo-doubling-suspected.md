# 0014: sc_stream.rsのframe_count計算がステレオ時に誤っている疑い

- Status: Implemented (macOS実機検証待ち、疑いは実装時の追加読解でほぼ確定) — Decision 1の一環として`FrameForwarder`の呼び出し元(`run()`)を確認した結果、**`FrameForwarder`にはそもそも`channels`フィールドが存在せず、`did_output_sample_buffer`のスコープ内でチャンネル数を参照する手段が構造的に無かった**ことを確認し、疑いはほぼ確定と判断。`FrameForwarder`に`channels: u16`フィールドを追加し、`run()`内2箇所の`FrameForwarder::new`呼び出しで`self.channels`を渡すよう配線した上で、Decision 2の修正(`frame_count = samples.len() / channels`)を実装。無意味だった`sample_rate > 0`分岐も削除。Decision 1後半(実機でのステレオキャプチャ実測)・Decision 3のテストは実機・`capture-macos`のコンパイル可否(swiftc依存でこのサンドボックスでは不可)に阻まれ未実施。Decision 4(`sample_rate > 0`分岐の意図確認)は削除という形で解消。
- 該当箇所: `crates/capture-macos/src/sc_stream.rs:274-278`、
  `crates/app-service/src/macos_frame_collector.rs:67`
  (`device_position_frames / sample_rate`をホスト時刻ナノ秒として使用)
- 発見経緯: [0001](0001-macos-scstream-error-callback-unverified.md)のOpusレビュー
  (2026-08-18)で発見。**Opus自身も「疑い」として報告しており、未検証。**

## Context

`sc_stream.rs:274-278`のframe_count計算:

```rust
let frame_count = if self.sample_rate > 0 {
    (samples.len() as u32).max(1)
} else {
    samples.len() as u32
};
```

`samples`はチャンネルインターリーブ済みのf32サンプルの総数であるため、
本来フレーム数は`samples.len() / channels`であるべきだが、上記の実装は
チャンネル数で割っていない。ステレオ(channels=2)の場合、`frame_count`が
実際のフレーム数の2倍になる可能性がある。

この値は`macos_frame_collector.rs:67`で`device_position_frames / sample_rate`
としてホスト時刻(ナノ秒)の計算に使われているため、もし本当に2倍になって
いるなら、**タイムラインが実時間の2倍速で進む**(録音した音声の時刻情報が
実際の経過時間の半分になる)という重大な不具合になりうる。

また`sample_rate > 0`の分岐は両側の式が実質同じ(`.max(1)`の有無だけ)であり、
cargo-mutantsのミュータントを通すためだけに追加された不自然な分岐に見える
(本質的なロジックの分岐ではない)。

**この項目はOpusによる静的読解のみに基づく「疑い」であり、実機での実測による
検証を経ていない。** 725125fと同じ「未検証」コード群に属するが、health可視化
機能よりも先に音声データそのものの正しさに関わるため優先度は高く見積もる。

## Decision

1. **(最優先)** macOS実機で、ステレオ入力(2ch)のキャプチャを行い、
   `device_position_frames`の増分が実測のサンプル数/フレーム数と一致するか
   検証する。単体テストとしても、`samples.len()`と`channels`を変えた場合の
   `frame_count`計算を明示的にテストする(現状この計算にテストが無い)。
2. 疑いが実証された場合、`frame_count = samples.len() as u32 / channels.max(1)`
   相当に修正し、`device_position_frames`ベースのタイムスタンプ計算全体
   (`macos_frame_collector.rs`)に影響がないか確認する。
3. 疑いが否定された場合(例: `samples`が実際はフレーム単位で既に格納されている
   等)、本ADRにその根拠を追記してStatus: `Rejected`とする。
4. `sample_rate > 0`分岐の意図(mutantsを通すためだけの分岐なのか、実際に
   意味のある分岐なのか)を確認し、不要であれば単純化する。

## Consequences

- 疑いが実証され対応した場合: 録音データのタイムライン精度に関わる
  重大な不具合を出荷前に防げる。
- 検証を怠った場合: もし実際に2倍速の不具合が存在すれば、文字起こし結果と
  実際の発言タイミングがズレる、要約の時刻情報が不正確になる等、
  ユーザーが直接気づく形で問題が顕在化するリスクがある。
