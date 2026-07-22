# サマリー生成と文字起こしの疎結合化 設計書

* **文書ステータス**: Draft v0.2（レビュー指摘反映済み）
* **作成日**: 2026-07-20
* **関連文書**: [design.md](../design.md), [stt-transcription-architecture.md](../stt-transcription-architecture.md)
* **位置づけ**: サマリー生成と文字起こしをイベント駆動で疎結合にし、後段 consumer（要約、検索、Drive/Gemini 連携など）を独立進化可能にする設計書。コードレビュー済み。

---

## 1. 現状分析

### 1.1 現在の結合状況

`summarize` クレートと `stt-api` クレートは**既に完全に疎結合**である。両者は互いに依存しておらず、`summarize::TranscriptTurn` はクレートローカルな型として定義されている。

**唯一の結合点**は `apps/desktop/src/ui.rs:678-686` の `on_generate_summary` ハンドラである：

```rust
// ui.rs の要約生成ハンドラ（現在）
let segments = state.store.list_transcript_segments(session_id)?;
let turns = transcript::to_turns(&segments);
// ... summarizer.summarize(&turns, &options) ...
```

このハンドラは以下の責務を一手に引き受けている：

1. `SessionStore` から `TranscriptSegment` を読み出す
2. `transcript::to_turns()` で `TranscriptTurn` に変換する
3. `CredentialStore` からプロバイダ/モデル選択を読み出す
4. `Summarizer` を構築する（genai / CLI / Vertex / Ollama の4経路）
5. 要約を実行し、結果を `SessionStore` に永続化する

### 1.2 現状の課題

| 課題 | 説明 |
|------|------|
| **UI プロセスへの密結合** | 要約生成はユーザーが「要約を生成」ボタンを押した時のみ実行される。録音終了時の自動要約や、録音中の逐次要約更新ができない |
| **consumer 追加の障害** | 新しい後段処理（Drive 保存、Gemini 連携、RAG インデックス作成など）を追加するたびに `ui.rs` のハンドラが肥大化する |
| **テスト困難** | 要約生成ロジックが UI ハンドラに埋め込まれており、UI 非依存の単体テストが書けない |
| **プロセス分離不可** | 文字起こしと要約が同一プロセス内で動作するため、要約の長時間実行が UI の応答性に影響する |

---

## 2. 目標

### 2.1 機能ゴール

1. 文字起こし結果を**イベントストリーム**として外部化する
2. 要約生成を**独立した consumer** として分離する
3. 録音終了時の**自動要約生成**を可能にする
4. 新しい consumer（Drive 保存、外部連携など）を**コード変更なしで追加**できるようにする

### 2.2 非機能ゴール

- 文字起こしのレイテンシに影響を与えない
- 要約 consumer の障害が文字起こしの継続に影響しない
- 既存の `summarize` クレートと `stt-api` クレートの公開 API は変更しない

---

## 3. アーキテクチャ

### 3.1 全体像

```
┌──────────────────────────────────────────────────────────────────────┐
│  Capture App（既存: apps/desktop + app-service）                       │
│                                                                      │
│  ┌──────────────────────┐    ┌──────────────────────────────────┐   │
│  │ live_transcription   │    │ TranscriptEvent 変換 (新規)        │   │
│  │ (stt-deepgram etc.)  │───▶│ SttEvent → TranscriptEvent         │   │
│  └──────────────────────┘    └──────────────┬───────────────────┘   │
│                                              │ publish               │
└──────────────────────────────────────────────┼───────────────────────┘
                                                │
                                      ┌─────────▼──────────────┐
                                      │  Local Broker (新規)     │
                                      │  subject-based pub/sub   │
                                      │  tokio::broadcast ベース  │
                                      └──┬──────────┬───────────┘
                                         │          │
                               ┌─────────▼──┐  ┌───▼──────────────┐
                               │ UI Consumer │  │ Summary Consumer │
                               │ (表示更新)   │  │ (要約生成)        │
                               └─────────────┘  └──────────────────┘
```

### 3.2 レイヤ構成

| レイヤ | 所在 | 責務 |
|--------|------|------|
| **ASR Adapter** | 既存 `stt-deepgram` 等 | ベンダー固有の WebSocket/gRPC レスポンスを `SttEvent` に変換（変更なし） |
| **TranscriptEvent 変換** | 新規 `crates/transcript-event` | `SttEvent` から `TranscriptEvent` を直接生成 |
| **Local Broker** | 新規 `crates/local-broker` | subject ベースの pub/sub（フェーズ 1 は `tokio::broadcast` ベース） |
| **UI Consumer** | 既存 `apps/desktop` 改修 | `segment.updated` を購読して UI 更新 |
| **Summary Consumer** | 新規 `apps/desktop` 内に分離 | `segment.finalized` を購読して要約生成 |

---

## 4. イベントモデル

### 4.1 TranscriptEvent（Broker に publish される正規化済みイベント）

`SttEvent` から直接変換する。中間の `AsrProviderEvent` 層は設けない。

```rust
/// セグメントの共通データ。`SegmentUpdated` と `SegmentFinalized` で共有する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentData {
    pub segment_id: String,
    pub revision: u32,
    pub text: String,
    pub speaker_label: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
}

/// Local Broker に publish される正規化済みイベント。
/// 全 consumer がこの型だけを購読する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Finality {
    Interim,
    Final,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TranscriptEvent {
    /// セグメントの内容が更新された（最新スナップショット、差分ではない）
    SegmentUpdated {
        session_id: SessionId,
        data: SegmentData,
        finality: Finality,
    },
    /// このセグメントは今後更新されない（`is_final: true` の SttEvent から生成）
    SegmentFinalized {
        session_id: SessionId,
        data: SegmentData,
    },
    /// 発話の切れ目（Deepgram speech_final, AssemblyAI end_of_turn 等）
    UtteranceEnded {
        session_id: SessionId,
        segment_id: Option<String>,
        reason: UtteranceEndReason,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UtteranceEndReason {
    EndOfTurn,
    SpeechPause,
    SessionEnd,
}
```

### 4.2 SummaryEvent（要約 consumer の出力）

```rust
/// 要約生成の結果を通知するイベント。
/// Summary Consumer が publish し、UI Consumer や保存 Consumer が購読する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SummaryEvent {
    /// 要約生成を開始した
    Started {
        session_id: SessionId,
    },
    /// 要約生成に成功した
    Completed {
        session_id: SessionId,
        text: String,
        provider_model: String,
    },
    /// 要約生成に失敗した
    Failed {
        session_id: SessionId,
        error: String,
    },
}
```

### 4.3 イベントエンベロープ

Broker を流れる全メッセージに付与する共通ヘッダ：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope<T> {
    pub event_id: String,       // ULID、冪等性担保用
    pub schema_version: u32,    // スキーマ進化用
    pub producer: String,       // "capture-app"
    pub session_id: SessionId,
    pub occurred_at: DateTime<Utc>,
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub body: T,
}
```

### 4.4 Subject 命名規則

| Subject | 用途 | 購読者 |
|---------|------|--------|
| `transcription.{session_id}.segment.updated` | セグメント更新（interim + final） | UI Consumer |
| `transcription.{session_id}.segment.finalized` | セグメント確定 | Summary Consumer, 保存 Consumer |
| `transcription.{session_id}.utterance.ended` | 発話終了 | Summary Consumer（セッション終了検知） |
| `summary.{session_id}.started` | 要約生成開始 | UI Consumer（`summary_busy` 表示用） |
| `summary.{session_id}.completed` | 要約生成完了 | UI Consumer, 保存 Consumer |
| `summary.{session_id}.failed` | 要約生成失敗 | UI Consumer（エラー表示用） |

---

## 5. Local Broker 設計

### 5.1 トランスポート

- **Windows**: named pipe（`\\.\pipe\1on1-recorder-broker`）
- **macOS / Linux**: Unix domain socket（`$XDG_RUNTIME_DIR/1on1-recorder/broker.sock` または `/tmp/1on1-recorder-broker.sock`）

Rust の `interprocess` crate を使用し、OS 差分を隠蔽する。

### 5.2 フレーミング

length-prefixed frame：
```
[u32: payload_length] [payload: JSON/MessagePack bytes]
```

- 最大フレームサイズ: 1 MiB
- シリアライズ形式: JSON（デバッグ容易性を優先、MessagePack は後日最適化）

### 5.3 プロトコル

| メッセージ | 方向 | 内容 |
|------------|------|------|
| `SUB {subject}` | Consumer → Broker | 購読開始 |
| `UNSUB {subject}` | Consumer → Broker | 購読解除 |
| `PUB {subject, headers, payload}` | Producer → Broker | イベント発行 |
| `MSG {subject, reply_to, headers, payload}` | Broker → Consumer | イベント配信 |

### 5.4 フェーズ 1 の簡略化

実装の第一歩として、**IPC を伴う別プロセス Broker は実装せず**、同一プロセス内の `tokio::broadcast` チャネルで実装する。IPC 化はフェーズ 2 以降とする。

```rust
// crates/local-broker/src/lib.rs（フェーズ 1）
pub struct LocalBroker {
    /// subject → broadcast sender のマップ
    subjects: Arc<DashMap<String, broadcast::Sender<Vec<u8>>>>,
}

impl LocalBroker {
    pub fn new() -> Self { /* ... */ }
    pub fn subscribe(&self, subject: &str) -> broadcast::Receiver<Vec<u8>> { /* ... */ }
    pub fn publish<T: Serialize>(&self, subject: &str, event: EventEnvelope<T>) -> Result<(), BrokerError> { /* ... */ }
}
```

---

## 6. Consumer 設計

### 6.1 UI Consumer（既存 UI の Broker 購読化）

現在の `Arc<Mutex<Vec<TranscriptSegment>>>` ベースの UI 更新を Broker 購読に置き換える。UI Consumer は `transcription.{session_id}.segment.updated` を購読し、`SegmentData` から UI 表示用の状態を構築する。

```rust
/// UI Consumer: `segment.updated` を購読し、リアルタイム表示を更新する。
pub struct UiConsumer {
    broker: LocalBroker,
    /// segment_id → SegmentData のマップで UI 表示状態を管理
    segments: Arc<Mutex<BTreeMap<String, SegmentData>>>,
}

impl UiConsumer {
    pub async fn run(&self, session_id: SessionId) {
        let mut rx = self.broker.subscribe(
            &format!("transcription.{session_id}.segment.updated")
        );

        loop {
            match rx.recv().await {
                Ok(envelope) => {
                    let event: TranscriptEvent = deserialize(&envelope).unwrap();
                    if let TranscriptEvent::SegmentUpdated { data, finality, .. } = event {
                        // segment_id をキーに最新状態を保持（revision の大小比較は不要：
                        // broadcast は順序保証があるため、後から来たものが最新）
                        self.segments.lock().unwrap().insert(data.segment_id.clone(), data);
                        // UI 更新シグナルを発行（Dioxus の schedule_update 等）
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // lagged: SessionStore から全セグメントを再読み込みして復旧
                    self.recover_from_store(session_id).await;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    /// lagged 発生時の復旧：SessionStore から全セグメントを再読み込みし、
    /// segment_id で重複排除しながら状態を再構築する。
    async fn recover_from_store(&self, session_id: SessionId) {
        let segments = self.store.list_transcript_segments(session_id).ok()
            .unwrap_or_default();
        let mut map = self.segments.lock().unwrap();
        map.clear();
        for seg in &segments {
            if seg.is_final {
                let segment_id = segment_id_for(seg);
                map.insert(segment_id, SegmentData {
                    segment_id: segment_id.clone(),
                    revision: 0, // 再構築時は revision をリセット
                    text: seg.text.clone(),
                    speaker_label: speaker_label(seg.track, seg.speaker),
                    start_ms: seg.start_ms,
                    end_ms: seg.end_ms,
                });
            }
        }
    }
}
```

### 6.2 Summary Consumer

#### 6.2.1 ライフサイクルと複数セッション管理

Summary Consumer は `AppState` が保持する単一の long-lived インスタンスである。セッションごとに `spawn_summary_task()` を呼び出し、内部で `tokio::spawn` により独立したタスクを生成する。複数セッションが同時に録音されている場合、各セッションの要約タスクは独立して動作する。

```rust
/// Summary Consumer: アプリケーション全体で1つのインスタンス。
/// セッションごとに `spawn_summary_task()` でタスクを生成する。
pub struct SummaryConsumer {
    broker: LocalBroker,
    store: Arc<SessionStore>,
    credential_store: Arc<CredentialStore>,
    app_settings: Arc<Mutex<AppSettings>>,
}

impl SummaryConsumer {
    /// 指定されたセッションの要約生成タスクを開始する。
    /// 内部で `tokio::spawn` し、JoinHandle を返す。
    /// 呼び出し元は `AbortHandle` を使ってキャンセル可能。
    pub fn spawn_summary_task(&self, session_id: SessionId) -> tokio::task::JoinHandle<()> {
        let broker = self.broker.clone();
        let store = self.store.clone();
        let credential_store = self.credential_store.clone();
        let app_settings = self.app_settings.clone();

        tokio::spawn(async move {
            run_summary_task(broker, store, credential_store, app_settings, session_id).await;
        })
    }
}
```

#### 6.2.2 要約タスクの本体

```rust
async fn run_summary_task(
    broker: LocalBroker,
    store: Arc<SessionStore>,
    credential_store: Arc<CredentialStore>,
    app_settings: Arc<Mutex<AppSettings>>,
    session_id: SessionId,
) {
    // 1. 既存の確定セグメントを SessionStore から読み込み（途中参加対応）
    let existing = store.list_transcript_segments(session_id)
        .unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut turns: Vec<TranscriptTurn> = Vec::new();

    for seg in &existing {
        if seg.is_final {
            let sid = segment_id_for(seg);
            if seen.insert(sid.clone()) {
                turns.push(TranscriptTurn {
                    speaker: Some(speaker_label(seg.track, seg.speaker)),
                    text: seg.text.clone(),
                });
            }
        }
    }

    // 2. Broker 購読開始
    let mut segment_rx = broker.subscribe(
        &format!("transcription.{session_id}.segment.finalized")
    );
    let mut utterance_rx = broker.subscribe(
        &format!("transcription.{session_id}.utterance.ended")
    );

    // 3. イベントループ
    loop {
        tokio::select! {
            result = segment_rx.recv() => {
                match result {
                    Ok(envelope) => {
                        let event: TranscriptEvent = deserialize(&envelope).unwrap();
                        if let TranscriptEvent::SegmentFinalized { data, .. } = event {
                            // segment_id で重複排除（lagged リカバリ時に
                            // SessionStore から読み込んだものと重複する可能性がある）
                            if seen.insert(data.segment_id.clone()) {
                                turns.push(TranscriptTurn {
                                    speaker: Some(data.speaker_label),
                                    text: data.text,
                                });
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // lagged: SessionStore から差分を再読み込み
                        // 既存の seen セットを維持したまま新規セグメントのみ追加
                        recover_turns_from_store(&store, session_id, &mut seen, &mut turns);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            result = utterance_rx.recv() => {
                match result {
                    Ok(envelope) => {
                        let event: TranscriptEvent = deserialize(&envelope).unwrap();
                        if let TranscriptEvent::UtteranceEnded {
                            reason: UtteranceEndReason::SessionEnd, ..
                        } = event {
                            break; // セッション終了 → 要約生成へ
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // UtteranceEnded の lagged は無視（SessionEnd は
                        // セッション終了時に必ず再発行される前提）
                    }
                }
            }
        }
    }

    // 4. 要約生成
    if turns.is_empty() {
        return; // 要約対象なし
    }

    broker.publish(
        &format!("summary.{session_id}.started"),
        EventEnvelope::new(session_id, SummaryEvent::Started { session_id }),
    );

    let summarizer = build_summarizer(&credential_store, &app_settings);
    let options = build_options(&app_settings);
    let result = summarizer.summarize(&turns, &options).await;

    match result {
        Ok(text) => {
            let provider_model = format!("{}/{}",
                selected_provider(&credential_store).key(),
                selected_model(&credential_store),
            );
            store.insert_summary(&Summary {
                session_id,
                text: text.clone(),
                provider_model: provider_model.clone(),
                generated_at: Utc::now(),
            }).ok();

            broker.publish(
                &format!("summary.{session_id}.completed"),
                EventEnvelope::new(session_id, SummaryEvent::Completed {
                    session_id,
                    text,
                    provider_model,
                }),
            );
        }
        Err(e) => {
            broker.publish(
                &format!("summary.{session_id}.failed"),
                EventEnvelope::new(session_id, SummaryEvent::Failed {
                    session_id,
                    error: e.to_string(),
                }),
            );
        }
    }
}

/// lagged 発生時の部分復旧：SessionStore から全セグメントを再読み込みし、
/// 既存の seen セットにない新規セグメントのみ turns に追加する。
fn recover_turns_from_store(
    store: &SessionStore,
    session_id: SessionId,
    seen: &mut HashSet<String>,
    turns: &mut Vec<TranscriptTurn>,
) {
    let segments = store.list_transcript_segments(session_id)
        .unwrap_or_default();
    for seg in &segments {
        if seg.is_final {
            let sid = segment_id_for(seg);
            if seen.insert(sid) {
                turns.push(TranscriptTurn {
                    speaker: Some(speaker_label(seg.track, seg.speaker)),
                    text: seg.text.clone(),
                });
            }
        }
    }
}
```

#### 6.2.3 自動要約トリガー

`UtteranceEnded { reason: SessionEnd }` をトリガーとして要約を自動生成する。ユーザーが手動で「要約を生成」ボタンを押した場合の動作も維持する（UI から直接 `SummaryConsumer::spawn_summary_task()` を呼び出す）。

手動実行時は `UtteranceEnded` の到着を待たず、その時点で `SessionStore` に存在する全セグメントを収集して即座に要約を生成する。

---

## 7. SttEvent → TranscriptEvent 変換

### 7.1 既存コードとの関係

`live_transcription.rs` の `persist_event` 関数を拡張し、`SttEvent` から `TranscriptEvent` を直接生成して Local Broker に publish する経路を追加する。中間の `AsrProviderEvent` 層は設けない。

```rust
// live_transcription.rs の変更（疑似コード）
async fn persist_event(
    store: &SessionStore,
    broker: &LocalBroker,
    session_id: SessionId,
    track: TrackKind,
    event: &SttEvent,
    speaker: Option<u32>,
    segment_id_counter: &mut u64, // セグメント ID 生成用カウンタ
) {
    // 既存: SessionStore への書き込み（後方互換性のため維持）
    let segment = to_transcript_segment(session_id, track, event, speaker);
    store.insert_transcript_segment(&segment)?;

    // 新規: SttEvent → TranscriptEvent を直接生成して Broker に publish
    let transcript_event = stt_to_transcript_event(
        session_id, &segment, event, segment_id_counter,
    );
    broker.publish(
        &subject_for(&transcript_event, session_id),
        EventEnvelope::new(session_id, transcript_event),
    );

    // segment_id は is_final: true の時だけインクリメント（interim は同一 segment_id を再利用）
    if segment.is_final {
        *segment_id_counter += 1;
    }
}
```

### 7.2 各ベンダーのマッピング

| ベンダー | SttEvent | TranscriptEvent |
|----------|----------|-----------------|
| Deepgram | `PartialTranscript(is_final=false)` | `SegmentUpdated(Interim)` |
| Deepgram | `FinalTranscript(is_final=true)` | `SegmentUpdated(Final)` + `SegmentFinalized` |
| Deepgram | `SpeechEnded` | `UtteranceEnded(EndOfTurn)` |
| Google | `PartialTranscript(is_final=false)` | `SegmentUpdated(Interim)` |
| Google | `FinalTranscript(is_final=true)` | `SegmentUpdated(Final)` + `SegmentFinalized` |
| Google | `SpeechActivityEnd` | `UtteranceEnded(SpeechPause)` |
| AssemblyAI | streaming chunk | `SegmentUpdated(Interim)` |
| AssemblyAI | Turn(end_of_turn=true) | `SegmentFinalized` + `UtteranceEnded(EndOfTurn)` |
| （全ベンダー） | `finalize()` 完了 | `UtteranceEnded(SessionEnd)` |

### 7.3 segment_id の生成規則

`segment_id` は `{session_id}:{track_kind}:{counter}` の形式で生成する。`counter` は `is_final: true` の `TranscriptSegment` が確定するたびにインクリメントされる。interim 更新は同一 `segment_id` に対して `revision` をインクリメントする。

```rust
fn segment_id_for_segment(seg: &TranscriptSegment) -> String {
    format!("{}:{}:{}", seg.session_id, track_kind_key(seg.track), seg.start_ms.unwrap_or(0))
}

fn segment_id_for(session_id: SessionId, track: TrackKind, counter: u64) -> String {
    format!("{session_id}:{}:{counter}", track_kind_key(Some(track)))
}
```

---

## 8. 新規クレート構成

### 8.1 `crates/transcript-event`

```toml
[package]
name = "transcript-event"
edition = "2021"

[dependencies]
recorder-domain = { path = "../recorder-domain" }
serde = { workspace = true }
chrono = { workspace = true }
```

公開 API:
- `TranscriptEvent`, `Finality`, `UtteranceEndReason`, `SegmentData`
- `SummaryEvent`
- `EventEnvelope<T>`
- `subject_for(event: &TranscriptEvent, session_id: SessionId) -> String`
- `segment_id_for(session_id, track, counter) -> String`

### 8.2 `crates/local-broker`

```toml
[package]
name = "local-broker"
edition = "2021"

[dependencies]
transcript-event = { path = "../transcript-event" }
tokio = { workspace = true }
dashmap = "6"
serde_json = { workspace = true }
thiserror = { workspace = true }
```

---

## 9. 導入ステップ

### Step 1: `transcript-event` クレートの作成と型定義

- `TranscriptEvent`, `Finality`, `UtteranceEndReason`, `SegmentData` を定義
- `SummaryEvent` を定義
- `EventEnvelope<T>` を定義
- `subject_for()` 関数を実装
- **影響範囲**: `crates/transcript-event`（新規）

### Step 2: `local-broker` クレートの作成（プロセス内版）

- `tokio::broadcast` ベースの `LocalBroker` を実装
- `DashMap` で subject → sender のマッピングを管理
- `subscribe()` / `publish()` / `unsubscribe()` を実装
- **影響範囲**: `crates/local-broker`（新規）

### Step 3: `live_transcription.rs` に publish 経路を追加

- `persist_event` を拡張し、`SttEvent` から `TranscriptEvent` を直接生成して Broker に publish
- `finalize()` 完了時に `UtteranceEnded(SessionEnd)` を publish
- `SessionStore` への既存の書き込みは維持（後方互換性）
- segment_id カウンタを導入
- **影響範囲**: `crates/app-service/src/live_transcription.rs`

### Step 4: UI Consumer を Broker 購読に切り替え

- 現在の `Arc<Mutex<Vec<TranscriptSegment>>>` ベースの UI 更新を Broker 購読に置き換え
- `segment.updated` を購読し、`segment_id` をキーに `BTreeMap` で UI 表示状態を管理
- `lagged` 検出時に `SessionStore` から全セグメントを再読み込みする fallback を実装
- **影響範囲**: `apps/desktop/src/ui.rs`

### Step 5: `on_generate_summary` を Summary Consumer に分離

- `apps/desktop/src/summary_consumer.rs` を作成
- UI ハンドラから要約ロジックを移動
- `segment.finalized` 購読 + `UtteranceEnded(SessionEnd)` トリガーで自動要約
- 途中参加対応（起動時に `SessionStore` から既存セグメントを読み込み）
- `lagged` リカバリ（`seen` セットで重複排除しながら差分を再読み込み）
- 要約成功時は `summary.{session_id}.completed`、失敗時は `summary.{session_id}.failed` を publish
- 手動「要約を生成」ボタンは `spawn_summary_task()` を直接呼び出す
- **影響範囲**: `apps/desktop/src/ui.rs`, `apps/desktop/src/summary_consumer.rs`（新規）

### Step 6: イベントログの永続化（オプション）

- 保存 Consumer を追加し、全イベントを append-only で永続化
- 再起動後のリプレイを可能にする
- **影響範囲**: `crates/session-store`（イベントログテーブル追加）

---

## 10. リスクと対策

| リスク | 対策 |
|--------|------|
| Broker の単一障害点 | フェーズ 1 は同一プロセス内のため、プロセスが死ねば全体が停止する。IPC 化時に再接続・バッファリングを実装する |
| `tokio::broadcast` の `lagged` | consumer 側で `lagged` 検出時に `SessionStore` から再読み込み。UI Consumer は全件再読み込み + 状態再構築、Summary Consumer は `seen` セットで重複排除しながら差分追加 |
| consumer の途中参加 | Summary Consumer 起動時に `SessionStore` から既存の確定セグメントを読み込み、`seen` セットで初期化。その後の Broker 購読で新規セグメントを追加 |
| 要約 consumer の遅延 | 要約生成は `tokio::spawn` による非同期タスクとして実行され、UI スレッドをブロックしない |
| 既存の手動要約ワークフローとの整合性 | `on_generate_summary` の既存パスはそのまま残し、Summary Consumer は追加の自動化パスとして提供する |
| 複数セッション同時実行 | Summary Consumer は単一の long-lived インスタンス。セッションごとに `spawn_summary_task()` で独立したタスクを生成し、各タスクが独立して動作する |
| 要約生成失敗時の UI 状態 | `SummaryEvent::Failed` を publish し、UI Consumer が `summary.{session_id}.failed` を購読して `summary_busy` フラグをリセット + エラーメッセージを表示する |

---

## 11. 既存コードへの影響まとめ

| ファイル | 変更内容 | 破壊的変更 |
|----------|----------|------------|
| `crates/transcript-event/` | **新規** | なし |
| `crates/local-broker/` | **新規** | なし |
| `crates/app-service/src/live_transcription.rs` | Broker への publish 経路追加、segment_id カウンタ導入 | なし |
| `crates/app-service/Cargo.toml` | `transcript-event`, `local-broker` 依存追加 | なし |
| `apps/desktop/src/summary_consumer.rs` | **新規** | なし |
| `apps/desktop/src/ui.rs` | `on_generate_summary` から要約ロジックを分離、UI Consumer として Broker 購読に切り替え | なし（既存パス維持） |
| `apps/desktop/Cargo.toml` | `transcript-event`, `local-broker` 依存追加 | なし |
| `Cargo.toml` | workspace members に2クレート追加 | なし |

`summarize` クレート、`session-store` クレート、`credential-store` クレート、`stt-api` クレートの公開 API に変更はない。

---

## 12. 将来の拡張余地

- **IPC Broker**: フェーズ 2 で `interprocess` crate を使った別プロセス Broker に移行
- **NATS 移行**: subject 命名が NATS 互換であるため、Local Broker を NATS に置き換え可能
- **JetStream 永続化**: `summary.completed` など永続が必要なイベントを JetStream に流す
- **Drive/Gemini 連携**: `summary.completed` を購読する新 consumer を追加するだけ
- **ストリーミング要約**: 録音中に `segment.finalized` が来るたびに要約を逐次更新する consumer を追加可能