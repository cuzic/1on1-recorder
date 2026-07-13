# リアルタイム文字起こし抽象化 設計書

* **文書ステータス**: Draft v0.4(PoC対象をGemini LiveからDeepgramへ変更。実装着手可)
* **作成日**: 2026-07-13(最終更新: 2026-07-13、PoC対象変更)
* **関連文書**: [design.md](design.md) §13(アップロードAPI境界、`UploadAdapter`と同型の抽象化パターン)・§13.4(将来のリアルタイム文字起こしに関する既定方針)、[docs/logging-policy.md](docs/logging-policy.md)(音声内容・トークンを外部に出す際の既定ルール)
* **位置づけ**: 本書は「複数のSTT(音声認識)エンジンを差し替え可能にする」抽象化そのものの設計書であり、実装前にCodexレビューを経てから着手する。**PoC対象はDeepgram(Nova-3)**。抽象化自体は他プロバイダを見据えて設計する。

> **2026-07-13 変更メモ**: §4〜§5の抽象設計(トレイト形状・extra機構)はGemini向けにCodexレビュー2回を経て確定していたが、その後の実機検証(Gemini Live APIへの実接続スパイク)で、Gemini Liveのnative-audioモデルは常に音声応答を自発生成する会話系APIであり、無応答・聞き専用の文字起こしモードが存在しないことが判明した(公式ドキュメントでも明示的に確認)。これはdrain完了判定などの実装詳細以前の、PoCとして採用できないという根本的な問題だったため、PoC対象をGemini Liveから撤回した。加えて、日本語対応状況を調査した結果、当初最安と見えたAssemblyAIの主力ストリーミングモデルは日本語非対応(英語+欧州5言語のみ)で、Deepgram(Nova-3)は日本語をstreaming/batch双方で追加コストなく正式サポートしていることが分かった。このため、実際にサーバー側から常時ストリーミング応答が得られ、日本語対応もクリーンなDeepgramをPoC対象に変更する。§4(トレイト)・§5(extra機構)は設計変更不要(プロバイダ非依存の抽象化はそのまま使える)。§6のみDeepgram向けに書き換える。

---

## 1. 目的とスコープ

design.md §13.4は将来のリアルタイム文字起こしについて次の方針を既に定めている。

> 将来のリアルタイム文字起こしでは、録音保存用の30秒セグメントとは別に、1秒から5秒程度の短チャンクまたはストリーミング経路を追加する。文字起こし経路の失敗は録音継続に影響させず、保存済みセグメントから後処理で再文字起こしできるようにする。

本書はこの方針を実装可能な形に具体化する。スコープは以下の3点。

1. **STTエンジンを抽象化するトレイト設計**(`crates/stt-api`という新規汎用crate)
2. **プロバイダ固有機能を、名前を統一しつつ利用可能にする拡張(`extra`)機構**の設計
3. Gemini Live APIをこのトレイトの上でどう実装するかの方針(実装そのものは別タスク)

本書のスコープ外: `app-service`への具体的な組み込み方(セッション管理・session-storeスキーマ拡張)、要約(summarization)機能の設計、実装そのもの。これらは本設計がレビューを経て確定した後に着手する。

---

## 2. 調査結果サマリ

実装前に、(a) 既存のRust crateがこの抽象化を解決していないか、(b) 主要STTエンジンの実APIがどういう形をしているか、を調査した。

### 2.1 既存crate

複数プロバイダを1つのストリーミングトレイトで抽象化する、実用に足るRust crateは**存在しない**(2026-07-12にcrates.io/lib.rs/GitHubを調査)。

* `transcribe-rs`(crates.io, v0.3.11, 2026-07-07公開): ローカルモデル(Whisper/ggml、Parakeet、SenseVoice等)向けの`SpeechModel`トレイトと、リモート用`RemoteTranscriptionEngine`トレイトを持つが、後者はOpenAIのみ1実装。バッチ(ファイル全体)前提でストリーミング非対応。
* `llm-kit`(github.com/saribmah/llm-kit、`llm-kit-assemblyai` v0.1.0、23ダウンロード、2026年1月): 汎用`TranscriptionModel`トレイトはあるが、12プロバイダ中Groq/ElevenLabs/AssemblyAIの3つのみ実装、いずれもバッチ/ファイル前提。9スター・作成から日が浅く、実用に耐える成熟度ではない。

→ 自作する。この調査自体は本設計書のためにAgent経由でWebSearch/WebFetchを行った結果であり、上記バージョン番号・日付が変わっていないか実装着手前に再確認すること。

### 2.2 主要プロバイダの実APIの共通点・相違点

| プロバイダ | トランスポート | interim/final の区別方法 | 単語タイムスタンプ/話者分離 | 明示的なspeech-start/end |
|---|---|---|---|---|
| Deepgram | WebSocket | `is_final` + `speech_final`(2種のフラグ) | あり | あり(`SpeechStarted`/`UtteranceEnd`) |
| Google Cloud STT v2 | gRPC専用 | `is_final`フラグ | あり(`speaker_label`) | あり(`SPEECH_ACTIVITY_BEGIN/END`) |
| AssemblyAI | WebSocket | `end_of_turn`フラグ(1種) | あり | 明示イベントなし(無音タイムアウトで代替) |
| Azure AI Speech | SDK限定(生プロトコル非公開) | イベント種別(`recognizing`/`recognized`) | 話者分離は別API(`ConversationTranscriber`) | 明示イベントなし |
| OpenAI Realtime | WebSocket/WebRTC/SIP | イベント種別(`.delta`/`.completed`) | バッチ版のみで確認、Realtimeは未確認 | あり(`speech_started`/`speech_stopped`、サーバVAD時) |
| Gemini Live | WebSocket | 区別なし(`serverContent.inputTranscription.text`が逐次追記) | 未確認(未提供の可能性) | サーバ通知はなし(手動VAD時にクライアントが送る制御メッセージとして`activityStart`/`activityEnd`はある — サーバからの通知ではない点に注意。§6参照) |

この表からの結論:

* **共通の骨格**: 「セッションを開始→音声を送り続ける→イベントを受け取る→終了」という形は全社共通。
* **強制すると歪む部分**: interim/final の判定方法(bool 1個/2個/イベント種別/区別なし)、speech-start/end イベントの有無、単語レベルのメタデータの有無、トランスポート層(WebSocket vs gRPC専用 vs SDK限定)。これらを無理に1つの形へ合成せず、「対応していれば出る、していなければ出ない」という設計にする。

---

## 3. アーキテクチャ全体像

`capture-api`(OS非依存の方針・共通型)+ `capture-windows`(Windows実装)と同じ分離パターンを踏襲する。ただし正確には、`capture-api`は「トレイトのみ」ではなく、`CaptureAdapter`のようなトレイトはまだ定義されておらず、OS非依存の`rebinding` FSM(型・純粋関数)を公開している段階である([capture-api/src/rebinding.rs](crates/capture-api/src/rebinding.rs)参照)。本設計が踏襲したいのは「OS/プロバイダ非依存の方針・共通型を別crateに置き、実I/Oは実装crateへ隔離する」という分離の考え方であり、`capture-api`の現状の実装詳細(トレイト定義の有無)そのものではない。

```
crates/
├─ stt-api/       # トレイト・共通型のみ。特定プロバイダに一切依存しない。汎用crateとして公開可能な設計にする。
└─ stt-deepgram/  # Deepgram(Nova-3)ストリーミングAPIの実装。stt-apiに依存する。
```

将来 `stt-gemini`(バッチ用途に限定すれば有望)や`stt-assemblyai`等を追加しても `stt-api` は変更不要、というのが分離の目的。`stt-api`自体は他のこのプロジェクト固有の型(`SessionId`・`TrackKind`等)に依存させない — 「音声を入れたらテキストが出てくる」という抽象化だけを持ち、「どのセッション/どのtrackの音声か」はこれを呼び出す`app-service`側の責務とする。

---

## 4. コア抽象(`stt-api`)の設計

### 4.1 トレイト

```rust
use async_trait::async_trait;
use tokio::sync::mpsc;

/// STTプロバイダ1つを表す。`Box<dyn SttSession>`を返すことで、このトレイト自体を
/// object-safe に保つ(関連型を使うと`Box<dyn SttProvider>`として実行時に
/// プロバイダを差し替えられなくなる)。Codexレビューで確認済み:
/// `async_trait` + `Box<dyn SttSession>` + `finalize(self: Box<Self>)`は
/// Rustのobject-safeなパターンとして問題なく成立する。
#[async_trait]
pub trait SttProvider: Send + Sync {
    async fn start_session(
        &self,
        config: SttSessionConfig,
    ) -> Result<(Box<dyn SttSession>, mpsc::UnboundedReceiver<SttEvent>), SttError>;
}

#[async_trait]
pub trait SttSession: Send {
    /// mono f32 PCM を1チャンク送る。チャンクサイズは呼び出し側の自由。
    /// 固定フレーミングが必要な実装(Google gRPCの約15-25KB上限等)は
    /// 内部でバッファ/分割する。`chunk.start_sample`は、このチャンクが
    /// セッション開始(`send_audio`の最初の呼び出し)から数えて何サンプル目
    /// から始まるかを表す — 結果イベント(`FinalTranscript`等)の
    /// `audio_start_ms`/`audio_end_ms`をこの位置と対応づけられるようにする
    /// ため(Codexレビュー指摘: タイムライン上の位置情報が無いと、後で
    /// session-storeや録音セグメントとの対応づけができない)。
    async fn send_audio(&mut self, chunk: AudioChunk<'_>) -> Result<(), SttError>;

    /// 音声終了を通知し、最終結果をflushする。プロバイダによって必要な
    /// 手順が異なる(例: Gemini Liveなら`audioStreamEnd`送信→残りの
    /// `inputTranscription`をdrain→close、Deepgram/AssemblyAIなら
    /// `CloseStream`/`Terminate`送信)。「接続を切るだけでよい」プロバイダは
    /// 現時点では確認できていない — 各アダプタは自分のプロバイダの終了
    /// 手順を明示的に実装すること。
    async fn finalize(self: Box<Self>) -> Result<(), SttError>;
}

/// 送信する音声チャンクと、セッション内での絶対位置。
pub struct AudioChunk<'a> {
    pub pcm: &'a [f32],
    /// セッション開始から数えたサンプル位置(`sample_rate_hz`基準)。
    pub start_sample: u64,
}
```

**注意(Codexレビュー指摘)**: `async_trait`はデフォルトで`Send`なfutureを要求する。実装側(特にWebSocketベースのアダプタ)が`.await`をまたいで`!Send`なsink/guardを保持すると、この制約によりコンパイルできない。各アダプタの実装では、必要ならWebSocket書き込みを専用taskに閉じ込め、チャネル経由でやり取りする設計にすること。

### 4.2 設定・イベント型

```rust
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SttSessionConfig {
    pub sample_rate_hz: u32,
    /// `None`は自動検出(対応していれば)。
    pub language: Option<String>,
    /// 非対応プロバイダは常にFinalのみ返す(全てのイベントで`is_final`相当)。
    pub interim_results: bool,
    /// 非対応プロバイダは`Word.speaker`を常に`None`のまま返す。
    pub diarization: bool,
    /// 非対応プロバイダは`SpeechStarted`/`SpeechEnded`を一切出さない。
    pub vad_events: bool,
    pub extra: SttExtraRequest,
}

// SttSessionConfigも#[non_exhaustive]なので、SttExtraRequestと同様に
// `new(sample_rate_hz)` + `with_language`/`with_interim_results`/
// `with_diarization`/`with_vad_events`/`with_extra`といったビルダー
// メソッド一式を提供する(§5.1でこの理由を詳述)。
//
// 注意(再レビュー指摘): `SttSessionConfig::default()`は`sample_rate_hz = 0`
// という無効な値を許してしまう。`new(sample_rate_hz)`の使用を推奨するだけでは
// 不十分で、各`start_session`実装が`sample_rate_hz`(0や、そのプロバイダが
// 対応していないレート)を検証し、無効なら`SttError::PermanentError`で
// 即座に拒否すること。

#[derive(Debug, Clone)]
pub enum SttEvent {
    SpeechStarted,
    SpeechEnded,
    PartialTranscript {
        text: String,
        /// `AudioChunk::start_sample`と対応する、セッション内の絶対位置
        /// (Codexレビュー指摘: 位置情報が無いと録音タイムラインと対応
        /// づけられない)。プロバイダが範囲を報告しない場合は`None`。
        audio_start_ms: Option<u64>,
        audio_end_ms: Option<u64>,
        extra: SttExtraResult,
    },
    FinalTranscript {
        text: String,
        words: Option<Vec<Word>>,
        audio_start_ms: Option<u64>,
        audio_end_ms: Option<u64>,
        extra: SttExtraResult,
    },
    /// `SttError`をそのまま包む(Codexレビュー指摘: 独自の`{message,
    /// recoverable}`にすると`SttError::is_retryable()`と二重管理になる)。
    Error(SttError),
}

#[derive(Debug, Clone)]
pub struct Word {
    pub text: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub confidence: Option<f32>,
    pub speaker: Option<u32>,
}
```

### 4.3 エラー

`recorder_domain::UploadError`と**同型ではない**(Codexレビュー指摘: `UploadError`は`Timeout`/`ServerError`/`RateLimited`/`AuthExpired`/`PermanentClientError`/`Transport`を分け、401だけ`needs_token_refresh_before_retry`という別メソッドで扱う、より細かい分類を持つ)。`SttError`が踏襲するのは「`is_retryable()`のようなメソッドで、呼び出し側にリトライ可否を型で示す」という**方針**であり、バリアント構成そのものではない。

```rust
#[derive(Debug, Clone, thiserror::Error)]
pub enum SttError {
    #[error("connection/transport error: {0}")]
    Transport(String),
    #[error("request timed out")]
    Timeout,
    #[error("rate limited")]
    RateLimited,
    #[error("authentication failed or expired: {0}")]
    AuthenticationFailed(String),
    #[error("provider rejected the request: {0}")]
    PermanentError(String),
    #[error("session already closed")]
    SessionClosed,
}

impl SttError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, SttError::Transport(_) | SttError::Timeout | SttError::RateLimited)
    }
}
```

`AuthenticationFailed`を追加したのは、APIキー方式の認証を使うプロバイダでもトークン失効/再認証が起き得るため。`UploadError::needs_token_refresh_before_retry`のような「リトライ前にトークン更新が必要」という区別が要るかどうかは、実装時にプロバイダごとの認証方式を確認してから決める(未解決点として§7に記載)。

---

## 5. 拡張(`extra`)機構の設計

### 5.1 設計方針

`SttSessionConfig`/`SttEvent`の共通フィールドだけでは表現できない、一部のプロバイダにしかない機能を扱うための仕組み。**フィールド名の統一を`stt-api`側で一元管理する**ことが最重要の要件であり、各プロバイダcrateが勝手にJSONキーを発明することを防ぐ。

```rust
/// 「既知だが全プロバイダ対応ではない」追加機能のカタログ。新しい機能は
/// プロバイダ名を冠したキーではなく、ここに"概念名"で1回だけ追加する。
/// 2社目が同じ機能を実装したら、新しい名前を作らずこのフィールドを再利用する。
/// 各フィールドは`Option`なので、(a)呼び出し側が指定しない、(b)選んだ
/// プロバイダが対応していない、のどちらの場合も単に無視される(エラーにしない)。
///
/// `#[non_exhaustive]`にしているため、他crateからは struct literal で構築
/// できない(`..Default::default()`を使っていても不可 — Codexレビューで
/// 判明: `#[non_exhaustive]`はフィールドの一部指定を許さず、構築手段を
/// `Default::default()`か明示的なコンストラクタ/ビルダーに限定する言語
/// 仕様である)。そのため、`with_*`ビルダーメソッドを提供する。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct SttExtraRequest {
    /// 特定の単語/フレーズの認識精度を上げる(固有名詞・専門用語など)。
    /// 対応済み: Deepgram(`keywords`)、Google(`speech_contexts`)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vocabulary_boost: Option<Vec<String>>,

    /// 文字起こし前にモデルに「考えさせる」予算。
    /// 対応済み: Gemini。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<u32>,

    /// まだ共有フィールド化するほどでもない、本当にプロバイダ固有のものの
    /// 素通し用。ここに逃げる前に、専用フィールド化を検討すること。
    /// (ユーザー確認済み: 素通しでよい — 型安全性より「対応していない
    /// プロバイダは単に無視する」という緩さを優先する)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_specific: Option<serde_json::Value>,
}

impl SttExtraRequest {
    pub fn with_vocabulary_boost(mut self, words: Vec<String>) -> Self {
        self.vocabulary_boost = Some(words);
        self
    }
    pub fn with_reasoning_budget(mut self, budget: u32) -> Self {
        self.reasoning_budget = Some(budget);
        self
    }
    pub fn with_provider_specific(mut self, value: serde_json::Value) -> Self {
        self.provider_specific = Some(value);
        self
    }
    // 新しいフィールドを追加するたびに、対応する`with_*`も追加する。
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct SttExtraResult {
    /// 自動言語検出の結果。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_language: Option<String>,

    /// 発話の感情分類(対応していれば)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sentiment: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_specific: Option<serde_json::Value>,
}

impl SttExtraResult {
    pub fn with_detected_language(mut self, language: String) -> Self {
        self.detected_language = Some(language);
        self
    }
    pub fn with_sentiment(mut self, sentiment: String) -> Self {
        self.sentiment = Some(sentiment);
        self
    }
    pub fn with_provider_specific(mut self, value: serde_json::Value) -> Self {
        self.provider_specific = Some(value);
        self
    }
}
// 訂正(2026-07-13, 再レビュー): 当初「SttExtraResultは各アダプタ内部でのみ
// 構築するのでビルダー不要」と書いていたが誤り。`stt-gemini`は`stt-api`とは
// 別crateなので、`#[non_exhaustive]`な`SttExtraResult`をプロバイダの
// レスポンスから組み立てる際もstruct literalは使えず、同じく`with_*`が必要。
```

### 5.2 なぜ`Option<serde_json::Value>`の生JSONバッグではなく型付き構造体にするか

- **命名の一貫性**: 生JSONバッグ(例: `extra: Option<serde_json::Value>`を各プロバイダが自由に解釈する形)だと、命名規則を強制する手段がない。型付き構造体なら、フィールドを追加する行為そのものが「この名前で統一する」というレビュー可能な変更になる。
- **それでも「extra json」の感覚とは矛盾しない**: 構造体は`Serialize`/`Deserialize`を持つので、必要なら`serde_json::to_value`/`from_value`でJSONとして相互変換できる。実体がJSONかどうかは呼び出し側からは意識しなくてよい。
- **`#[non_exhaustive]` + ビルダーメソッド**: 将来フィールドが増えても、他crateは`Default::default()`と`with_*`メソッドの組み合わせでしか構築できない(struct literalでの直接構築は`#[non_exhaustive]`により禁止される — この点は上のコード直前の注記、および§5.1で訂正済み)ので、新しいフィールドをこの構造体に追加しても既存の呼び出しコードは壊れない。

### 5.3 想定される使い方

```rust
// Geminiを名指しで使う場合。SttSessionConfigも#[non_exhaustive]なので、
// 同様にビルダーメソッド(with_sample_rate_hz等)経由で構築する。
let config = SttSessionConfig::new(16_000)
    .with_interim_results(true)
    .with_vad_events(true)
    .with_extra(SttExtraRequest::default().with_reasoning_budget(0));
```

---

## 6. Deepgramアダプタ(`stt-deepgram`)実装方針

Deepgram Streaming API(WebSocket)の実プロトコルは以下の通り(2026-07-13にhttps://developers.deepgram.com/reference/speech-to-text/listen-streaming、https://developers.deepgram.com/docs/close-stream、https://developers.deepgram.com/docs/understanding-end-of-speech-detection、https://developers.deepgram.com/reference/authentication を実際に確認して裏取り済み)。

* 接続: `wss://api.deepgram.com/v1/listen` に対しクエリパラメータで設定を渡す(Gemini Liveのような別途JSON setupメッセージは不要):
  `model=nova-3`、`language=ja`(日本語固定。将来`SttSessionConfig.language`から渡す)、`encoding=linear16`、`sample_rate=<SttSessionConfig.sample_rate_hz>`、`channels=1`、`interim_results=true|false`(`SttSessionConfig.interim_results`から)、`punctuate=true`、`vad_events=true|false`(`SttSessionConfig.vad_events`から)、`endpointing=<ms>`(発話終端検出の無音長。既定10msは短すぎるため実装時にチューニング)、`utterance_end_ms=1000`以上(`UtteranceEnd`メッセージを有効化する場合)。
* 認証: HTTPヘッダ`Authorization: Token <DEEPGRAM_API_KEY>`(GeminiのようなURLクエリへの生キー埋め込みではない — 資格情報の扱いは[docs/logging-policy.md](docs/logging-policy.md)の既定方針(ログへ出さない)にも合致しやすい)。
* 音声送信: JSON setupメッセージなしで、接続直後からバイナリWebSocketフレームとしてPCM16 little-endianの生バイト列をそのまま送るだけ(Gemini Liveのような`{"realtimeInput":{"audio":{"data": base64...}}}`のようなJSONラップ・base64化は不要)。
* 受信: `type: "Results"`のJSONメッセージ。形状は概ね次の通り:
  ```json
  {
    "type": "Results",
    "channel": { "alternatives": [{ "transcript": "...", "words": [...] }] },
    "is_final": true,
    "speech_final": true,
    "start": 5.99,
    "duration": 1.98
  }
  ```
  `is_final`(このメッセージの音声区間についてこれ以上変わらない)と`speech_final`(発話がこの時点で自然に終わったとDeepgramが判断した)の**2種のフラグが独立にある**点がGemini Liveとの大きな違いであり、§2.2の表の通り「順序保証がなく単一テキストが逐次追記される」Geminiより判定が単純。
* `vad_events=true`時は`{"type":"SpeechStarted", ...}`(発話区間の開始を検知)、`utterance_end_ms`設定時は`{"type":"UtteranceEnd", "channel": [0,1], "last_word_end": 3.1}`(無音が閾値を超えて発話区切りと判定)がそれぞれサーバから送られる — こちらはGemini Liveの`activityStart`/`activityEnd`(クライアント→サーバの制御メッセージ)とは異なり、**サーバ→クライアントの通知イベント**である点に注意(方向性の取り違えはGemini設計時にCodexへ指摘された誤りなので、Deepgramでも実装時に確認すること)。
* 終了: `{"type": "CloseStream"}`をJSON textフレームで送信すると、サーバは残りの音声を処理してから`{"type": "Metadata", "request_id": ..., "duration": ..., ...}`を送り、その後WebSocketを閉じる。**drain完了の判定はこの`Metadata`メッセージの受信そのもの**であり、Geminiのようなタイムアウト方式による推測が不要(§7.3で条件としていたスパイク検証が、Deepgramでは設計時点で不要になった理由)。
* エラー: WSクローズコード、または`{"type": "Error", ...}`系のインバンドメッセージ(詳細は実装時にドキュメント再確認)。HTTPレベルの401/429は接続確立前に発生しうるため、`start_session`内のWebSocketハンドシェイク失敗時にステータスコードから`SttError::AuthenticationFailed`/`SttError::RateLimited`へマッピングする。

これを`SttSession`にマッピングする方針:

* `send_audio`: PCM f32 → PCM16への変換をここで行い、JSONラップなしでバイナリフレームとしてそのまま送信する(サンプルレート変換は呼び出し側`app-service`の責務とし、`stt-deepgram`は`SttSessionConfig.sample_rate_hz`で指定されたレートのf32を受け取ってそのままDeepgramへの`sample_rate`クエリに渡す — Geminiと違い16kHz固定を要求しない)。
* interim/finalの境界判定: `is_final: false`の`Results`は`PartialTranscript`、`is_final: true`の`Results`は`FinalTranscript`として発火する。`speech_final: true`は「発話の自然な区切り」を表すのみで、`is_final`と独立してよい(`speech_final`単体を追加のセマンティクスとして`SttExtraResult.provider_specific`に載せるかは実装時に判断)。
* 単語タイムスタンプ・話者分離: `channel.alternatives[0].words[]`に単語ごとの`start`/`end`/`confidence`があり、`diarization: true`を渡していれば`speaker`も含まれる(Deepgramの`diarize`クエリパラメータに対応)。`Word`型へそのままマッピングできる。
* `SpeechStarted`/`SpeechEnded`: `vad_events: true`かつ`utterance_end_ms`設定時、`SpeechStarted`メッセージ受信で`SttEvent::SpeechStarted`、`UtteranceEnd`メッセージ受信で`SttEvent::SpeechEnded`を発火する(Geminiと異なり、Deepgramアダプタでは`vad_events: true`が実際に効く)。
* `finalize()`: `{"type": "CloseStream"}`をテキストフレームで送信し、`{"type": "Metadata", ...}`受信をもって完了とみなしてからWebSocketをcloseする。判定条件が明確なため、Geminiで必要だったタイムアウト方式のフォールバックは不要。
* エラー: WSクローズコード / インバンドの`Error`メッセージを`SttError`へマッピング(429相当は`RateLimited`、401/403相当は`AuthenticationFailed`、それ以外の接続断は`Transport`)。

---

## 7. 未解決点・将来課題

* `SttExtraRequest`/`SttExtraResult`が今後どれだけ肥大化するか未知数。フィールドが数十を超えたら、カテゴリ別に分割する(例: `SttExtraRequest.vocabulary: VocabularyExtra`のようにネストする)再設計が必要になる可能性がある。
* `stt-api`を実際に外部公開可能な汎用crateにするかどうかは、Gemini実装が動いてから改めて判断する(`audio-timeline`/`capture-api`と同様の判断プロセス)。
* Azure(SDK限定・生プロトコル非公開)とGoogle(gRPC専用)を将来追加する場合、`stt-gemini`/将来の`stt-deepgram`のようなWebSocketベースの実装とは大きく異なるトランスポート層が必要になる — この設計書のトレイト自体はそれを許容する形になっているはずだが、実装時に改めて確認する。
* `app-service`への組み込み方(録音本経路と完全に独立させる具体的な配線、失敗時の扱い、session-storeへの永続化スキーマ)は本書のスコープ外— 本書のレビュー完了後に別途設計する。ただし、以下2点は**次の設計フェーズで詰まらないよう、原則だけここで決めておく**(Codexレビュー指摘)。

### 7.1 イベントのfan-out所有者(原則のみ)

`app-service::windows_supervisor`の`FrameSinkEvent`は「単一consumerが受け取り、必要な副作用へforwardする」設計で、競合consumerを避けている([windows_supervisor.rs](crates/app-service/src/windows_supervisor.rs)参照)。`stt-api`の`mpsc::UnboundedReceiver<SttEvent>`も同じ原則に従う: **`stt-api`自身は常に単一のconsumerだけを想定する**。UI表示・session-storeへの永続化・summarization入力など複数の用途に文字起こし結果を配りたい場合は、その1つのconsumer(`app-service`側)が受け取った後、自分の責務でfan-outする(複数の`mpsc::Receiver`を同じチャネルにcloneして競合させることはしない)。次フェーズの設計では、この単一consumerが具体的にどこに置かれるか(セッションごとのtask、`app-service`内の新モジュール等)を決める。

`mpsc::UnboundedReceiver`は長時間録音でメモリ上限が無い点も指摘されている。次フェーズでは、境界付き(`bounded`)チャネル + バックプレッシャー方針、または一定間隔でのドレイン処理を検討する。

### 7.2 タイムライン対応づけ(原則のみ)

本書の§4改訂で`AudioChunk::start_sample`と`SttEvent`の`audio_start_ms`/`audio_end_ms`を追加したのは、後続で文字起こし結果を録音セグメント(`AudioSegment`の`timeline_start_ms`)や要約と対応づけるために最低限必要な情報を今のうちに用意しておくため。実際に session-store のどのテーブル/スキーマに保存するかは次フェーズで設計する。

### 7.3 実装着手の条件(2026-07-13時点)

トレイト形状・`extra`機構・crate分割は、Gemini向けにCodexレビュー2回を経て確定済みでプロバイダ非依存のため変更不要。PoC対象をDeepgramに変更したことで、§6のGemini実装方針で残っていた懸案(interim/finalの境界判定、drain完了判定)はいずれもDeepgramのドキュメント調査だけで解決している(`is_final`/`speech_final`の独立フラグ、`CloseStream`→`Metadata`という明確な完了通知)。そのため、Geminiのときのような「実装着手前の実機スパイクが必須」という条件は外れる — ただし、ドキュメントと実際の挙動が一致するかは通常のリスクとして、実装後の早い段階で一度実際にDeepgramへ接続して疎通確認は行うこと。**実装着手可。**
