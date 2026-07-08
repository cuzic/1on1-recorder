# 技術検証スパイク実施計画

* **文書ステータス**: Draft v0.1
* **作成日**: 2026-07-06
* **対象設計書**: [design.md](design.md) — デスクトップ2トラック会議レコーダー Phase 1 設計書
* **目的**: Phase 1A 実装着手前に、設計の前提となる技術的仮説を最小コストで検証し、実現不可能な箇所を早期に発見する

---

## 1. スパイクの進め方

### 1.1 原則

* 各スパイクは **使い捨てコード** とする。プロダクションコードへの直接流用は前提にしない
* 各スパイクに **仮説・検証手順・合否基準・タイムボックス** を明記する
* タイムボックス超過時は「未検証のまま延長」ではなく、**結果を記録して判断会議へエスカレーション** する
* 成果物は `spikes/{spike-id}/` 配下にコードと `RESULT.md`(結果レポート)として残す

### 1.2 結果レポートのフォーマット

```markdown
# SPIKE-XX 結果
- 判定: GO / CONDITIONAL-GO / NO-GO
- 検証環境: OS バージョン、デバイス、会議アプリのバージョン
- 測定値: 合否基準に対する実測値
- 発見した制約・落とし穴
- 設計書への反映事項(修正が必要な節を明記)
```

### 1.3 優先順位の考え方

design.md の実装フェーズ(§21)は Windows 縦切り(Phase 1A)から始まるため、
**Windows キャプチャ系と共通タイムライン系を最優先(Wave 1)** とし、
macOS / Linux は方式の成立確認レベル(Wave 2)、周辺技術(Wave 3)の順で行う。

---

## 2. スパイク一覧(サマリ)

| ID | テーマ | リスク | Wave | タイムボックス |
|----|------|------|------|----------|
| SPIKE-01 | WASAPI マイク + Endpoint Loopback 同時取得とタイムスタンプ | 高 | 1 | 3日 |
| SPIKE-02 | Windows Application Loopback Capture(プロセス指定) | 高 | 1 | 3日 |
| SPIKE-11 | Audio Endpoint Registry(全デバイスの観測状態追跡) | 高 | 1 | 3日 |
| SPIKE-12 | Capture Rebinding State Machine(再バインド時のepoch整合性) | 高 | 1 | 4日 |
| SPIKE-03 | 共通タイムライン整列・drift 補正アルゴリズム(疑似音源) | 高 | 1 | 4日 |
| SPIKE-04 | Opus 30秒セグメントの atomic commit とクラッシュ復旧 | 中 | 1 | 2日 |
| SPIKE-05 | Tauri 2 常駐(トレイ・ウィンドウ閉鎖時の録音継続) | 中 | 1 | 2日 |
| SPIKE-06 | macOS ScreenCaptureKit: system audio + microphone 同時取得 | 高 | 2 | 4日 |
| SPIKE-07 | Linux PipeWire: playback node 取得と sink monitor フォールバック | 高 | 2 | 4日 |
| SPIKE-08 | チャンクアップロード + Idempotency-Key による重複防止 | 中 | 2 | 2日 |
| SPIKE-09 | デバイス切断・スリープ復帰・Bluetooth プロファイル切替の挙動観測 | 中 | 3 | 3日 |
| SPIKE-10 | OS 資格情報ストアへのトークン保存(3 OS) | 低 | 3 | 1日 |

合計目安: 約 7 人週(Wave 間は一部並行可能)

> **番号についての注記**: SPIKE-11/12 は本計画の後追いで追加したため、番号は末尾(11/12)だが **実施順は Wave 1、SPIKE-01/02 の直後** に置く。上表の並び順もその実施順を表しており、ID の昇順ではない。

---

## 3. Wave 1: Windows 縦切りの成立検証(Phase 1A の前提)

### SPIKE-01: WASAPI マイク + Endpoint Loopback 同時取得とタイムスタンプ

**検証する仮説(design.md §5.1, §9.2)**

* Rust `windows` crate で WASAPI capture(マイク)と Endpoint Loopback を同一プロセス内で同時に安定取得できる
* 各フレームに対して `QueryPerformanceCounter` ベースの単調時刻(`host_time_ns`)を紐付けられ、`IAudioCaptureClient::GetBuffer` の `u64 QPCPosition` が実用精度で使える
* discontinuity フラグ(`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`)を検出できる

**検証手順**

1. Rust コンソールアプリで 2 ストリームを開き、各コールバックの QPC 時刻・フレーム数・フラグを CSV へ記録
2. スピーカーからテスト音(880Hz)を再生しつつマイクへ 440Hz を入力し、10 分間録音
3. コールバック間隔のジッタ、バッファ欠落、タイムスタンプの単調性を分析
4. 生 PCM を WAV に落とし、2 トラックが取得できていることを聴感確認

**合否基準**

* 10 分間、両ストリームでサンプル欠落なし(または discontinuity として検出できる)
* QPC タイムスタンプが単調増加し、公称サンプルレートからの累積ずれが説明可能(drift として観測できる)
* CPU 使用率が実用範囲(目安: 1 コアの 10% 未満)

**タイムボックス**: 3日

---

### SPIKE-02: Application Loopback Capture(プロセス指定)

**検証する仮説(design.md §5.1)**

* `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` で指定プロセスツリーの音声のみを取得できる(Rust `windows` crate から呼べる)
* Zoom / Teams デスクトップ版のプロセスツリーを対象に、実際の会議音声を分離取得できる
* 対象プロセス再起動(PID 変化)を検出し、再アタッチできる

**検証手順**

1. C++ 公式サンプル(ApplicationLoopback)をまず動かし、挙動のベースラインを得る
2. 同等処理を Rust `windows` crate で移植する(ここが本丸。`ActivateAudioInterfaceAsync` の Rust からの呼び出しが最大の不確実点)
3. Zoom テスト会議に参加し、(a) Zoom 音声のみ取得されること、(b) 同時再生した YouTube 音声が混入しないことを確認
4. ブラウザ(Chrome)の Meet で同様に試し、他タブ音声の混入有無を記録する(混入する前提の設計だが、実態を把握する)
5. 会議アプリを再起動し、ストリームがどう振る舞うか(無音・エラー・停止)を観測

**合否基準**

* GO: Rust から Zoom/Teams のプロセス限定音声を取得でき、他アプリ音声が混入しない
* CONDITIONAL-GO: C++ サンプル経由(C++/Rust FFI)でのみ動作 → design.md §6.1 の「必要箇所のみ C++」の範囲を確定
* NO-GO 時の代替: Phase 1A/1B とも Endpoint Loopback のみとし、Application Loopback を Phase 2 へ繰り延べ

**タイムボックス**: 3日

---

### SPIKE-11: Audio Endpoint Registry(全デバイスの観測状態追跡)

**実装詳細**: 本節の仮説・検証手順をコーディング着手可能な粒度まで詳細化したものが [spike-windows-11-detail-design.md](spike-windows-11-detail-design.md) にある(`IMMNotificationClient`の`#[implement]`実装、登録/解除、初期スナップショット構築、`EndpointOsEvent`型定義、消費スレッドでの`AudioEvent`変換)。

**検証する仮説(design.md §16.5, §10 セッション状態モデルの前提)**

* `IMMNotificationClient` により、マイク・スピーカー全体の追加/削除/状態変化/プロパティ変化/既定デバイス変更を endpoint ID 単位で追跡できる
* callback はイベントを queue へ積むだけに留め、重い処理(再列挙、COM オブジェクトの解放)は別スレッドへ逃がす構造にできる
* 既定デバイスが存在しない状態(NULL)を `Option<EndpointId>` として正しく表現できる
* この登録・追跡は、列挙されたデバイスごとに個別の状態機械を持たなくても(スナップショットの保持のみで)成立する

**検証手順**

1. 起動時に全 endpoint(capture / render)を列挙し、endpoint ID をキーにした `EndpointSnapshot` レジストリを構築する
2. `IMMNotificationClient` を登録し、callback は `EndpointOsEvent`(`DeviceAdded` / `DeviceRemoved` / `DeviceStateChanged` / `PropertyChanged` / `DefaultChanged`)を queue へ push するだけの実装にする
3. 管理スレッド側で queue を消費し、対象 ID を再取得してレジストリを更新、変更前後を JSONL(`seq` / `event` / `endpoint_id` / `flow` / `old_state` / `new_state` / `observed_at_100ns`)へ記録する
4. 次の操作を行い、記録された JSONL とレジストリの最終状態を確認する: USB マイク/ヘッドセットの抜き差し、Windows 設定でのマイク/スピーカーの無効化・有効化、既定マイク・既定スピーカー・通信用既定デバイスの変更、デバイス名変更、マイク mute 変更、スピーカー mute/音量変更、Bluetooth 接続・切断

**合否基準**

* すべての変化が endpoint ID 単位で JSONL に記録される
* callback 内で再列挙や COM 解放などの重い処理を行っていない(callback 所要時間を計測し、ブロッキングが起きていないことを確認)
* 再列挙後のレジストリが Windows の実状態(サウンド設定・コントロールパネル)と一致する
* 既定デバイスなしの状態を表現でき、異常終了しない
* 同一 endpoint が重複登録されない、削除済み endpoint の callback 登録が残らない、終了時にすべて Unregister される(ハンドル/登録のリークがない)

**タイムボックス**: 3日

---

### SPIKE-12: Capture Rebinding State Machine(再バインド時の epoch 整合性)

**検証する仮説(design.md §16.5 のデバイス binding mode、SPIKE-01/02 の `capture_epoch` の一般化)**

* `Idle → Resolving → Activating → Capturing → Interrupted → Rebinding / WaitingForDevice → Capturing` という共通の FSM で、マイク capture・Endpoint Loopback・Process Loopback の 3 種類のキャプチャソースを扱える
* design.md §16.5 の binding mode(`Fixed selected device` = Pinned / `Follow system default` = FollowDefault)を、既定デバイス変更時の挙動として明確に分離できる
* 再バインド前後で、SPIKE-01/02 の `capture_epoch` を全ストリーム共通の `stream_epoch` として一般化しても、旧世代と新世代のフレームを混在させずに扱える
* Process Loopback は対象 endpoint の状態変化(既定スピーカー変更など)では再起動すべきでなく、対象プロセスの終了/再起動と Audio Service 側の切断のみを再バインド対象にできる

**検証手順**

1. SPIKE-11 のレジストリ更新イベントを入力として、実 WASAPI を呼ばない Fake Worker で `CaptureBinding` の FSM と reducer(`reduce(state, event) -> effects`)を実装する
2. 次の 4 シナリオをイベント列テストとして自動化する: (a) FollowDefault マイクでの既定変更(A→B)で A 停止完了後に B へ接続すること、(b) Pinned マイクの切断で `WaitingForDevice` へ遷移し既定デバイスへ自動フォールバックしないこと・再接続で復帰すること、(c) FollowDefault スピーカー Loopback での既定変更で旧 stream 停止→新 endpoint で再開すること、(d) Process Loopback で既定スピーカー変更/対象アプリの出力先変更では継続し、対象アプリ再起動時のみ PID 再探索・epoch 更新が起きること
3. `operation_id` / `stream_epoch` の不一致による stale event の棄却、Stop 完了前に新規 Start を発行しない、といった不変条件を property test で検証する
4. Fake Worker を実 WASAPI ワーカーへ差し替え、SPIKE-01/02/09 の環境で同シナリオを実機実行し、`RecoveryTiming`(障害検出時刻・旧 stream 停止時刻・新 stream Activate 時刻・最初の新 PCM 到着時刻)を計測する

**合否基準**

* 旧 epoch のフレームが新 epoch 開始後に出力されない(全フレームに `endpoint_id` と `stream_epoch` が記録され、境界で混線が起きない)
* FollowDefault と Pinned とで、既定デバイス変更時の挙動が design.md §16.5 の binding mode どおりに異なる
* Pinned デバイス切断時に `WaitingForDevice` へ遷移し、別デバイスへ勝手にフォールバックしない。再接続時に復帰する
* Process Loopback が endpoint 側の変更だけでは不要に再起動しないことを実機(Zoom/Teams)で確認できる
* 復旧不能時に無限リトライしない、callback スレッド内で stop/join/Activate を行わない
* `AUDCLNT_E_DEVICE_INVALIDATED` と session disconnect の理由(`IAudioSessionEvents::OnSessionDisconnected`)を記録できる

**タイムボックス**: 4日

**設計の全体像**: SPIKE-11/12 が検証する三層モデル(endpoint観測状態・選択ポリシー・録音bindingのFSM)、および Observation → Admission → Decision → Effect Execution への発展形は、[audio-device-state-architecture.md](audio-device-state-architecture.md) に設計書として整理してある。Fake Worker 実装(検証手順1〜3)はこの設計書の型定義・reducer をそのまま出発点にできる。

**備考**: SPIKE-09(デバイス変更・スリープ・Bluetooth の挙動観測)とは役割が異なる。SPIKE-09 は「Windows から実際にどんな通知・エラーが飛んでくるか」の実地観測であり、SPIKE-11/12 は「観測したイベントを状態機械としてどう安全に処理するか」の検証にあたる。SPIKE-12 の FSM とハーネスは SPIKE-09 の実機シナリオでそのまま利用できるため、SPIKE-09 の実施前に完了しておくのが望ましい。

---

### SPIKE-03: 共通タイムライン整列・drift 補正(疑似音源)

**検証する仮説(design.md §11, §19.2)**

* 異なるサンプルレート・異なるコールバック周期・意図的 clock drift を持つ 2 疑似音源を、20ms スロットの共通タイムラインへ整列できる
* resampler 比率の緩やかな調整で、聴感上の歪みなく drift を吸収できる
* 2 時間相当のシミュレーションで同期差 100ms 以内(§3.2 の品質ゴール)を達成できる

**検証手順**

1. OS API 非依存の疑似キャプチャ(440Hz / 880Hz、+50ppm / −50ppm の drift、ランダム packet loss、discontinuity)を実装
2. タイムライン整列 + silence 補完 + drift 補正のプロトタイプを Rust で実装(rubato 等の resampler crate を評価)
3. 2 時間分を高速実行し、出力 2 トラックの長さ一致・位相ずれを自動計測
4. drift 補正の有無での同期差を比較し、補正パラメータ(比率上限・観測窓)の目安を得る

**合否基準**

* 2 時間相当で両トラック長が完全一致、相互相関による同期差測定で 100ms 以内(目標 20ms 以内)
* packet loss / discontinuity 混入時もセグメント境界が壊れない
* 20ms フレーム処理がリアルタイムの 10 倍速以上で回る(CPU 余裕の確認)

**タイムボックス**: 4日

**備考**: このスパイクの成果はテストハーネス(§19.2)としてほぼそのまま再利用価値があるため、例外的に品質高めに書いてよい。

---

### SPIKE-04: Opus セグメントの atomic commit とクラッシュ復旧

**検証する仮説(design.md §11.1, §12.2)**

* Rust から Opus(Ogg コンテナ、48kHz mono)で 30 秒セグメントをエンコードできる(`opus` / `ogg` crate、または audiopus)
* 「.partial 書き込み → flush → fsync → SHA-256 → atomic rename → SQLite 登録」の手順が Windows(NTFS)で期待どおり動く
* 各段階での強制 kill 後、再起動スキャンで孤立ファイルを正しく分類・復旧できる

**検証手順**

1. 疑似 PCM を 30 秒ごとに Opus セグメント化して書き出すループを実装
2. 書き込みシーケンスの各ステップ(partial 書き込み中 / fsync 前 / rename 前 / DB 登録前)で `taskkill /F` する自動テストを作成
3. 再起動スキャンが「破棄すべき partial」「DB 未登録の完成ファイル」を正しく処理することを確認
4. セグメントサイズを実測し、ビットレート設定(例: 32kbps)での 2 時間分の容量を見積もる

**合否基準**

* どのタイミングで kill しても、確定済みセグメントが破損しない・失われない
* 未確定損失が 30 秒 + エンコーダバッファ以内(§3.2)
* Ogg/Opus ファイルが ffprobe / 一般プレイヤーで正常に再生できる

**タイムボックス**: 2日

**設計の全体像**: 本スパイクが検証する「atomic commit + クラッシュ復旧」は、[audio-device-state-architecture.md](audio-device-state-architecture.md) §7(Effectの完了保証と冪等性)で一般化した Durable Effect Ledger モデルの具体例にあたる。特に §7.4.3(設定・JSON・summaryの保存)・§7.6〜7.8(録音保存プロトコル・append再送・WAV遅延生成)は、本スパイクの検証手順・合否基準をそのまま拡張できる設計になっている。

---

### SPIKE-05: Tauri 2 常駐と録音ライフサイクル分離

**検証する仮説(design.md §6, §14)**

* Tauri 2 でウィンドウを閉じてもトレイ常駐でバックエンド(録音スレッド)が継続する
* Rust 側の状態(レベルメーター値、録音時間)を UI へ低負荷でストリーミングできる(イベント emit の頻度・負荷)
* WebView の再オープン時に状態を復元表示できる

**検証手順**

1. Tauri 2 + Vue 3 の雛形に、SPIKE-01 のキャプチャ(またはダミー音源)を組み込む
2. トレイアイコン・録音中インジケータ・ウィンドウ閉鎖→再表示を実装
3. レベルメーター更新(30fps 想定)を 1 時間流し、UI 側・Rust 側のメモリと CPU を観測
4. Windows でのウィンドウ閉鎖時のプロセス生存、OS 再起動時の挙動を確認

**合否基準**

* ウィンドウ閉鎖後も録音(音声取得+書き込み)が途切れない
* 1 時間の常駐でメモリリークの兆候がない(RSS が安定)
* レベルメーター更新でフレーム落ち・IPC 詰まりが起きない

**タイムボックス**: 2日

---

## 4. Wave 2: macOS / Linux の方式成立確認とアップロード

### SPIKE-06: macOS ScreenCaptureKit の audio + microphone 同時取得

**検証する仮説(design.md §5.2)**

* macOS 15 で同一 `SCStream` から `SCStreamOutputType.audio`(system audio)と `.microphone` を同時取得できる
* `SCContentFilter` でアプリ単位(Zoom.app 等)の音声に限定できる
* 両出力の `CMSampleBuffer` タイムスタンプを共通のホスト単調時刻(mach absolute time 系)へ変換して整列に使える
* Swift → C ABI → Rust のブリッジで PCM とタイムスタンプを受け渡せる

**検証手順**

1. Swift 単体のコマンドラインツールで SCStream を構成し、権限プロンプト(Microphone / Screen & System Audio Recording)の発火と拒否時の挙動を確認
2. Zoom を対象にした SCContentFilter で会議音声を取得し、他アプリ音声(Music.app 再生)が混入しないことを確認
3. audio / microphone 両出力のタイムスタンプの時刻系を特定し、換算式を検証
4. C ABI 経由で Rust 側へフレームを渡す最小ブリッジを作り、スループットとコピーコストを測定
5. 権限取消(録音中に System Settings で OFF)時のエラー通知のされ方を観測

**合否基準**

* system audio と microphone が別ストリームとして取得でき、アプリフィルタが機能する
* 両者のタイムスタンプ差から相対整列が可能(SPIKE-03 のタイムラインへ載せられる)
* 権限拒否・取消を検出してエラーとして扱える(サイレント無音にならない、なる場合は検出策を確立)

**タイムボックス**: 4日

**備考**: 開発には macOS 15 実機と Apple Developer 署名が必要。未署名時の TCC 挙動の差異も記録する。

---

### SPIKE-07: Linux PipeWire の playback node 取得と monitor フォールバック

**検証する仮説(design.md §5.3)**

* pipewire-rs で registry を列挙し、アプリの playback stream node を識別できる(Zoom / Chrome の node 名・プロパティの実態確認)
* 特定アプリの playback node を直接キャプチャできる、または `stream.capture.sink` で sink monitor を取得できる
* node の消失・再生成(アプリ再起動)を registry イベントで検出できる

**検証手順**

1. Ubuntu 24.04(Wayland / PipeWire)で `pw-dump` により Zoom・Chrome(Meet)の node 構造とプロパティを調査
2. pipewire-rs でマイク source capture と sink monitor capture を同時実行し、タイムスタンプ取得方法(`pw_time`)を確認
3. 特定アプリ node に接続するキャプチャを試み、可否と制約(接続タイミング、node 出現前の扱い)を記録
4. 会議アプリ再起動 → node 再生成 → 再接続の流れを実装して観測
5. 同一手順を Fedora でも実行し、差異を記録。加えて Flatpak 版 Chrome で portal 制約の有無を確認

**合否基準**

* GO: アプリ単位の playback node キャプチャ + monitor フォールバックの両方が成立
* CONDITIONAL-GO: monitor フォールバックのみ成立 → §5.3 の選択方針を「monitor 主・node 直結は best effort」へ修正
* node 消失検出から silence 挿入への繋ぎに必要なイベントが取得できる

**タイムボックス**: 4日

---

### SPIKE-08: チャンクアップロードと Idempotency-Key

**検証する仮説(design.md §13)**

* `UploadAdapter` trait の契約(create → segment PUT → finalize)で、順序不定・重複送信・途中再開を安全に扱える
* Idempotency-Key(`{session_id}:{track}:{sequence}`)+ Content-SHA256 でサーバ側重複排除が成立する

**検証手順**

1. 推奨 API 契約(§13.1)を実装したモックサーバ(Rust axum 等)を作成。ランダムに 5xx / 429 / timeout / 「受領済みなのに応答喪失」を注入する
2. reqwest + exponential backoff + jitter の Upload Worker プロトタイプを実装
3. 100 セグメントのセッションを、故障注入率 30% で完走させ、サーバ側の登録がちょうど 100 件であることを確認
4. アップロード途中でクライアントを kill → 再起動 → SQLite の状態から再開できることを確認

**合否基準**

* 故障注入下でも重複登録 0 件・欠落 0 件で finalize まで到達
* 再起動後の再開で二重送信が発生しても API 上は冪等に処理される

**タイムボックス**: 2日

---

## 5. Wave 3: 障害系と周辺技術

### SPIKE-09: デバイス変更・スリープ・Bluetooth の挙動観測(Windows 中心)

**検証する仮説(design.md §16, §23)**

* マイク切断・既定デバイス変更・スリープ復帰・Bluetooth の A2DP↔HFP プロファイル切替を、WASAPI のイベント/エラーとして検出できる
* format change(サンプルレート変更)を検出して再初期化できる

**検証手順**

1. SPIKE-01 のハーネスへイベントログを追加し、次の操作を実施して観測結果を表にまとめる:
   USB マイク抜去 → 再接続 / 既定出力デバイス切替 / スリープ → 復帰 / BT ヘッドセットでマイク使用開始(HFP 切替)
2. 各イベントで (a) 何のエラー・通知が来るか、(b) ストリームは自動復帰するか、(c) タイムスタンプは連続するか、を記録
3. 検出 → silence 挿入 → 再初期化の最小実装で録音が継続できることを確認

**合否基準**

* すべての障害イベントが「検出可能」に分類できる(検出不能なサイレント無音が存在しない、または回避策がある)
* 障害を跨いでもタイムラインが破綻しない(SPIKE-03 の整列器が silence で埋めて長さ一致を保つ)

**タイムボックス**: 3日

---

### SPIKE-10: OS 資格情報ストアへのトークン保存

**検証する仮説(design.md §12.4)**

* `keyring` crate(または同等)で Windows Credential Manager / macOS Keychain / Linux Secret Service へトークンを保存・取得できる
* Linux でヘッドレス・Secret Service 不在環境(最小構成デスクトップ)のフォールバック方針を決められる

**検証手順**

1. 3 OS で save / load / delete の round-trip を確認
2. Linux で gnome-keyring 不在時のエラーを観測し、フォールバック(拒否 or 暗号化ファイル)の判断材料を得る

**合否基準**: 3 OS の標準デスクトップ環境で round-trip 成功。Linux 例外系の方針が決まる

**タイムボックス**: 1日

---

## 6. スケジュールと依存関係

```mermaid
flowchart LR
    S01[SPIKE-01 WASAPI同時取得] --> S02[SPIKE-02 App Loopback]
    S01 --> S11[SPIKE-11 Endpoint Registry]
    S01 --> S12[SPIKE-12 Rebinding FSM]
    S02 --> S12
    S11 --> S12
    S12 --> S09[SPIKE-09 障害系観測]
    S01 --> S09
    S03[SPIKE-03 タイムライン] --> S04[SPIKE-04 セグメント確定]
    S01 --> S05[SPIKE-05 Tauri常駐]
    S04 --> S08[SPIKE-08 アップロード]
    S06[SPIKE-06 macOS SCK]
    S07[SPIKE-07 PipeWire]
    S10[SPIKE-10 資格情報]
```

* SPIKE-01 と SPIKE-03 は独立しており、**2 名いれば初日から並行可能**(1 名なら 01 → 03 の順)
* SPIKE-11 は Windows Core Audio のデバイス列挙 API のみに依存するため、SPIKE-01 と並行着手できる。SPIKE-12 は Fake Worker で FSM 自体を先に作れるため SPIKE-01/02 の完了を待たずに設計・実装を始められるが、実 WASAPI ワーカーへの差し替え検証(検証手順4)は SPIKE-01/02 完了後に行う
* SPIKE-06 / 07 は Wave 1 と独立のため、環境(macOS 15 実機、Ubuntu 24.04 実機)さえあれば前倒し可能
* **Phase 1A 着手の Go/No-Go 判断は SPIKE-01〜05, 11, 12 完了時点** で行う(目安: 開始から 3〜3.5 週)
* SPIKE-11/12 を通してから SPIKE-03(共通タイムライン)へ進むことで、後から録音基盤(epoch 境界の扱い)を作り直すリスクを下げる
* macOS(Phase 1C)/ Linux(Phase 1D)の計画確定は SPIKE-06 / 07 の結果を待つ

---

## 7. Go/No-Go 判断基準(全体)

| 判断ポイント | GO 条件 | NO-GO 時のアクション |
|---|---|---|
| Phase 1A 着手 | SPIKE-01, 03, 04, 05, 11, 12 がすべて GO | 技術スタック(§6)の再検討。特に SPIKE-05 NO-GO なら Tauri 以外のシェルを評価 |
| Application Loopback 採用 | SPIKE-02 が GO または CONDITIONAL-GO | Endpoint Loopback のみで Phase 1B を再定義し、§5.1 / §20.2 を修正 |
| デバイス自動再バインド(§16.5 Follow system default 含む) | SPIKE-11, 12 が GO | Phase 1 は §16.5 の既定どおり `Fixed selected device` のみを提供し、`Follow system default` / `Ask before switching` は実験的設定または Phase 2 へ繰延べ。Pinned デバイスの切断検出・待機・復帰(§16.1)のみは最低限の縮退スコープとして維持する |
| macOS を Phase 1C で実施 | SPIKE-06 が GO | macOS 対応時期の再計画、または CoreAudio tap 等の代替方式スパイクを追加 |
| Linux を Phase 1D で実施 | SPIKE-07 が GO または CONDITIONAL-GO | 対応ディストリの絞り込み、または monitor 専用へ §5.3 を縮退 |
| 品質ゴール(同期 100ms) | SPIKE-03 の実測 + SPIKE-01/06/07 のタイムスタンプ精度で達成見込み | §3.2 の目標値見直し、または drift 補正方式の追加スパイク |

---

## 8. 準備物チェックリスト

* [ ] Windows 11 実機(build 20348 以降 — Application Loopback 要件)
* [ ] macOS 15 実機 + Apple Developer アカウント(署名・TCC 検証用)
* [ ] Ubuntu 24.04(Wayland / PipeWire)実機または VM(音声パススルー可能なもの)
* [ ] Fedora Workstation 環境
* [ ] USB マイク、Bluetooth ヘッドセット(HFP/A2DP 切替検証用)
* [ ] Zoom / Teams / Chrome(Meet)のテスト用アカウントとテスト会議
* [ ] 検証結果を置くリポジトリ `spikes/` ディレクトリと RESULT.md テンプレート

SPIKE-11/12 は上記の USB マイク・Bluetooth ヘッドセット・Windows 11 実機のみで検証可能であり、追加の準備物は不要。

---

## 9. 検討中の追加候補(未着手)

design.md・本計画・audio-device-state-architecture.md を通読した結果、既存の SPIKE-01〜12 のどれとも重複しない、かつ技術的に掘る価値があると判断した候補を記録する。優先度・タイムボックス・検証手順は未確定であり、着手する場合は改めて仮説・検証手順・合否基準を詰めてから ID(SPIKE-13〜)を割り当てる。

| 候補 | 技術的な論点 | 既存スパイクとの関係 |
|---|---|---|
| SQLite(session-store)の並行アクセス・クラッシュ安全性 | WAL モードでの複数スレッド同時書き込み、Windows NTFS 上での busy-timeout 挙動、強制終了時の DB 破損可能性。design.md §7/§12 の Segment Writer・Upload Worker・Session Orchestrator が同一 DB へ書き込む構成の中核部分 | SPIKE-04(Opus ファイルの原子性)とは対象が異なり、どのスパイクにも含まれていない |
| Modern Standby(S0ix)と classic sleep(S3)の違い | Windows 11 の新しいラップトップの多くは Modern Standby がデフォルト。ネットワーク・一部プロセスを制限付きで動かし続けるなど classic sleep と挙動が大きく異なり、「録音中は必ず継続する」(design.md §3.2)に直結する | SPIKE-09(スリープ→復帰)の検証手順を明示的に拡張すれば足りる可能性が高い |
| 企業ネットワーク環境でのアップロード(プロキシ認証・TLS インスペクション) | NTLM/Kerberos プロキシ認証、企業の TLS インスペクション(独自ルート CA)下での挙動 | SPIKE-08 はモックサーバでの故障注入(5xx/429/timeout)のみを対象としており、実際の企業ネットワーク条件は未検証 |
| Windows Credential Manager の実運用制限 | Generic Credential のサイズ上限(概ね 2.5KB 程度)、企業のローミングプロファイル・グループポリシーによる資格情報ストアへのアクセス制限 | SPIKE-10 は 3 OS での round-trip 確認に留まる |
| WebView2 ランタイムの企業環境での可用性 | Tauri が内部で依存する WebView2 の自動更新(Evergreen)が、企業管理された Windows 機ではグループポリシーで無効化・特定バージョン固定されている場合がある | SPIKE-05 は「WebView2 が動く前提」でトレイ常駐のみを検証しており、WebView2 自体の可用性は対象外 |
| コード署名なしバイナリに対する SmartScreen/AV(EDR)の反応 | WASAPI ループバック取得・プロセス列挙・資格情報保存の組み合わせは、行動ベースの AV/EDR から見て「スパイウェア的挙動」に見えやすい。VirusTotal へのアップロード等の軽量確認で早期に実態を把握できる | 技術検証というより配布可否に関わるリスクだが、実装が進んでから気づくと手戻りが大きい |

優先度の目安(技術的な深掘り価値・design.md への影響度から): SQLite並行性 > Modern Standby > 企業ネットワーク環境 > WebView2可用性 > Credential Manager制限・SmartScreen/AV反応。
