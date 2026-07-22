# サマリー機能の Rhai プラグイン化 設計書

* **文書ステータス**: Draft v0.2（レビュー指摘反映済み）
* **作成日**: 2026-07-20
* **関連文書**: [decouple-summary-transcription.md](../docs/decouple-summary-transcription.md)
* **位置づけ**: 現在 `SummaryConsumer` として Rust にハードコードされているサマリー生成ロジックを Rhai プラグインに移植し、ユーザーがカスタマイズ可能にする。コードレビュー済み。

---

## 1. 現状分析

### 1.1 SummaryConsumer の責務

`apps/desktop/src/summary_consumer.rs`（322行）は以下の責務を持つ：

| 責務 | 内容 | Rust/Rhai |
|------|------|-----------|
| イベント購読 | `segment.finalized` + `utterance.ended` を Broker 購読 | **Rhai 化** |
| ターン収集 | `SegmentFinalized` から `TranscriptTurn` を蓄積 | **Rhai 化** |
| 重複排除 | `segment_id` で `HashSet` 管理 | **Rhai 化** |
| トリガー判定 | `SessionEnd` で要約開始 | **Rhai 化** |
| プロバイダ選択 | `CredentialStore` から読み出し | **Rust 維持** |
| AI 呼び出し | genai / CLI / Vertex / Ollama の4経路 | **Rust 維持** |
| 結果永続化 | `SessionStore::insert_summary()` | **Rust 維持** |
| イベント発行 | `SummaryEvent` を Broker に publish | **Rust 維持** |

### 1.2 移植の原則

```
「何をするか」→ Rhai スクリプト（ユーザーがカスタマイズ可能）
「どうやるか」→ Rust ホスト（APIキー管理、認証、永続化）
```

Rust 側は **安全なプリミティブ** を提供し、Rhai スクリプトはそれらを **組み合わせてワークフローを定義** する。

---

## 2. アーキテクチャ

### 2.1 全体像

```
Capture App
  │
  ├── LocalBroker
  │     ├──▶ UI Consumer（Rust, 変更なし）
  │     ├──▶ RhaiEngine（新規）
  │     │     ├── plugins/default/summary.rhai  ← デフォルト要約
  │     │     ├── plugins/default/std.rhai       ← 標準ライブラリ（Rhai側）
  │     │     └── plugins/user/*.rhai             ← ユーザープラグイン
  │     │
  │     └──▶ （将来）SocketListener（named pipe, 任意言語）
  │
  └── SummaryConsumer（Rust, 削除）
        └── 機能は summary.rhai に移行
```

### 2.2 レイヤ構成

```
┌─────────────────────────────────────────────┐
│  Rhai スクリプト層（plugins/）               │
│  - ワークフロー定義                          │
│  - イベントハンドラ                          │
│  - 条件分岐・ループ                          │
├─────────────────────────────────────────────┤
│  RhaiEngine（crates/rhai-engine/）           │
│  - スクリプト読み込み・コンパイル            │
│  - ブローカー購読・イベントディスパッチ      │
│  - Scope 管理（セッション間の状態保持）      │
│  - エラーハンドリング・サンドボックス        │
├─────────────────────────────────────────────┤
│  Standard Library（Rust 側で登録）           │
│  - call_ai()         - AI 呼び出し           │
│  - get_setting()     - 設定読み取り          │
│  - save_summary()    - 要約永続化            │
│  - publish_event()   - イベント発行          │
│  - list_segments()   - セグメント一覧取得    │
│  - log_info/warn()   - ログ出力             │
└─────────────────────────────────────────────┘
```

---

## 3. Hook 定義

Rhai スクリプトは以下の hook 関数を定義する。RhaiEngine はイベント受信時に該当 hook が存在すれば呼び出す。

### 3.1 イベントフック

| hook 関数 | 発火タイミング | 引数 | 戻り値 |
|-----------|---------------|------|--------|
| `on_session_start(session_id)` | セッション開始時（`start_session()` 呼び出し） | session_id: 文字列 | なし |
| `on_segment_update(data)` | セグメント更新のたび（interim 含む） | `#{segment_id, text, speaker_label, track, is_final, start_ms, end_ms}` | なし |
| `on_segment_finalized(data)` | セグメント確定時のみ | 同上 | なし |
| `on_utterance_ended(reason)` | 発話の切れ目 | `"EndOfTurn"` / `"SpeechPause"` / `"SessionEnd"` | なし |
| `on_session_end()` | 録音終了（`SessionEnd` 受信時） | なし | なし |
| `on_manual_summary(session_id)` | ユーザーが「要約を生成」ボタンを押した時 | session_id: 文字列 | なし（または要約テキスト） |
| `on_summary_completed(text, model)` | 要約生成完了（`SummaryEvent::Completed`） | text: 要約テキスト, model: プロバイダ/モデル名 | なし |

### 3.2 ライフサイクルフック

| hook 関数 | 発火タイミング | 用途 |
|-----------|---------------|------|
| `on_load()` | スクリプト読み込み時 | 初期化、定数定義 |
| `on_unload()` | スクリプト解放時 | クリーンアップ |

---

## 4. 標準ライブラリ関数（Rust 側で登録）

### 4.1 非同期コマンド（統一インターフェース）

```rust
// Rhai に登録する関数は1つだけ
fn call_async(name: &str, args: rhai::Map) -> Result<Dynamic, Box<EvalAltResult>>;
```

`call_async` は内部で MPSC チャネルを使って async ワーカーにコマンドを送り、
oneshot チャネルで結果を同期待機する。Rhai スクリプト開発者は `call_async` を
直接使うのではなく、`std.rhai` が提供する curry ラッパー関数を呼び出す。

```js
// std.rhai — Rhai 側の curry ラッパー
fn call_ai(model, system_prompt, turns) {
    call_async("ai_summarize", #{ model, system_prompt, turns })
}

fn http_get(url) {
    call_async("http_get", #{ url })
}

fn http_post(url, body) {
    call_async("http_post", #{ url, body })
}
```

Rust 側の async ワーカーは単一のコマンドディスパッチャ：

```rust
enum AsyncCommand {
    CallAi { model: String, system_prompt: String, turns: Vec<Turn>, reply: oneshot::Sender<Result<String>> },
    HttpGet { url: String, reply: oneshot::Sender<Result<String>> },
    HttpPost { url: String, body: String, reply: oneshot::Sender<Result<String>> },
    // 将来の拡張はここに variant を追加するだけ
}
```

### 4.2 データアクセス

```rust
// Rhai: get_setting(key)
// key: "ollama_base_url" | "summary_template" | "silence_gate_enabled"
fn get_setting(key: &str) -> Dynamic;

// Rhai: get_selected_model()
// 戻り値: "claude-sonnet-4-5" のようなモデル識別子文字列
// CredentialStore から SELECTED_PROVIDER_ACCOUNT + SELECTED_MODEL_ACCOUNT を読み出す
fn get_selected_model() -> String;

// Rhai: get_session_metadata(session_id)
// 戻り値: #{started_at: "...", tracks: [...], ...}
fn get_session_metadata(session_id: &str) -> rhai::Map;

// Rhai: list_segments(session_id)
// 戻り値: Array of #{segment_id, text, speaker_label, track, is_final, start_ms, end_ms}
// is_final: false のセグメントも含まれる（interim 含む全件）
fn list_segments(session_id: &str) -> rhai::Array;

// Rhai: save_summary(session_id, text, provider_model)
fn save_summary(session_id: &str, text: &str, provider_model: &str);

// Rhai: format_turns(turns, format)
// format: "markdown" | "text" | "json"
// 戻り値: フォーマット済みの文字列
fn format_turns(turns: rhai::Array, format: &str) -> String;
```

### 4.3 イベント発行

```rust
// Rhai: publish_event(subject, data)
// subject: "summary.{session_id}.completed" など
// data: 任意のオブジェクトマップ
fn publish_event(subject: &str, data: rhai::Map);
```

### 4.4 ユーティリティ

```rust
fn log_info(msg: &str);
fn log_warn(msg: &str);
fn log_error(msg: &str);

// Rhai: sleep_ms(ms)
// 指定ミリ秒だけ待機。Rhai Engine の操作数制限ではカウントされない。
fn sleep_ms(ms: i64);
```

---

## 5. デフォルトプラグイン: `summary.rhai`

現在の `SummaryConsumer` のロジックを Rhai に移植したもの。

```js
// plugins/default/summary.rhai
// デフォルトのサマリー生成プラグイン
// 現在の SummaryConsumer と同等の動作をする

import "std" as std;

let turns = [];          // 収集したターン
let seen = #{};

fn on_session_start(session_id) {
    turns = [];
    seen = #{};
}

fn on_segment_finalized(data) {
    if seen.contains(data.segment_id) {
        return;
    }
    seen[data.segment_id] = true;
    turns.push(#{
        speaker: data.speaker_label,
        text: data.text,
    });
}

fn on_session_end() {
    if turns.is_empty() {
        let segments = list_segments(session_id);
        for seg in segments {
            if seg.is_final && !seen.contains(seg.segment_id) {
                seen[seg.segment_id] = true;
                turns.push(#{ speaker: seg.speaker_label, text: seg.text });
            }
        }
    }
    if turns.is_empty() {
        return;
    }

    let model = get_selected_model();
    let system_prompt = get_setting("summary_template");
    if system_prompt == () {
        system_prompt = "You summarize 1-on-1 meeting transcripts. Produce a concise summary covering key discussion points, decisions, and action items.";
    }

    publish_event(`summary.${session_id}.started`, #{ session_id });

    try {
        let text = call_ai(model, system_prompt, turns);
        save_summary(session_id, text, model);
        publish_event(`summary.${session_id}.completed`, #{ session_id, text, provider_model: model });
    } catch (err) {
        log_error("要約に失敗しました: " + err);
        publish_event(`summary.${session_id}.failed`, #{ session_id, error: err });
    }
}

fn on_manual_summary(session_id) {
    turns = [];
    seen = #{};
    let segments = list_segments(session_id);
    for seg in segments {
        if seg.is_final && !seen.contains(seg.segment_id) {
            seen[seg.segment_id] = true;
            turns.push(#{ speaker: seg.speaker_label, text: seg.text });
        }
    }
    if turns.is_empty() {
        return;
    }

    let model = get_selected_model();
    let system_prompt = get_setting("summary_template");
    if system_prompt == () {
        system_prompt = "You summarize 1-on-1 meeting transcripts. Produce a concise summary covering key discussion points, decisions, and action items.";
    }

    publish_event(`summary.${session_id}.started`, #{ session_id });

    try {
        let text = call_ai(model, system_prompt, turns);
        save_summary(session_id, text, model);
        publish_event(`summary.${session_id}.completed`, #{ session_id, text, provider_model: model });
    } catch (err) {
        log_error("要約に失敗しました: " + err);
        publish_event(`summary.${session_id}.failed`, #{ session_id, error: err });
    }
}

fn on_load() {
    log_info("summary.rhai loaded");
}
```

---

## 6. RhaiEngine の実装

### 6.1 クレート構成

```
crates/rhai-engine/
  ├── Cargo.toml      # rhai = { version = "=1.20.0", features = ["sync"] }
  └── src/
      ├── lib.rs           # RhaiEngine 構造体、公開API
      ├── engine.rs        # Rhai Engine のセットアップ、スクリプト読み込み
      ├── hooks.rs         # フックディスパッチ
      ├── stdlib.rs        # 標準ライブラリ関数の登録（call_async 1つのみ）
      ├── dispatcher.rs    # 汎用 async コマンドディスパッチャ
      └── scope.rs         # セッション単位の Scope 管理
```

### 6.2 RhaiEngine API

```rust
pub struct RhaiEngine {
    engine: rhai::Engine,
    broker: LocalBroker,
    store: Arc<SessionStore>,
    credential_store: Arc<FallbackCredentialStore>,
    app_settings: Arc<Mutex<AppSettings>>,
    scripts: Vec<CompiledScript>,
    /// スクリプトID × セッションID で Scope を管理。
    /// 各スクリプトは独立した Scope を持ち、変数名の衝突がない。
    active_scopes: DashMap<(usize, SessionId), Scope<'static>>,
    /// call_async() 用の async ワーカーへのコマンド送信チャネル
    command_tx: mpsc::UnboundedSender<AsyncCommand>,
}

impl RhaiEngine {
    pub fn new(
        broker: LocalBroker, store: Arc<SessionStore>,
        credential_store: Arc<FallbackCredentialStore>,
        app_settings: Arc<Mutex<AppSettings>>,
    ) -> (Self, tokio::task::JoinHandle<()>);  // JoinHandle は async_worker のもの

    /// plugins/ ディレクトリから全 .rhai ファイルを読み込みコンパイル。
    /// default/ を先に読み込み、その後 user/ を読み込む。
    pub fn load_plugins(&mut self, plugin_dir: &Path) -> Result<usize, RhaiError>;

    /// セッション開始時に呼ぶ。全スクリプトの on_session_start を発火。
    pub fn start_session(&self, session_id: SessionId);

    /// セッション終了時に呼ぶ。全スクリプトの Scope を解放。
    pub fn end_session(&self, session_id: SessionId);

    /// 手動要約（「要約を生成」ボタン）用。全スクリプトの on_manual_summary を発火。
    pub fn trigger_manual_summary(&self, session_id: SessionId);
}
```

### 6.3 call_async() の非同期処理パターン（MPSC チャネル）

RhaiEngine は `spawn_blocking` で隔離された同期スレッドで動作する。
`call_async()` は MPSC チャネルで async ワーカーにリクエストを送り、
oneshot チャネルで結果を同期待機する。

```
┌──────────────────────────────┐       ┌──────────────────────────────┐
│ RhaiEngine (spawn_blocking)  │       │ Async Worker (tokio)         │
│                              │       │                              │
│  call_async("ai_summarize",) │──Cmd──▶  match command.name {        │
│    rx.recv()  ← 同期待機     │◀─Reply──    "ai_summarize" => ...    │
│    return result             │       │    "http_get" => ...         │
│                              │       │  }                           │
└──────────────────────────────┘       └──────────────────────────────┘
```

```rust
// Rhai に登録する同期関数（この1つだけ）
fn call_async(name: &str, args: rhai::Map) -> Result<Dynamic, Box<EvalAltResult>> {
    let (tx, rx) = std::sync::mpsc::channel();
    self.command_tx.send(AsyncCommand {
        name: name.to_string(),
        args: args.into(),
        reply: tx,
    }).map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?
}

// async ワーカー側（RhaiEngine 起動時に spawn）
async fn async_worker(
    mut command_rx: mpsc::UnboundedReceiver<AsyncCommand>,
    credential_store: Arc<FallbackCredentialStore>,
    app_settings: Arc<Mutex<AppSettings>>,
) {
    while let Some(cmd) = command_rx.recv().await {
        let result = match cmd.name.as_str() {
            "ai_summarize" => handle_ai_summarize(cmd.args).await,
            "http_get" => handle_http_get(cmd.args).await,
            "http_post" => handle_http_post(cmd.args).await,
            _ => Err(format!("unknown async command: {}", cmd.name)),
        };
        cmd.reply.send(result).ok();
    }
}
```

### 6.4 RhaiEngine の実行モデル

```rust
impl RhaiEngine {
    /// 全セッションのイベントループを開始する。
    /// この関数は spawn_blocking で呼び出すこと。
    pub fn run(self, session_id: SessionId) {
        // 1. Broker 購読
        let mut segment_rx = self.broker.subscribe(&segment_subject);
        let mut utterance_rx = self.broker.subscribe(&utterance_subject);

        loop {
            // 2. イベント受信 → 全スクリプトの hook をディスパッチ
            tokio::select! {
                // ...
            }
        }
    }
}

// アプリ側の呼び出し
tokio::task::spawn_blocking(move || {
    engine.run(session_id);
});
```

### 6.3 イベントディスパッチの流れ

```
Broker イベント受信
  │
  ├─▶ ProtocolValidator::validate()
  │
  ├─▶ JSON → Rhai Dynamic に変換
  │
  ├─▶ 全ロード済みスクリプトに対して:
  │     if hook_exists(script, "on_segment_finalized") {
  │         engine.call_fn(&mut scope, "on_segment_finalized", (data,));
  │     }
  │
  └─▶ エラー発生時: log_error() + スクリプト実行継続（他のスクリプトは影響なし）
```

### 6.5 セッション間の Scope 管理

```rust
// セッション開始時 — 全スクリプトの Scope を初期化
for (idx, script) in scripts.iter().enumerate() {
    let mut scope = Scope::new();
    scope.push("session_id", session_id.to_string());
    active_scopes.insert((idx, session_id), scope);
    if has_hook(script, "on_session_start") {
        engine.call_fn(&mut scope, "on_session_start", (session_id.to_string(),));
    }
}

// イベントディスパッチ時 — スクリプトID × セッションID で Scope を取得
for (idx, script) in scripts.iter().enumerate() {
    if let Some(scope) = active_scopes.get_mut(&(idx, session_id)) {
        if has_hook(script, hook_name) {
            engine.call_fn_with_scope(scope, hook_name, (data,));
        }
    }
}

// セッション終了時 — 全スクリプトの Scope を解放
for (idx, _) in scripts.iter().enumerate() {
    active_scopes.remove(&(idx, session_id));
}
```

---

## 7. ファイル構成

```
plugins/
  ├── default/                    # デフォルトプラグイン（アプリ同梱）
  │   ├── summary.rhai            # デフォルト要約
  │   └── std.rhai                # Rhai 側の共通ユーティリティ
  │
  └── user/                       # ユーザープラグイン（ユーザーが自由に追加）
      ├── notify_slack.rhai       # 例: Slack 通知
      ├── format_minutes.rhai     # 例: 議事録フォーマット
      └── auto_tag.rhai           # 例: 自動タグ付け
```

`plugins/default/` はアプリのインストールディレクトリに同梱。`plugins/user/` は `$APPDATA/1on1-recorder/plugins/` に配置。

---

## 8. SummaryConsumer の削除計画

### 8.1 削除するコード

| ファイル | 削除内容 |
|----------|----------|
| `apps/desktop/src/summary_consumer.rs` | ファイル全体を削除 |
| `apps/desktop/src/app_state.rs` | `summary_consumer` フィールドを削除 |
| `apps/desktop/src/main.rs` | `SummaryConsumer::new()` の初期化を削除 |
| `apps/desktop/src/ui.rs` | `on_generate_summary` から `summary_consumer.generate_summary_now()` 呼び出しを削除 |

### 8.2 手動要約（「要約を生成」ボタン）の代替

UI の「要約を生成」ボタンは、`RhaiEngine` に対して `call_hook("on_manual_summary", session_id)` を呼び出す。`summary.rhai` 側で `on_manual_summary(session_id)` を定義し、`list_segments(session_id)` でセグメントを取得して要約を生成する。

---

## 9. 導入ステップ

### Step 1: `crates/rhai-engine` クレート作成

- Cargo.toml 設定（rhai, local-broker, transcript-event, session-store に依存）
- `RhaiEngine` 構造体の定義
- 標準ライブラリ関数の登録（`stdlib.rs`）
- スクリプト読み込み（`engine.rs`）

### Step 2: Hook ディスパッチの実装

- Broker 購読とイベント→Rhai Dynamic 変換
- フック存在チェックと呼び出し
- Scope 管理（セッション単位）

### Step 3: デフォルトプラグインの作成

- `plugins/default/summary.rhai` の作成
- `plugins/default/std.rhai` の作成

### Step 4: アプリへの統合

- `main.rs` で `RhaiEngine` を初期化
- `AppState` に `rhai_engine` を追加
- `on_start_recording` で `rhai_engine.start_session()` を呼ぶ
- `on_generate_summary` を `rhai_engine.trigger_manual_summary()` に置き換え

### Step 4.5: フィーチャーフラグによる並行稼働（推奨）

- `AppSettings` に `use_rhai_plugins: Option<bool>` を追加
- `None`（未設定）または `false` の場合は従来の `SummaryConsumer` を使用
- `true` の場合は `RhaiEngine` を使用
- 1〜2リリースの並行稼働期間を経て、Step 5 で `SummaryConsumer` を削除

### Step 5: SummaryConsumer の削除

- `summary_consumer.rs` を削除
- 関連するインポートとフィールドを削除
- 動作確認

---

## 10. リスクと対策

| リスク | 対策 |
|--------|------|
| Rhai スクリプトの実行時エラー | エラーはログに出力し、他のスクリプトの実行は継続。1つのスクリプトの失敗が全体を止めない |
| スクリプトの無限ループ | `Engine::set_max_operations()` で操作数制限を設定（デフォルト 100,000） |
| スクリプトのメモリ消費 | `Engine::set_max_string_size()` + `set_max_array_size()` で制限 |
| セッション間の状態リーク | `end_session()` で全スクリプトの Scope を削除、`DashMap` からエントリを除去 |
| パフォーマンス低下（`on_segment_update`） | 高頻度で呼ばれるため、スクリプト側で軽量な処理に限定するようドキュメント化。または `on_segment_update` を `enable_realtime_hooks` 設定でオプトインにする |
| Rhai の破壊的変更 | `Cargo.toml` で `rhai = "=1.20.0"` のようにバージョンを固定。アップグレード時は全プラグインの動作確認を必須とする |
| スクリプト読み込み順序の曖昧さ | `default/` → `user/` の順で読み込み。同名フックがある場合、`user/` のスクリプトが後から実行される（上書きではなく追加実行） |
| ホットリロード未対応 | 初回リリースでは非対応。将来の拡張として `notify` crate を使ったファイル監視を検討 |
| エラーメッセージの国際化 | Rhai スクリプト内の文字列は国際化対象外。UI に表示するエラーメッセージは Rust 側でラップする |
| Windows の plugins パス解決 | `dirs::data_dir()` を使用し `$APPDATA/1on1-recorder/plugins/` に解決。デフォルトプラグインは実行ファイルと同梱 |

---

## 11. 既存コードへの影響まとめ

| 変更 | 種類 | 影響範囲 |
|------|------|----------|
| `crates/rhai-engine/` | 新規 | なし |
| `apps/desktop/Cargo.toml` | rhai-engine 依存追加 | なし |
| `apps/desktop/src/main.rs` | RhaiEngine 初期化 | 中 |
| `apps/desktop/src/app_state.rs` | rhai_engine 追加, summary_consumer フィールドは残す（フィーチャーフラグで共存） | 中 |
| `apps/desktop/src/app_settings.rs` | `use_rhai_plugins: Option<bool>` 追加 | 小 |
| `apps/desktop/src/ui.rs` | on_generate_summary をフィーチャーフラグで分岐 | 小 |
| `apps/desktop/src/summary_consumer.rs` | 変更なし（フィーチャーフラグで共存後、Step 5 で削除） | 大 |
| `plugins/default/summary.rhai` | 新規 | なし |
| `plugins/default/std.rhai` | 新規 | なし |
| `Cargo.toml` | workspace members に rhai-engine 追加 | 小 |