# Windows スパイク(SPIKE-01 / SPIKE-02) 内部詳細設計書

* **文書ステータス**: Draft v0.1
* **作成日**: 2026-07-07
* **上位文書**: [design.md](design.md)、[spike-plan.md](spike-plan.md)
* **対象スパイク**:
  * SPIKE-01: WASAPI マイク + Endpoint Loopback 同時取得とタイムスタンプ
  * SPIKE-02: Application Loopback Capture(プロセス指定)
* **目的**: spike-plan.md の仮説・検証手順・合否基準を、そのままコーディング着手できる粒度(モジュール構成、型定義、関数シグネチャ、Windows API 呼び出し順序、CLI仕様、出力ファイル形式)まで詳細化する

---

## 1. 前提と対象範囲

### 1.1 対象外

本文書は次を対象**外**とする。

* SPIKE-03(共通タイムライン整列)、SPIKE-04(セグメント確定)、SPIKE-05(Tauri常駐) — 別文書で扱う
* プロダクションコードとしての品質担保(spike-plan.md §1.1の原則どおり使い捨てコードとする)
* UI。すべてCLIコンソールアプリとする

### 1.2 SPIKE-01とSPIKE-02の関係

SPIKE-02はSPIKE-01のキャプチャ基盤(タイムスタンプ処理、CSV/WAV出力、スレッドモデル)を流用し、ストリーム取得元だけを「既定レンダーデバイスのEndpoint Loopback」から「指定プロセスのApplication Loopback」に差し替える。そのため、両スパイクで共有できる部分を`spike-common`クレートへ切り出し、SPIKE-01実装時点でSPIKE-02が乗れる形にしておく。

### 1.3 成果物

```text
spikes/
├─ Cargo.toml                          # workspace定義
├─ spike-common/                       # 共有ユーティリティ
├─ spike-01-wasapi-dual-capture/       # SPIKE-01バイナリ
├─ spike-02-app-loopback/              # SPIKE-02バイナリ
└─ RESULT_TEMPLATE.md                  # spike-plan.md §1.2のテンプレート
```

各スパイク完了後、`spikes/spike-0X-*/RESULT.md` に結果を記録する(本文書はコードの設計のみを扱い、RESULT.mdの記入は対象外)。

---

## 2. 全体構成(ワークスペース)

```toml
# spikes/Cargo.toml
[workspace]
resolver = "2"
members = [
    "spike-common",
    "spike-01-wasapi-dual-capture",
    "spike-02-app-loopback",
]

[workspace.package]
edition = "2021"
publish = false

[workspace.dependencies]
windows = { version = "0.58", features = [
    "Win32_Media_Audio",
    "Win32_Media_Multimedia",
    "Win32_Media_KernelStreaming",
    "Win32_System_Com",
    "Win32_System_Com_StructuredStorage",
    "Win32_System_Variant",
    "Win32_System_Threading",
    "Win32_System_Diagnostics_ToolHelp",
    "Win32_Foundation",
    "Win32_System_Performance",
    # 実機検証で判明(§7): これがないと#[implement(...)]用の_Implトレイト
    # 自体が生成されない。SPIKE-02のCompletionHandler(§5.5)に必須。
    "implement",
] }
thiserror = "1"
anyhow = "1"
clap = { version = "4", features = ["derive"] }
csv = "1"
hound = "3"
crossbeam-channel = "0.5"
sysinfo = "0.31"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

`spike-02-app-loopback/Cargo.toml`には、上記に加えて`windows-core = "0.58"`(`windows`と同じバージョン)を直接の依存として追加する。`#[implement(...)]`マクロが生成するコードは`windows_core::...`という直接のクレートパスを参照するため、`windows`経由の再エクスポート(`windows::core`)だけでは解決できない(§7・§5.5参照)。

> **実装時の確認ポイント**: `windows` crateはバージョンによりfeatureフラグ名・型の所属モジュールが変わることがある。`cargo add windows --dry-run`後に`cargo doc --open -p windows`で`ActivateAudioInterfaceAsync`、`AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`等の実在パスを確認してから着手する(§7で既知の不確実性として再掲)。
>
> **Windows実機がなくても`cargo check`で検証できる**: `windows`クレートは`#![cfg(windows)]`でクレート全体がガードされているため、Linux上では素の`cargo check`は失敗する(クレートが空扱いになる)。`rustup target add x86_64-pc-windows-gnu`を実行すれば、`cargo check --target x86_64-pc-windows-gnu`でリンクを伴わない型チェックまでは実行できる(実際にこのワークスペースの雛形をこの方法で検証し、§5.5の`_Impl`ターゲット誤りなど複数の実装上の誤りを実装前に発見できた)。ビルド・実行・実際のWASAPI/Process Loopback動作検証にはWindows 11実機が必要な点は変わらない。

---

## 3. 共通基盤(`spike-common`)

### 3.1 モジュール構成

```text
spike-common/
└─ src/
   ├─ lib.rs
   ├─ com_guard.rs      # CoInitializeEx RAIIガード
   ├─ timestamp.rs       # QPC/100ns -> ns 変換、単調性チェック
   ├─ frame_record.rs    # CapturedFrameRecord定義
   ├─ csv_log.rs         # フレームメタデータCSV書き出し
   ├─ wav_writer.rs       # 生PCM -> WAVファイル書き出し(聴感確認用)
   ├─ mmcss.rs           # スレッド優先度(Pro Audio)設定
   ├─ error.rs           # SpikeError
   └─ analyze.rs         # CSV解析・合否判定支援
```

### 3.2 `CapturedFrameRecord`

WASAPIコールバック1回(=1バッファ取得)ごとに記録する行データ。CSVの1行、およびスレッド間チャネルのメッセージ単位を兼ねる。

```rust
// spike-common/src/frame_record.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamId {
    Mic,
    EndpointLoopback,
    ProcessLoopback,
}

#[derive(Debug, Clone)]
pub struct CapturedFrameRecord {
    pub stream: StreamId,
    /// `WaitForMultipleObjects`が1回戻るごとにインクリメントされる連番。
    /// 同一wakeで複数パケットを排出した場合、それらのレコードは同じ値を持つ。
    /// wakeジッタの計算はこの単位で行う(§4.9参照)。
    pub wake_seq: u64,
    /// パケット(`GetBuffer`呼び出し1回)ごとの連番(0始まり、ストリーム内で単調増加)
    pub packet_seq: u64,
    /// `IAudioCaptureClient::GetBuffer`が返す`pu64QPCPosition`を100ns単位に
    /// 変換した値。**プロセス・ストリームをまたいで共通のQPCクロックドメイン**に
    /// 属する(QueryPerformanceCounterの生値と同じ基準)。`timestamp_error`が
    /// trueの場合はこの値を信頼しない。
    pub capture_qpc_100ns: u64,
    /// `WaitForMultipleObjects`が戻った時点で別途`QueryPerformanceCounter`を
    /// 呼んで取得したQPC値(100ns単位)。共通タイムライン変換には使わない。
    ///
    /// 【解釈の訂正】`capture_qpc_100ns`は「バッファ先頭フレームが記録された時刻」
    /// であり、`wake_qpc_100ns - capture_qpc_100ns`(= `packet_age_at_wake_100ns`。
    /// §4.9の`compute_packet_age_at_wake`参照)を単純に「スレッドのスケジューリング
    /// 遅延」と呼ぶのは不正確である。この差分には、バッファ内蓄積時間・
    /// オーディオエンジンの通知周期・スレッド起床遅延が混在しており、
    /// 分離できない。「起床時点で観測されたパケットの経過時間(観測遅延)」
    /// として扱い、スケジューリング遅延と断定しない。
    pub wake_qpc_100ns: u64,
    /// `pu64DevicePosition`。ストリーム先頭からの累積オーディオフレーム数
    /// (サンプル数ではなくフレーム数。チャンネル数に依存しない)
    pub device_position_frames: u64,
    /// このパケットのフレーム数
    pub frame_count: u32,
    /// `IAudioCaptureClient::GetBuffer`が返す生フラグ
    pub raw_flags: u32,
    pub discontinuity: bool,
    pub silent: bool,
    pub timestamp_error: bool,
    /// キャプチャの世代番号。SPIKE-02でプロセス再アタッチが発生するたびに
    /// インクリメントする。旧世代のデータが新世代のバッファ/CSVへ混入しないか
    /// を検証するために使う(§5.7参照)。
    pub capture_epoch: u64,
    /// SPIKE-02でのみ使用。取得元プロセスのPID(Endpoint Loopback/Micでは`None`)
    pub target_pid: Option<u32>,
}

impl CapturedFrameRecord {
    pub const FLAG_DATA_DISCONTINUITY: u32 = 0x1; // AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY
    pub const FLAG_SILENT: u32 = 0x2;              // AUDCLNT_BUFFERFLAGS_SILENT
    pub const FLAG_TIMESTAMP_ERROR: u32 = 0x4;     // AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR

    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        stream: StreamId,
        wake_seq: u64,
        packet_seq: u64,
        wake_qpc_100ns: u64,
        device_position_frames: u64,
        capture_qpc_100ns: u64,
        frame_count: u32,
        raw_flags: u32,
        capture_epoch: u64,
        target_pid: Option<u32>,
    ) -> Self {
        Self {
            stream,
            wake_seq,
            packet_seq,
            capture_qpc_100ns,
            wake_qpc_100ns,
            device_position_frames,
            frame_count,
            raw_flags,
            discontinuity: raw_flags & Self::FLAG_DATA_DISCONTINUITY != 0,
            silent: raw_flags & Self::FLAG_SILENT != 0,
            timestamp_error: raw_flags & Self::FLAG_TIMESTAMP_ERROR != 0,
            capture_epoch,
            target_pid,
        }
    }
}
```

`host_time_ns`(design.md §9.2)は、`timestamp_error`でないレコードについて`capture_qpc_100ns * 100`をそのまま使う。この値はストリーム間・プロセス内で共通のQPCクロックドメインに属するため、ストリーム開始時刻を加算する必要はない(§3.3参照)。SPIKE-01の合否基準(タイムスタンプの単調性・drift観測可能性)は、この値の系列を検証することで判定する。

### 3.3 タイムスタンプ変換

```rust
// spike-common/src/timestamp.rs

/// QueryPerformanceFrequencyで取得したカウンタ周波数。
/// システム起動時に固定されるが、基盤タイマー実装により環境ごとに値が
/// 異なりうるため、定数(10MHz)扱いにせず必ず実行時に取得する。
pub struct QpcClock {
    freq_hz: u64,
}

impl QpcClock {
    pub fn query() -> windows::core::Result<Self> {
        // windows::Win32::System::Performance::QueryPerformanceFrequency
        // freq_hzへ格納する。値そのものをログ/summary.jsonへ記録し、
        // 環境依存を後から確認できるようにする。
    }

    pub fn now_100ns(&self) -> u64 {
        // QueryPerformanceCounter() の値を 100ns 単位へ換算する。
        // count * 10_000_000 は長時間稼働環境(countもfreq_hzも大きくなり得る)で
        // u64乗算がオーバーフローし得るため、中間計算はu128で行う。
        // let count: u64 = ...; // QueryPerformanceCounter()
        // ((count as u128 * 10_000_000u128) / self.freq_hz as u128) as u64
    }

    pub fn hundred_ns_to_ns(v: u64) -> u64 {
        v.saturating_mul(100)
    }
}

/// 単調性チェック用。逆行を検出したら呼び出し側へ知らせる。
/// `timestamp_error == true`のレコードはチェック対象から除外する。
pub struct MonotonicGuard {
    last: Option<u64>,
}

impl MonotonicGuard {
    pub fn check(&mut self, value_100ns: u64) -> bool {
        let ok = self.last.map_or(true, |last| value_100ns >= last);
        self.last = Some(value_100ns);
        ok
    }
}
```

**QPC時刻の起点についての訂正**: `IAudioCaptureClient::GetBuffer`が返す`pu64QPCPosition`は、「ストリームやデバイスのアクティベート時刻を起点とした相対値」ではなく、`QueryPerformanceCounter`の生値を100ns単位に変換したものであり、**同一PC上のすべてのストリーム・プロセスに共通するQPCクロックドメイン**に属する(Microsoft Learnの`IAudioCaptureClient::GetBuffer`解説が示す変換式に準拠)。

したがって、共通タイムラインへ配置する際も次の式で十分であり、ストリーム開始時刻をオフセットとして加算する必要は**ない**。

```text
host_time_ns = capture_qpc_100ns * 100   // timestamp_error == false のときのみ有効
```

マイクとEndpoint Loopbackの2ストリームが別々の`Start()`タイミングを持っていても、両ストリームの`capture_qpc_100ns`は同じクロックドメイン上の値なので、そのまま引き算すれば経過時間や相対オフセットが求まる。旧設計にあった`StreamOrigin`(ストリーム開始時のQPCを起点として加算する構造)は、この生QPC値へさらに加算することで実際には二重加算となり誤った時刻を生成するため、本設計では**廃止**する。SPIKE-03の共通タイムライン整列でも、この生QPC値をそのまま入力として使う前提とする。

### 3.4 CSV出力

```rust
// spike-common/src/csv_log.rs

pub struct FrameCsvWriter {
    writer: csv::Writer<std::fs::File>,
}

impl FrameCsvWriter {
    pub fn create(path: &std::path::Path) -> anyhow::Result<Self> {
        // ヘッダー: stream,wake_seq,packet_seq,capture_qpc_100ns,wake_qpc_100ns,
        //           device_position_frames,frame_count,raw_flags,
        //           discontinuity,silent,timestamp_error,capture_epoch,target_pid
    }

    pub fn write(&mut self, record: &CapturedFrameRecord) -> anyhow::Result<()>;
    pub fn flush(&mut self) -> anyhow::Result<()>;
}
```

CSVは分析用途に徹し、音声サンプルは含めない(別途WAVへ出す)。

### 3.5 WAV出力

聴感確認用(spike-plan.md SPIKE-01 手順4「生PCMをWAVに落とし聴感確認」)。

```rust
// spike-common/src/wav_writer.rs

pub struct PcmWavWriter {
    inner: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
}

impl PcmWavWriter {
    pub fn create_f32_mono(path: &std::path::Path, sample_rate: u32) -> anyhow::Result<Self>;
    pub fn create_from_format(path: &std::path::Path, channels: u16, sample_rate: u32) -> anyhow::Result<Self>;
    pub fn write_samples(&mut self, samples: &[f32]) -> anyhow::Result<()>;
    pub fn finalize(self) -> anyhow::Result<()>;
}
```

デバイスのミックスフォーマット(`WAVEFORMATEX`)をそのまま使うため、チャンネル数・サンプルレートは実行時にデバイスから取得した値で初期化する。

### 3.6 COM初期化ガード

```rust
// spike-common/src/com_guard.rs

/// スレッドごとに1つ生成する。Drop時にCoUninitializeする。
pub struct ComApartment;

impl ComApartment {
    /// オーディオキャプチャスレッドは MTA (COINIT_MULTITHREADED) で初期化する。
    /// メッセージポンプを持たないバックグラウンドスレッドで STA を使うと
    /// マーシャリング関連の問題が起きやすいため MTA を既定とする。
    pub fn new_mta() -> windows::core::Result<Self> {
        // CoInitializeEx(None, COINIT_MULTITHREADED)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // CoUninitialize()
    }
}
```

### 3.7 エラー型

```rust
// spike-common/src/error.rs

#[derive(thiserror::Error, Debug)]
pub enum SpikeError {
    #[error("COM/WASAPI呼び出し失敗: {0}")]
    Com(#[from] windows::core::Error),

    #[error("対象デバイスが見つかりません: {0}")]
    DeviceNotFound(String),

    #[error("対象プロセスが見つかりません: {0}")]
    ProcessNotFound(String),

    #[error("ActivateAudioInterfaceAsync がタイムアウトしました({0:?})。オプションのハードタイムアウトモード使用時のみ発生する")]
    ActivationTimeout(std::time::Duration),

    #[error("ActivateAudioInterfaceAsync の完了通知チャネルが送信側切断のまま閉じられました")]
    ActivationChannelClosed,

    #[error("ActivateAudioInterfaceAsync がエラーを返しました: hresult=0x{0:08X}")]
    ActivationFailed(u32),

    #[error("未対応のフォーマットです: {0}")]
    UnsupportedFormat(String),

    #[error("Process Loopback未対応の可能性があるOSビルドです: build={build}")]
    UnsupportedOsBuild { build: u32 },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

### 3.8 スレッド/チャネル共通パターン

両スパイクとも「キャプチャスレッド(1本/ストリーム) → 集約ライタースレッド(1本)」という構成を共有する。

**チャネルのバックプレッシャー方針(P1)**: `CaptureEvent`のチャネルは`crossbeam_channel::bounded(queue_capacity)`で作成する(`unbounded`にしない)。容量の目安は「1秒分のパケット数」程度(例: 20msフレームなら50)とし、CLIの`--queue-capacity`(既定値をこの目安にする)で調整可能にする。満杯時は`tx.try_send`が`Full`を返すので、その場でブロックせず`pipeline_drop_counter`(§4.4)をインクリメントしてキャプチャを継続する(§4.6のAggregator側I/O遅延がWASAPIコールバックへ波及するのを防ぐ)。`summary.json`には少なくとも次を記録する。

```json
{
  "queue_capacity": 256,
  "queue_high_watermark": 17,
  "queue_drop_packets": 0
}
```

`queue_high_watermark`は集約スレッド側(§4.6のAggregator)がループの各反復で`rx.len()`を観測した最大値とする。

```rust
// spike-common/src/lib.rs (抜粋)

pub enum CaptureEvent {
    Frame { record: CapturedFrameRecord, samples: Vec<f32> },
    StreamStarted { stream: StreamId, format: AudioFormatInfo, qpc_freq_hz: u64 },
    StreamError { stream: StreamId, error: String },
    /// `mmcss_applied`: このストリームのキャプチャスレッドでMMCSS登録が
    /// 成功したか(§3.9)。Aggregatorはこの値を`StreamStats`へ記録し、
    /// `summary.json`の`mmcss_applied`(§4.8)へ反映する。
    StreamStopped { stream: StreamId, exit: CaptureExit, mmcss_applied: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureExit {
    StoppedByRequest,
    DeviceLost,
}

#[derive(Debug, Clone)]
/// `WAVEFORMATEXTENSIBLE`を安全に解釈するための情報。旧設計の4項目だけでは
/// 不足しており(P1)、以下を追加する。`wFormatTag`が`WAVE_FORMAT_EXTENSIBLE`
/// の場合、実際のフォーマットは`SubFormat`(GUID)と`Samples.wValidBitsPerSample`
/// /`dwChannelMask`から判定する必要があるため、これらを保持する。
pub struct AudioFormatInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub is_float: bool,
    /// `WAVEFORMATEX::wFormatTag`(例: `WAVE_FORMAT_PCM`, `WAVE_FORMAT_IEEE_FLOAT`,
    /// `WAVE_FORMAT_EXTENSIBLE`)
    pub format_tag: u16,
    /// `wFormatTag == WAVE_FORMAT_EXTENSIBLE`の場合の`SubFormat` GUID。
    /// (`KSDATAFORMAT_SUBTYPE_PCM` / `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`等)
    pub sub_format: Option<windows::core::GUID>,
    pub block_align: u16,
    /// `WAVEFORMATEXTENSIBLE::Samples.wValidBitsPerSample`。
    /// `bits_per_sample`(コンテナ幅)と実際の有効ビット数が異なる場合がある。
    pub valid_bits_per_sample: u16,
    /// `WAVEFORMATEXTENSIBLE::dwChannelMask`。チャンネル配置の解釈に使う。
    pub channel_mask: u32,
    pub bytes_per_sample: u16, // bits_per_sample / 8 のヘルパー値
}

impl AudioFormatInfo {
    /// `GetMixFormat`/固定フォーマット双方から生成する。`wFormatTag`が
    /// `WAVE_FORMAT_EXTENSIBLE`かどうかで`WAVEFORMATEXTENSIBLE`として
    /// 再解釈するかを分岐する。
    pub fn from_waveformatex(wfx: &WAVEFORMATEX) -> Self;
}

/// `IAudioClient::GetMixFormat`が返す`*mut WAVEFORMATEX`をラップするRAII型。
/// Microsoftのドキュメントは、このメモリを呼び出し側が`CoTaskMemFree`で
/// 解放する責務を負うと明記している(P1)。生ポインタのまま`Initialize`へ
/// 渡した後は速やかに`AudioFormatInfo::from_waveformatex`で値をコピーし、
/// このガードをdropしてポインタを解放する。
pub struct WaveFormatBox {
    ptr: *mut WAVEFORMATEX,
}

impl WaveFormatBox {
    pub fn from_raw(ptr: *mut WAVEFORMATEX) -> Self { Self { ptr } }
    pub fn as_ref(&self) -> &WAVEFORMATEX { unsafe { &*self.ptr } }
}

impl Drop for WaveFormatBox {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(self.ptr as *const _)) };
    }
}

/// 停止通知は`AtomicBool`のポーリングではなく、手動リセットのWin32イベント
/// オブジェクトで行う。キャプチャループは`WaitForMultipleObjects`で
/// `[audio_ready_event, stop_event]`を同時に待つため、`stop`がシグナルされた
/// 時点で(コールバックタイムアウトの最大待ち時間である2秒を待たずに)
/// 即座にループを抜けられる。SPIKE-02の再アタッチ(§5.7)はこの即時停止性に
/// 依存するため、`AtomicBool`ポーリングへ戻さないこと。
pub struct StopSignal {
    event: windows::Win32::Foundation::HANDLE, // CreateEventW(manual reset, initial=false)
}

impl StopSignal {
    pub fn new() -> windows::core::Result<Self>;
    /// SetEvent。以後`handle()`を待っているすべてのスレッドが即座に解除される。
    pub fn signal(&self) -> windows::core::Result<()>;
    pub fn handle(&self) -> windows::Win32::Foundation::HANDLE;
}

/// P1: `HANDLE`はRAIIで確実に`CloseHandle`する。`StopSignal`は`Arc<StopSignal>`
/// として複数スレッド間で共有するため、最後の参照がdropされた時点で解放する。
/// キャプチャループ内で毎回生成する`audio_ready_event`(§4.4)も同様に、
/// スコープ終了時に`CloseHandle`されるRAIIラッパー(`EventHandleGuard`のような
/// 単純な新型)で保持し、生ポインタのまま持ち回さない。
impl Drop for StopSignal {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.event) };
    }
}

pub trait CaptureStream: Send {
    fn stream_id(&self) -> StreamId;

    /// 呼び出しスレッド内でブロッキングし、`stop`がシグナルされるか回復不能な
    /// エラーが起きるまでキャプチャを継続する。成功時・想定内の停止時は
    /// `Ok(CaptureExit)`を返し、それ以外は`Err(SpikeError)`を返す。
    /// `StreamStopped`/`StreamError`イベントへの変換は`spawn_capture_thread`側
    /// でまとめて行うため、`run`自身はイベント送信の成否(chanel切断等)を
    /// 気にする必要はない。
    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, SpikeError>;
}

/// `spawn_capture_thread`が返す`JoinHandle`の戻り値。P1改善: main.rs(§5.7)は
/// 「共有チャネル経由でCaptureEvent::StreamStoppedが届くのを期待する」のではなく、
/// **この`JoinHandle::join()`の戻り値**を再アタッチ制御の正とする。共有チャネルの
/// 受信側(rx)はAggregatorが所有しており、main.rsとAggregatorの両方が同じ
/// イベントを確実に見られる構造にはなっていないため(P1)、制御フローに必要な
/// 情報はチャネルを介さずスレッドの戻り値で直接返す。チャネル側の
/// `CaptureEvent::StreamStopped`は、Aggregatorが統計・ログ用に使うだけの
/// 副次的な通知として残す。
pub enum CaptureThreadOutcome {
    Stopped { exit: CaptureExit, mmcss_applied: bool },
    Errored { error: SpikeError, mmcss_applied: bool },
}

pub fn spawn_capture_thread(
    stream: Box<dyn CaptureStream>,
    tx: crossbeam_channel::Sender<CaptureEvent>,
    stop: std::sync::Arc<StopSignal>,
) -> std::thread::JoinHandle<CaptureThreadOutcome> {
    let stream_id = stream.stream_id();
    std::thread::Builder::new()
        .name(format!("capture-{:?}", stream_id))
        .spawn(move || {
            // with_pro_audio_priority(§3.9)はMMCSS登録の成否(bool)と
            // クロージャの戻り値の両方をタプルで返す(P1修正: 以前はbool
            // だけを返し、run()の結果を握りつぶしていた)。
            let (mmcss_applied, run_result) =
                crate::mmcss::with_pro_audio_priority(|| stream.run(&tx, &stop));

            match run_result {
                Ok(exit) => {
                    let _ = tx.send(CaptureEvent::StreamStopped { stream: stream_id, exit, mmcss_applied });
                    CaptureThreadOutcome::Stopped { exit, mmcss_applied }
                }
                Err(e) => {
                    let _ = tx.send(CaptureEvent::StreamError { stream: stream_id, error: e.to_string() });
                    CaptureThreadOutcome::Errored { error: e, mmcss_applied }
                }
            }
        })
        .expect("failed to spawn capture thread")
}
```

`CaptureStream` traitをSPIKE-01では`MicCaptureStream`と`EndpointLoopbackStream`が、SPIKE-02では`ProcessLoopbackStream`が実装する。これにより集約ライター側のロジック(CSV/WAV書き出し、統計計算)はスパイク間で完全に共通化できる。`run`が`Result`を返す設計にすることで、呼び出し側(`spawn_capture_thread`)は成功/失敗を一箇所でイベント化でき、実装疑似コード内の`?`演算子とtraitシグネチャの不整合(旧設計の問題点)を解消する。

### 3.9 MMCSS(スレッド優先度)

```rust
// spike-common/src/mmcss.rs

/// AvSetMmThreadCharacteristicsW(L"Pro Audio", ...) でスレッドを
/// マルチメディアクラスケジューラへ登録し、実行中はコールバックの
/// スケジューリング遅延を最小化する。Drop時にAvRevertMmThreadCharacteristicsで解除。
/// 戻り値は適用の成否(`summary.json`の`mmcss_applied`へ反映する。§4.10)。
/// P1修正: 以前は`bool`(MMCSS適用の成否)だけを返し、クロージャ`f`の戻り値
/// (`stream.run(...)`の`Result<CaptureExit, SpikeError>`)を捨てていたため、
/// `spawn_capture_thread`側で結果を握りつぶす原因になっていた。
/// `(mmcss_applied, f()の戻り値)`のタプルで両方を返す。
pub fn with_pro_audio_priority<F: FnOnce() -> R, R>(f: F) -> (bool, R) {
    struct MmcssGuard(windows::Win32::Media::Multimedia::HANDLE); // AvSetMmThreadCharacteristicsW の戻り値
    impl Drop for MmcssGuard {
        fn drop(&mut self) {
            // AvRevertMmThreadCharacteristics(self.0)
        }
    }
    match /* AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) */ {
        Ok(handle) => {
            let _guard = MmcssGuard(handle);
            (true, f())
        }
        Err(_) => {
            tracing::warn!("MMCSS registration failed; running without Pro Audio priority");
            (false, f())
        }
    }
}
```

design.mdやspike-plan.mdには明記されていないが、WASAPIの公式サンプルおよびMicrosoft Learnの推奨事項として、リアルタイムオーディオキャプチャスレッドはMMCSSへ登録することが強く推奨される。これを怠るとSPIKE-01の合否基準「サンプル欠落なし」「ジッタ許容範囲内」の測定値が本来の性能を反映しない可能性があるため、共通基盤に組み込む。適用に失敗した場合も録音自体は継続し、失敗した事実を記録するに留める(MMCSS登録失敗を致命的エラーにはしない)。

### 3.10 OS要件チェック

spike-plan.mdおよびdesign.md §5.1はWindows 11(build 20348以降)を前提とするが、Application Loopback Captureの最低ビルドはMicrosoftの情報源間で記載が揺れている(公式サンプルのREADMEは20348以降、APIリファレンス側の記述は20438以降とする版がある)。この揺れそのものが「既知の不確実性」であるため(§7)、実行時のOSビルドを必ず記録し、未対応が明確な場合は早期に専用エラーへ倒す。

```rust
// spike-common/src/os_check.rs

#[derive(Debug, Clone, serde::Serialize)]
pub struct OsVersionInfo {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

/// RtlGetVersion(公開APIのGetVersionExはマニフェストの互換シムで嘘の値を返し
/// うるため使わない)でOSバージョンを取得する。
pub fn query_os_version() -> windows::core::Result<OsVersionInfo>;

/// SPIKE-02(Process Loopback)専用の下限チェック。
/// 判定がグレーであることを踏まえ、「未満なら即NO-GO相当として扱う」閾値
/// (20348)と、「これ未満だと一部情報源で非対応とされる」閾値(20438)の
/// 両方を記録し、どちらの基準で判定したかをRESULT.mdへ残す。
pub const PROCESS_LOOPBACK_MIN_BUILD_CONSERVATIVE: u32 = 20348;
pub const PROCESS_LOOPBACK_MIN_BUILD_STRICT: u32 = 20438;

pub fn check_process_loopback_support(info: &OsVersionInfo) -> Result<(), SpikeError> {
    if info.build < PROCESS_LOOPBACK_MIN_BUILD_CONSERVATIVE {
        return Err(SpikeError::UnsupportedOsBuild { build: info.build });
    }
    Ok(())
}
```

`main.rs`起動直後にこのチェックを行い、`PROCESS_LOOPBACK_MIN_BUILD_CONSERVATIVE`未満であれば`SpikeError::UnsupportedOsBuild`(§3.7)として即時終了する(COM呼び出し自体は試みない)。`CONSERVATIVE`以上`STRICT`未満のビルドでは、実行は継続しつつ`summary.json`の`os`ブロックへビルド番号を記録し、`ActivateAudioInterfaceAsync`が失敗した場合に「COM実装の不具合」ではなく「OSビルド起因の可能性」として区別できるようにする。

---

## 4. SPIKE-01: WASAPI マイク + Endpoint Loopback 同時取得

### 4.1 目的の再掲(spike-plan.md §Wave1/SPIKE-01)

* マイクcaptureとEndpoint Loopbackを同一プロセス内で同時安定取得できるか
* `QueryPerformanceCounter`ベースの単調時刻を各フレームへ紐付けられるか
* `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`を検出できるか

### 4.2 モジュール構成

```text
spike-01-wasapi-dual-capture/
└─ src/
   ├─ main.rs             # CLI, オーケストレーション
   ├─ device_select.rs    # デバイス列挙・選択
   ├─ mic_stream.rs        # マイクcapture (CaptureStream実装)
   ├─ loopback_stream.rs   # Endpoint Loopback (CaptureStream実装)
   └─ aggregator.rs        # CaptureEvent受信、CSV/WAV書き出し、統計集計
```

### 4.3 デバイス選択

```rust
// device_select.rs

pub struct DeviceInfo {
    pub id: String,       // IMMDevice::GetId()
    pub friendly_name: String,
    pub is_default_for_role: Option<DeviceRole>,
}

/// WASAPIの`ERole`に対応。既定では`Console`を使うが、会議アプリが
/// `eCommunications`ロールの既定デバイス(Bluetoothヘッドセット等)へ
/// 出力/入力している場合があるため、CLIから選べるようにする(§4.7参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DeviceRole {
    Console,
    Multimedia,
    Communications,
}

pub fn enumerate_capture_devices() -> windows::core::Result<Vec<DeviceInfo>>;
pub fn enumerate_render_devices() -> windows::core::Result<Vec<DeviceInfo>>;

pub fn resolve_capture_device(
    enumerator: &IMMDeviceEnumerator,
    id_or_default: &str, // "default" または DeviceInfo.id
    role: DeviceRole,
) -> windows::core::Result<IMMDevice>;

pub fn resolve_render_device(
    enumerator: &IMMDeviceEnumerator,
    id_or_default: &str,
    role: DeviceRole,
) -> windows::core::Result<IMMDevice>;
```

呼び出し順序:

1. `CoCreateInstance::<IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)`
2. `id_or_default == "default"` の場合は `enumerator.GetDefaultAudioEndpoint(eCapture / eRender, role)` — `role`は既定`eConsole`だが、CLIで`eMultimedia` / `eCommunications`を選べるようにする
3. それ以外は `enumerator.GetDevice(id)`

実際に解決された`IMMDevice::GetId()`と`friendly_name`は、後から検証できるよう`summary.json`(§4.8)へ必ず記録する。

**COMオブジェクトのスレッド境界についての注意(§4.4/§5.4のP0修正と対応)**: `enumerate_capture_devices`/`enumerate_render_devices`は、CLIの`--list-devices`表示など「一度きりの短命な問い合わせ」用であり、呼び出したスレッドで`ComApartment::new_mta()`を張って完結させてよい。一方、**実際にキャプチャで使う`IMMDevice`/`IAudioClient`は、その値を取得したスレッドとは別のスレッドへ渡さない。** `resolve_capture_device`/`resolve_render_device`および後続の`Activate`/`Initialize`/`GetService`/キャプチャループ/`Stop`/解放は、すべて同一のcapture MTAスレッド内で完結させる(§4.4)。main.rs側は「"default"または`DeviceInfo.id`の文字列」と`DeviceRole`だけをキャプチャスレッドへ渡し、`IMMDeviceEnumerator`や`IMMDevice`そのものをスレッド間で受け渡さない。

### 4.4 マイクcapture初期化シーケンス

マイクとEndpoint Loopbackは、デバイス種別とstreamFlagsのみが異なり、初期化・キャプチャループの本体は共通化できる(§4.5)。ここでは共通ヘルパー`init_and_capture`として設計する。

```rust
// wasapi_common.rs (spike-01内、将来spike-commonへ格上げ可能)

/// マイク/レンダーデバイスの指定を「文字列+ロール」で保持する。
/// `IMMDevice`そのものは保持しない(P0-3: COM所有権をcapture MTAスレッドへ
/// 一本化する方針のため、デバイス解決自体をinit_and_capture内で行う)。
pub enum DeviceSelector {
    Capture { id_or_default: String, role: DeviceRole },
    Render { id_or_default: String, role: DeviceRole },
}

pub struct WasapiInitParams {
    pub device: DeviceSelector,
    pub extra_stream_flags: u32, // 0 または AUDCLNT_STREAMFLAGS_LOOPBACK
    pub stream_id: StreamId,
    pub callback_timeout_ms: u32,
    /// §3.8のbounded channelが満杯でフレームをdropした回数を数える。
    /// ストリームごとに1つ、mainが所有し`summary.json`(§4.8)の
    /// `queue_drop_packets`へ反映する。
    pub pipeline_drop_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// GetBufferで取得したパケットの解放を保証するRAIIガード。
/// 「ReleaseBufferより先にチャネル送信してはいけない」という制約を型で強制する:
/// このガードが生きている間はサンプルのコピーだけを行い、ガードをdropしてから
/// (＝ReleaseBufferが確実に呼ばれてから)チャネルへ送信する。
struct CapturePacketGuard<'a> {
    client: &'a IAudioCaptureClient,
    frames: u32,
}

impl<'a> Drop for CapturePacketGuard<'a> {
    fn drop(&mut self) {
        // 戻り値のエラーはログのみに留める(Drop内でpanicさせない)。
        // ReleaseBuffer失敗が続く場合は次のGetBufferがAUDCLNT_E_OUT_OF_ORDERを
        // 返すはずなので、そちらでストリームエラーとして検出する。
        let _ = unsafe { self.client.ReleaseBuffer(self.frames) };
    }
}
```

`init_and_capture`のステップ5以降(イベント待ち→`GetBuffer`/`ReleaseBuffer`ループ→`Stop`)は、`run_capture_loop`という名前で共通関数に切り出す。マイク/Endpoint Loopbackは`init_and_capture`がデバイス解決からこの関数を呼ぶ形になり、Process Loopback(§5.6)は独自のActivate/Initialize手順(`activate_and_initialize_with_retry`)の後に同じ`run_capture_loop`を直接呼ぶ。

```rust
pub fn run_capture_loop(
    audio_client: IAudioClient,
    stream_id: StreamId,
    target_pid: Option<u32>,
    capture_epoch: u64,
    format_info: AudioFormatInfo,
    pipeline_drop_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    stop: &StopSignal,
) -> Result<CaptureExit, SpikeError> {
    // audio_ready_event/SetEventHandle/GetService/Start/StreamStarted通知から
    // 下(§4.4のステップ5以降)を実行する。
}

pub fn init_and_capture(
    params: WasapiInitParams,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    stop: &StopSignal,
    capture_epoch: u64,
) -> Result<CaptureExit, SpikeError> {
    // 1. let _com = ComApartment::new_mta()?; // このスレッドでのみCOMを初期化する
    // 2. let enumerator: IMMDeviceEnumerator =
    //        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    //    let device: IMMDevice = match &params.device {
    //        DeviceSelector::Capture { id_or_default, role } =>
    //            resolve_capture_device(&enumerator, id_or_default, *role)?,
    //        DeviceSelector::Render { id_or_default, role } =>
    //            resolve_render_device(&enumerator, id_or_default, *role)?,
    //    };
    //    // enumerator/deviceはこの関数のローカル変数であり、他スレッドへは渡さない。
    //    let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    // 3. let mix_format = WaveFormatBox::from_raw(unsafe { audio_client.GetMixFormat()? });
    //    // WaveFormatBox: *mut WAVEFORMATEXをラップし、DropでCoTaskMemFreeするRAII型。
    //    // GetMixFormatが返すメモリは呼び出し側がCoTaskMemFreeで解放する責務を負う
    //    // (P1)。生ポインタのまま長期保持しない。
    // 4. unsafe {
    //        audio_client.Initialize(
    //            AUDCLNT_SHAREMODE_SHARED,
    //            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | params.extra_stream_flags,
    //            0 /* hnsBufferDuration: 0で最小レイテンシをOSに委ねる */,
    //            0,
    //            mix_format,
    //            None,
    //        )?;
    //    }
    // 5. let audio_ready_event = unsafe { CreateEventW(None, false, false, None)? };
    //    unsafe { audio_client.SetEventHandle(audio_ready_event)? };
    // 6. let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService()? };
    // 7. let qpc_clock = QpcClock::query()?;
    //    let format_info = AudioFormatInfo::from_waveformatex(mix_format);
    //    unsafe { audio_client.Start()? };
    //    tx.send(CaptureEvent::StreamStarted {
    //        stream: params.stream_id, format: format_info, qpc_freq_hz: qpc_clock.freq_hz(),
    //    }).ok();

    let wait_handles = [/* audio_ready_event */, stop.handle()];
    let mut wake_seq: u64 = 0;
    let mut packet_seq: u64 = 0;
    // Process Loopbackで対象アプリが無音の間、通知が来ないままタイムアウトを
    // 繰り返す回数。エラーではなく仕様上の挙動として記録する(P1)。
    let mut idle_timeout_count: u64 = 0;

    let exit = loop {
        // WaitForMultipleObjects(&wait_handles, bWaitAll=false, timeout_ms)
        // stop_eventを配列に含めることで、AtomicBoolポーリングに伴う
        // 「タイムアウト値(最大2秒)を待つまで停止できない」問題を避ける。
        match wait_for_multiple(&wait_handles, params.callback_timeout_ms) {
            WaitResult::Signaled(0) => {
                // audio_ready_event 側がシグナルされた
                wake_seq += 1;
                let wake_qpc_100ns = qpc_clock.now_100ns();

                loop {
                    // GetNextPacketSizeが0になるまで内側ループで排出する。
                    // イベント1回につきパケットは1個とは限らない
                    // (バースト時に複数パケット溜まることがある)。
                    let packet_len = unsafe { capture_client.GetNextPacketSize()? };
                    if packet_len == 0 { break; }

                    let mut data_ptr: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    let mut device_position_frames: u64 = 0;
                    let mut capture_qpc_100ns: u64 = 0;
                    unsafe {
                        capture_client.GetBuffer(
                            &mut data_ptr, &mut frames, &mut flags,
                            Some(&mut device_position_frames), Some(&mut capture_qpc_100ns),
                        )?;
                    }

                    // GetBufferの直後にガードを構築する。以後のどの経路
                    // (早期return、パニック、送信失敗)でもReleaseBufferが
                    // 必ず呼ばれる。ガードが生きている間にtx.send()は行わない。
                    let guard = CapturePacketGuard { client: &capture_client, frames };
                    let is_silent = flags & CapturedFrameRecord::FLAG_SILENT != 0 || data_ptr.is_null();
                    let samples = if is_silent {
                        vec![0.0f32; frames as usize * format_info.channels as usize]
                    } else {
                        copy_to_f32_vec(data_ptr, frames, &format_info)
                    };
                    drop(guard); // ここでReleaseBufferが確定する

                    let record = CapturedFrameRecord::from_raw(
                        params.stream_id, wake_seq, packet_seq,
                        wake_qpc_100ns, device_position_frames, capture_qpc_100ns,
                        frames, flags, capture_epoch, None,
                    );
                    packet_seq += 1;

                    // P1改善: send()(ブロッキング)ではなくtry_send()を使う。
                    // チャネルは§3.8で bounded として作成しており、集約スレッドの
                    // I/Oが詰まって満杯になった場合、ここでブロックするとWASAPIの
                    // コールバックスレッドが止まり欠落の原因になる。満杯時は
                    // 「内部パイプラインdrop」としてカウントし、キャプチャ自体は継続する。
                    match tx.try_send(CaptureEvent::Frame { record, samples }) {
                        Ok(()) => {}
                        Err(crossbeam_channel::TrySendError::Full(_)) => {
                            params.pipeline_drop_counter.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                            // 受信側(集約スレッド)が切断済み。ストリームを終了する。
                            unsafe { audio_client.Stop()? };
                            return Ok(CaptureExit::StoppedByRequest);
                        }
                    }
                }
            }
            WaitResult::Signaled(1) => {
                break CaptureExit::StoppedByRequest; // stop_event
            }
            WaitResult::Timeout => {
                // 【訂正】Process Loopbackは、対象プロセスツリーにレンダー
                // ストリームが存在しない(=対象アプリが無音の)場合、エラーでは
                // なく単に通知が来ない仕様である。したがって
                // `StreamId::ProcessLoopback`でのタイムアウトを毎回
                // `StreamError`として扱うのは適切でない。マイク/Endpoint
                // Loopback(常に何らかの信号があるはずの経路)では引き続き
                // `StreamError`として異常寄りに扱うが、Process Loopbackでは
                // `idle_timeout_count`(§4.8のsummary.jsonへ記録)として
                // カウントするだけに留め、ストリーム自体は継続する。
                match params.stream_id {
                    StreamId::ProcessLoopback => {
                        idle_timeout_count += 1;
                    }
                    StreamId::Mic | StreamId::EndpointLoopback => {
                        let _ = tx.send(CaptureEvent::StreamError {
                            stream: params.stream_id,
                            error: format!("callback timeout({}ms)", params.callback_timeout_ms),
                        });
                    }
                }
                continue;
            }
        }
    };

    unsafe { audio_client.Stop()?; }
    tracing::info!(stream = ?params.stream_id, idle_timeout_count, "capture loop finished");
    // idle_timeout_countはStreamStopped(§3.8)の付随情報としてではなく、
    // Aggregator側がCSV/ログから集計してsummary.jsonのidle_timeout_count
    // (§4.8/§5.9)へ反映する。ストリーム内部でカウントし、終了時にログへ
    // 出すのはRESULT.md作成時の手動確認を助けるためのものである。
    Ok(exit)
}
```

**設計上の注意点**:

* **`GetBuffer`と`ReleaseBuffer`の順序**: サンプルをコピーし終えるまでは`CapturePacketGuard`を保持し、コピー完了後すぐに`drop(guard)`で`ReleaseBuffer`を確定させる。**`ReleaseBuffer`より前に`tx.send()`(チャネル送信)を行わない。** 送信がブロックする、または受信側切断で失敗するケースがあり、`ReleaseBuffer`を呼ばずに次の`GetBuffer`を呼ぶと`AUDCLNT_E_OUT_OF_ORDER`になり得るため。
* `AUDCLNT_BUFFERFLAGS_SILENT`が立っている場合、`data_ptr`は無効(無音として扱い、ゼロ埋めサンプルを生成する)。
* `wake_seq`(コールバック起床の連番)と`packet_seq`(パケットの連番)を分離する。同一起床で複数パケットを排出した場合、それらのレコードは同じ`wake_seq`・同じ`wake_qpc_100ns`を持つ。コールバックジッタ(§4.9)は`wake_seq`ごとに1回だけ計算し、`packet_seq`単位の差分を混ぜない。
* 停止は`stop_event`を`WaitForMultipleObjects`へ含めることで即時に行う。コールバックタイムアウト(既定2000ms)はあくまで「コールバック間隔異常」の検出用であり、タイムアウト自体はエラー終了せず`StreamError`イベントとして記録を継続する。

両ストリームとも、保持するのは「デバイスID文字列とロール」だけであり、`IMMDevice`/`IAudioClient`はメンバに持たない(P0-3)。`main.rs`はこの2つの構造体をCLI引数から直接構築し、`spawn_capture_thread`へ`Box<dyn CaptureStream>`として渡すだけでよい。

```rust
// mic_stream.rs

pub struct MicCaptureStream {
    device_id_or_default: String, // CLIの--mic-deviceそのまま
    role: DeviceRole,
    pipeline_drop_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl CaptureStream for MicCaptureStream {
    fn stream_id(&self) -> StreamId { StreamId::Mic }

    fn run(self: Box<Self>, tx: &Sender<CaptureEvent>, stop: &StopSignal) -> Result<CaptureExit, SpikeError> {
        init_and_capture(
            WasapiInitParams {
                device: DeviceSelector::Capture {
                    id_or_default: self.device_id_or_default,
                    role: self.role,
                },
                extra_stream_flags: 0,
                stream_id: StreamId::Mic,
                callback_timeout_ms: 2000,
                pipeline_drop_counter: self.pipeline_drop_counter,
            },
            tx,
            stop,
            /* capture_epoch */ 0,
        )
    }
}
```

### 4.5 Endpoint Loopback初期化シーケンス

`init_and_capture`をそのまま使い、パラメータのみ変える。

```rust
// loopback_stream.rs

pub struct EndpointLoopbackStream {
    device_id_or_default: String, // CLIの--render-deviceそのまま。"render"(既定の再生エンドポイント)をActivateする
    role: DeviceRole,
    pipeline_drop_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl CaptureStream for EndpointLoopbackStream {
    fn stream_id(&self) -> StreamId { StreamId::EndpointLoopback }

    fn run(self: Box<Self>, tx: &Sender<CaptureEvent>, stop: &StopSignal) -> Result<CaptureExit, SpikeError> {
        init_and_capture(
            WasapiInitParams {
                device: DeviceSelector::Render {
                    id_or_default: self.device_id_or_default,
                    role: self.role,
                },
                extra_stream_flags: AUDCLNT_STREAMFLAGS_LOOPBACK,
                stream_id: StreamId::EndpointLoopback,
                callback_timeout_ms: 2000,
                pipeline_drop_counter: self.pipeline_drop_counter,
            },
            tx,
            stop,
            /* capture_epoch */ 0,
        )
    }
}
```

マイク側との差分は「デバイスがrenderエンドポイントであること」と「`extra_stream_flags`に`AUDCLNT_STREAMFLAGS_LOOPBACK`を追加すること」のみであり、`GetBuffer`/`ReleaseBuffer`ループ、MMCSS適用、CSV/WAV出力へ渡すイベントの形は完全に共通である。

### 4.6 集約ライター(`aggregator.rs`)

`AudioFormatInfo`は`StreamStarted`イベントが届くまで分からないため、CSVは即座に開けてもWAVライターは`StreamStarted`受信後まで生成できない。両者を`Option`で保持する`StreamSink`にまとめる。

```rust
pub struct StreamSink {
    csv: FrameCsvWriter,        // ファイルパスが決まっていればStreamStarted前でもcreate可能
    wav: Option<PcmWavWriter>,  // StreamStarted受信時に生成する
    format: Option<AudioFormatInfo>,
    stats: StreamStats,
}

pub struct Aggregator {
    sinks: HashMap<StreamId, StreamSink>,
}

#[derive(Default)]
pub struct StreamStats {
    pub wake_events: u64,
    pub packet_events: u64,
    pub discontinuity_count: u64,
    pub silent_count: u64,
    pub timestamp_error_count: u64,
    /// wake_seqごとの`wake_qpc_100ns`差分列(コールバックジッタ計算用)。
    /// packet_seq側の重複値は含めない(§4.4参照)。
    pub wake_interval_100ns: Vec<u64>,
    pub last_wake_qpc_100ns: Option<u64>,
    /// (capture_qpc_100ns, device_position_frames) の系列。
    /// gap検出とクロックdrift回帰の両方に使う(§4.9)。
    pub position_series: Vec<(u64, u64)>,
    pub monotonic_violations: u64,
    pub total_frames_captured: u64,
    /// `CaptureEvent::StreamStopped`受信時に埋める(P1修正)。スレッド開始時点では
    /// 未確定なため`false`初期値のままにせず、`Option<bool>`にして
    /// 「まだ終了していない」と「MMCSS適用に失敗した」を区別する。
    pub mmcss_applied: Option<bool>,
}

impl Aggregator {
    pub fn run(mut self, rx: Receiver<CaptureEvent>) -> anyhow::Result<SummaryReport> {
        for event in rx {
            match event {
                CaptureEvent::StreamStarted { stream, format, qpc_freq_hz } => {
                    let sink = self.sinks.get_mut(&stream).unwrap();
                    sink.format = Some(format.clone());
                    // ここで初めてWAVライターを生成する。
                    // 保存形式は「ネイティブPCM形式をそのまま保存」ではなく、
                    // 全ストリーム共通で「interleaved float32(IEEE float WAV)」に
                    // 統一する。チャネル上のペイロードが常にVec<f32>であるため、
                    // ネイティブビット幅を保存する経路は用意しない。
                    sink.wav = Some(PcmWavWriter::create_from_format(
                        &self.wav_path_for(stream), format.channels, format.sample_rate,
                    )?);
                }
                CaptureEvent::Frame { record, samples } => {
                    let sink = self.sinks.get_mut(&record.stream).unwrap();
                    sink.csv.write(&record)?;
                    if let Some(wav) = sink.wav.as_mut() {
                        wav.write_samples(&samples)?;
                    }
                    sink.stats.update(&record);
                }
                CaptureEvent::StreamError { stream, error } => {
                    tracing::warn!(?stream, %error, "stream error");
                }
                CaptureEvent::StreamStopped { stream, exit, mmcss_applied } => {
                    tracing::info!(?stream, ?exit, mmcss_applied, "stream stopped");
                    // summary.jsonのstreams.<name>.mmcss_appliedへ反映する(P1修正)。
                    // これはあくまで統計・ログ用途であり、main.rs側の再アタッチ制御は
                    // この受信を待たず、spawn_capture_threadのJoinHandle(§3.8の
                    // CaptureThreadOutcome)を正とする(§5.7参照)。
                    self.sinks.get_mut(&stream).unwrap().stats.mmcss_applied = Some(mmcss_applied);
                }
            }
        }
        // 終了時に各sinkをflush/finalizeし、SummaryReportを構築して返す
    }
}
```

集約スレッドを1本に絞ることで、ファイルI/OをキャプチャスレッドのリアルタイムクリティカルパスからI/O責務ごと分離する(design.md §7.1の「Captureは取得のみ」思想をスパイクレベルでも踏襲)。

### 4.7 CLIインターフェース

```rust
// main.rs

#[derive(clap::Parser)]
struct Cli {
    /// 録音時間(秒)。spike-plan.md既定は600秒(10分)
    #[arg(long, default_value_t = 600)]
    duration_secs: u64,

    /// マイクデバイスID、または "default"
    #[arg(long, default_value = "default")]
    mic_device: String,

    /// 再生(loopback対象)デバイスID、または "default"
    #[arg(long, default_value = "default")]
    render_device: String,

    /// マイク・再生デバイスの既定デバイス解決に使うロール。
    /// 会議アプリがeCommunicationsの既定デバイス(Bluetoothヘッドセット等)を
    /// 使っている場合があるため、consoleとcommunicationsの両方を試せるようにする。
    #[arg(long, value_enum, default_value_t = DeviceRole::Console)]
    device_role: DeviceRole,

    /// 出力先ディレクトリ。省略時は out/{timestamp} を自動生成
    #[arg(long)]
    output_dir: Option<PathBuf>,

    /// キャプチャコールバックのタイムアウト(ms)。WaitForMultipleObjectsに渡す。
    #[arg(long, default_value_t = 2000)]
    callback_timeout_ms: u32,
}
```

実行例:

```powershell
spike-01-wasapi-dual-capture.exe --duration-secs 600 --mic-device default --render-device default
```

### 4.8 出力ディレクトリ・成果物

```text
spikes/spike-01-wasapi-dual-capture/out/{run_id}/
├─ mic.csv
├─ loopback.csv
├─ mic.wav
├─ loopback.wav
└─ summary.json
```

`summary.json`のスキーマ:

```json
{
  "run_id": "2026-07-08T10-00-00Z",
  "duration_secs": 600,
  "os": { "major": 10, "minor": 0, "build": 22631 },
  "qpc_freq_hz": 10000000,
  "devices": {
    "mic": { "id": "{0.0.1.00000000}.{...}", "friendly_name": "Microphone Array", "role": "console" },
    "loopback_render": { "id": "{0.0.0.00000000}.{...}", "friendly_name": "Speakers", "role": "console" }
  },
  "streams": {
    "mic": {
      "wake_events": 60000,
      "packet_events": 60000,
      "total_frames_captured": 28800000,
      "discontinuity_count": 0,
      "silent_count": 3,
      "timestamp_error_count": 0,
      "expected_wake_interval_ms": 10.0,
      "wake_interval_ms": { "mean": 10.0, "p95": 10.3, "p99": 10.9, "max": 15.2 },
      "wake_jitter_ms": { "mean": 0.0, "p95": 0.3, "p99": 0.9, "max": 5.2 },
      "packet_age_at_wake_ms": { "mean": 1.2, "p95": 2.1, "p99": 3.4, "max": 6.0 },
      "position_gap_frames_total": 0,
      "position_overlap_frames_total": 0,
      "monotonic_violations": 0,
      "clock_drift_ppm_vs_qpc": 4.2,
      "mmcss_applied": true
    },
    "loopback": { "...": "..." }
  },
  "relative_drift_ppm_mic_vs_loopback": 1.8,
  "process_cpu_percent_estimate": 3.1,
  "process_peak_working_set_bytes": 41943040,
  "acceptance": {
    "qpc_monotonic": true,
    "device_position_continuous": true,
    "discontinuity_detection_operational": true,
    "discontinuity_count": 0,
    "position_gap_count": 0,
    "pipeline_drop_count": 0,
    "drift_within_target": true,
    "wake_jitter_within_target": true,
    "cpu_under_10_percent": true,
    "mmcss_applied": true,
    "os_build_supported": true,
    "overall_suggestion": "GO"
  }
}
```

### 4.9 解析(analyze)ロジック — `spike-common::analyze`

旧設計の`estimate_drift`(録音時間とframe_count合計だけからppmを出す方式)は、Start()直後のパケット到達遅延・停止境界誤差・2ストリームの開始時刻差・真のクロックdrift・パケット欠落を区別せずに混ぜてしまい、クロックdriftの指標として使えない。そのため、**パケット欠落検出**と**クロックdrift測定**を別の計算として分離する。

**解析単位についての訂正(P0-5)**: SPIKE-02では再アタッチのたびに`capture_epoch`が増分し、`device_position_frames`・`packet_seq`・`wake_seq`はいずれも新しいストリームとして0からリセットされる(§3.2, §5.7)。以下の`detect_position_gaps`/`estimate_clock_drift`/`detect_monotonic_violations`/`compute_wake_jitter`は、レコード列全体をそのまま渡すと、epoch境界での正当なリセットを巨大なgap/overlapや単調性違反として誤検出する。**必ず`group_records_by_epoch`で`(stream, capture_epoch, target_pid)`単位に分割してから、各グループへ個別に適用すること。** SPIKE-01は常に`capture_epoch == 0`・`target_pid == None`のみなので実質1グループになるが、関数の呼び出し方自体はSPIKE-01/02で共通にする。

```rust
// spike-common/src/analyze.rs

/// 解析対象のグルーピングキー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EpochKey {
    pub stream: StreamId,
    pub capture_epoch: u64,
    pub target_pid: Option<u32>,
}

pub struct EpochRecords<'a> {
    pub key: EpochKey,
    pub records: Vec<&'a CapturedFrameRecord>, // packet_seq昇順
    /// このepochの最初/最後のレコードのcapture_qpc_100ns。
    /// 相対drift比較(後述)で、時間範囲が重なるepoch同士だけを比較するために使う。
    pub qpc_range_100ns: (u64, u64),
}

/// CSVから読み込んだレコード列を`(stream, capture_epoch, target_pid)`でグルーピングする。
/// 以降のgap検出・drift回帰・単調性チェック・wakeジッタは、必ずこの関数が返す
/// グループ単位で呼び出す(レコード列全体を素通しで渡さない)。
pub fn group_records_by_epoch(records: &[CapturedFrameRecord]) -> Vec<EpochRecords<'_>>;

/// device_position_frames の連続性チェック(パケット欠落検出)。
/// 1つの`EpochRecords`(単一のstream・epoch・PID)を`packet_seq`順に走査し、
/// 前パケットの終端位置と現パケットの開始位置を比較する。
pub struct PositionGapStats {
    pub gap_frames_total: u64,     // 現在位置が期待より進んでいた(欠落)量の合計
    pub overlap_frames_total: u64, // 現在位置が期待より戻っていた(重複/異常)量の合計
    pub gap_events: u64,
    pub overlap_events: u64,
}

pub fn detect_position_gaps(records: &[CapturedFrameRecord]) -> PositionGapStats {
    let mut stats = PositionGapStats::default();
    let mut prev_end: Option<u64> = None;
    for r in records {
        if let Some(expected_next) = prev_end {
            let actual = r.device_position_frames;
            if actual > expected_next {
                stats.gap_frames_total += actual - expected_next;
                stats.gap_events += 1;
            } else if actual < expected_next {
                stats.overlap_frames_total += expected_next - actual;
                stats.overlap_events += 1;
            }
        }
        prev_end = Some(r.device_position_frames + r.frame_count as u64);
    }
    stats
}

/// QPC経過時間に対するdevice_position_framesの傾きを線形回帰で求め、
/// 公称サンプルレートとの差をppmで返す。先頭・末尾の2点だけを使うと
/// パケットジッタの影響を受けやすいため、全点を使った最小二乗回帰とする。
pub struct ClockDriftEstimate {
    pub effective_sample_rate_hz: f64,
    pub drift_ppm: f64, // (effective_sample_rate_hz / nominal_sample_rate_hz - 1) * 1e6
}

pub fn estimate_clock_drift(
    position_series: &[(u64 /* capture_qpc_100ns */, u64 /* device_position_frames */)],
    nominal_sample_rate_hz: u32,
) -> ClockDriftEstimate {
    // 【単位の訂正】capture_qpc_100nsは既に100ns単位(=QPCカウントを
    // QueryPerformanceFrequencyで換算済みの値)であり、QPCの生カウントでは
    // ない。したがって qpc_freq_hz で割ってはいけない(単位が合わない)。
    // 100ns単位から秒への換算は定数 10_000_000.0 で行う。
    //
    // また、capture_qpc_100ns・device_position_framesとも絶対値のまま
    // 回帰すると浮動小数点の桁落ちが起きやすいため、系列内の先頭値を
    // 差し引いた相対値で回帰する。
    let (first_qpc_100ns, first_pos_frames) = position_series[0];

    let points: Vec<(f64, f64)> = position_series.iter().map(|&(qpc_100ns, pos_frames)| {
        let x_sec = (qpc_100ns - first_qpc_100ns) as f64 / 10_000_000.0;
        let y_frames = (pos_frames - first_pos_frames) as f64;
        (x_sec, y_frames)
    }).collect();

    // 最小二乗法で傾き(= effective_sample_rate_hz)を求める。
    let effective_sample_rate_hz = least_squares_slope(&points);
    let drift_ppm = (effective_sample_rate_hz / nominal_sample_rate_hz as f64 - 1.0) * 1_000_000.0;

    ClockDriftEstimate { effective_sample_rate_hz, drift_ppm }
}

/// 2ストリーム間の相対drift。マイクとレンダーデバイスのクロック差を表す。
/// **同じ時間範囲を共有するepoch同士でのみ**比較すること。SPIKE-01では
/// 通常mic/loopbackとも単一epoch(0)なので単純比較でよいが、SPIKE-02で
/// 複数epochが存在する場合は、比較対象2つの`EpochRecords::qpc_range_100ns`が
/// 重なっているかを呼び出し側で確認してから渡す。
pub fn relative_drift_ppm(mic: &ClockDriftEstimate, loopback: &ClockDriftEstimate) -> f64 {
    (mic.effective_sample_rate_hz / loopback.effective_sample_rate_hz - 1.0) * 1_000_000.0
}

/// `mic_epoch`と`loopback_epoch`のqpc_range_100nsが重ならない場合はNoneを返す
/// (比較不能。summaryへは`null`として出力し、比較できなかった旨を記録する)。
pub fn overlapping_relative_drift_ppm(
    mic_epoch: &EpochRecords, mic_drift: &ClockDriftEstimate,
    loopback_epoch: &EpochRecords, loopback_drift: &ClockDriftEstimate,
) -> Option<f64> {
    let (a_start, a_end) = mic_epoch.qpc_range_100ns;
    let (b_start, b_end) = loopback_epoch.qpc_range_100ns;
    if a_start > b_end || b_start > a_end {
        return None; // 時間範囲が重ならない
    }
    Some(relative_drift_ppm(mic_drift, loopback_drift))
}

/// **「wake interval」と「jitter」の分離(P1)**: 旧設計は`wake_qpc_100ns`の
/// 差分列をそのまま統計化し「ジッタ」と呼んでいたが、これは観測された
/// 周期(interval)そのものであり、本来のジッタ(`observed_interval -
/// expected_interval`)ではない。期待周期を記録しないままでは、10ms周期
/// なのか20ms周期なのかも判断できないため、`expected_interval_ms`
/// (`IAudioClient::GetBufferSize`から算出した1バッファ分の周期。可能なら
/// `IAudioClient::GetDevicePeriod`の値も併記する)を明示的に記録し、
/// intervalとjitterを別のフィールドとして分ける。
pub struct IntervalStats { pub mean_ms: f64, pub p95_ms: f64, pub p99_ms: f64, pub max_ms: f64 }
pub struct JitterStats { pub mean_ms: f64, pub p95_ms: f64, pub p99_ms: f64, pub max_ms: f64 }

pub struct WakeTimingReport {
    pub expected_interval_ms: f64, // GetBufferSize(またはGetDevicePeriod)由来
    pub interval: IntervalStats,   // observed_interval_msの分布
    pub jitter: JitterStats,       // (observed_interval_ms - expected_interval_ms)の分布
}

/// `wake_seq`単位の`wake_qpc_100ns`差分列から`WakeTimingReport`を算出する。
/// `packet_seq`側の重複値(同一wakeで複数パケットを排出した場合)を混ぜないこと。
/// `wake_qpc_100ns`は既に100ns単位(秒への換算は`/10_000_000.0`、msへの換算は
/// `/10_000.0`)であるため、`qpc_freq_hz`による除算は不要(§4.9のdrift計算と
/// 同じ単位の誤りを避ける)。
pub fn compute_wake_timing(
    wake_qpc_100ns_series: &[u64],
    expected_interval_ms: f64,
) -> WakeTimingReport;

/// `wake_qpc_100ns - capture_qpc_100ns`(§3.2)を「起床時点で観測されたパケットの
/// 経過時間」として集計する。スケジューリング遅延と断定しないこと(§3.2参照)。
pub fn compute_packet_age_at_wake(records: &[&CapturedFrameRecord]) -> JitterStats;

pub fn detect_monotonic_violations(qpc_series_100ns: &[u64]) -> u64;
pub fn measure_cpu_percent(start: ProcessTimes, end: ProcessTimes, wall_secs: f64) -> f64;
pub fn measure_peak_working_set_bytes() -> u64; // GetProcessMemoryInfo().PeakWorkingSetSize
```

`measure_cpu_percent`は`GetProcessTimes`(kernel time + user time)の差分を経過壁時計時間で割り、論理コア数で割らない「1コア相当%」として算出する(spike-plan.md合否基準「1コアの10%未満」に合わせる)。

### 4.10 合否判定の自動化

`summary.json`の`acceptance`ブロックは、spike-plan.mdの合否基準を機械的にチェックした一次判定であり、最終判定(GO/CONDITIONAL-GO/NO-GO)はRESULT.md作成時に人が行う。**「欠落を検出できたか」と「そもそも欠落がなかったか」は別項目として分離する**(前者だけでは録音品質の良し悪しが分からないため)。

**epoch単位での集計についての注記(P0-5)**: 下表のうち「デバイス位置連続性」「drift」「QPC単調性」「wake jitter」は、`group_records_by_epoch`(§4.9)で分割した各epochに対して個別に計算し、`summary.json`にはepochごとの内訳(`streams.<name>.epochs[]`)を残したうえで、**いずれかのepochで基準を満たさなければ全体としても不合格**とするロールアップ規則にする。epoch境界(再アタッチの瞬間)そのものは「欠落」ではないため、境界をまたいだ差分をgap/overlapとして計上しない(§4.9)。

自動チェック項目:

| 項目 | 判定式 |
|---|---|
| QPC単調性 | `timestamp_error == false`のレコードについて`monotonic_violations == 0` |
| デバイス位置連続性 | `discontinuity_detection_operational == true`(gap/discontinuity検出の仕組みそのものが機能しているか)。**「検出できること」と「実際に発生しなかったこと」は別項目**であり、後者は`position_gap_count`/`discontinuity_count`として実測値をそのまま記録する(0が理想だが、非0でも検出できていれば「検出可能」要件自体は満たす) |
| WASAPI glitch実測 | `discontinuity_count`(非0でも即不合格にはしないが、RESULT.mdへ区間ごと記録する) |
| 内部パイプライン欠落 | `pipeline_drop_count`(§3.8のbounded channelでの`Full`回数の合計)が0であること。チャネル送信失敗数・CSVライター書き込み失敗数もあわせて0であることを確認する |
| drift | `relative_drift_ppm_mic_vs_loopback`が閾値内。**閾値の算出根拠の訂正**: 「10分で100ms」という品質ゴール(design.md §3.2)から逆算すると、600秒 × 200ppm = 120msとなり200ppmでは目標を超えてしまう。100ms以内に収めるための単純な定常drift許容量は600秒 × X ppm ≤ 100ms → X ≤ 約167ppmであり、初期目安は**167ppm未満**とする。ただし実運用ではSPIKE-03が継続的にdrift補正を行う前提のため、真に決めるべきは「無補正で許容する時間」「再同期・補正周期」「1回あたりの最大補正量」であり、167ppmはあくまで補正なし条件での目安値としてRESULT.mdに明記し、SPIKE-03側のパラメータ確定後に再計算する |
| wake jitter | `wake_jitter_ms.p99`が閾値内(初期目安: バッファ長の2倍未満)。§4.9のP1改善により、これは`observed_interval - expected_interval`ベースの真のジッタである(旧設計の生intervalではない) |
| CPU使用率 | `process_cpu_percent_estimate < 10.0` |
| メモリ | `process_peak_working_set_bytes`を記録(閾値は定めず傾向観測) |
| MMCSS | `mmcss_applied == true`(適用失敗時は理由を記録し、CPU/jitter結果への影響を考慮する) |
| OS/API | `os_build_supported == true`(§7のビルド要件参照) |

### 4.11 実行手順(spike-plan.md手順との対応)

**分離性能検証についての訂正(P1)**: 当初手順の「スピーカーから880Hz再生、マイクへ440Hz入力」は、スピーカー音が空気を通じてマイクへ回り込む(design.md §23の既知リスク)ため、Self/Remoteの分離性能そのものの検証としては曖昧である。ヘッドホンまたは仮想オーディオデバイスを使い、物理的な音の回り込みを排除したうえで、聴感確認だけでなく周波数強度の自動判定を行う。

1. `cargo run -p spike-01-wasapi-dual-capture -- --duration-secs 600`
2. マイクへの音源入力はヘッドホン出力を直接ループバックする、または仮想オーディオデバイス(検証用途限定。design.md §4の「仮想オーディオドライバはPhase 1で使用しない」は製品要件であり、本スパイクの検証環境構築とは別の話である)を使い、スピーカー→空気→マイクの回り込みを避ける
3. Remote(loopback)側で880Hz、Self(mic)側で440Hzのトーンを流し、区間の途中に無音区間と短い同期パルス(例: 1kHzを100ms)を意図的に挿入する
4. 終了後、`out/{run_id}/summary.json`を確認する
5. 聴感確認に加えて、`mic.wav`/`loopback.wav`をFFTし、以下を自動判定するスクリプト(スパイクのスコープ内。`spike-common::analyze`または別スクリプトで可)で分離性能を数値化する
   - loopback側での880Hz強度
   - mic側での440Hz強度
   - 相互混入比(mic側の880Hz強度 / mic側の440Hz強度、loopback側もその逆)
   - 同期パルスの検出位置(2トラック間の実測タイムラグの裏取り)
6. 通常状態に加え、次の負荷・障害ケースを1回ずつ実施する: CPU負荷中、ディスク書き込み負荷中、既定デバイス変更。デバイス切断・スリープ復帰・Bluetoothプロファイル切替はSPIKE-09のスコープであり本スパイクでは深追いしない
7. `RESULT.md`へ判定を記入する(FFT測定値・混入比を測定値欄に残す)

---

## 5. SPIKE-02: Application Loopback Capture(プロセス指定)

### 5.1 目的の再掲(spike-plan.md §Wave1/SPIKE-02)

* `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`で指定プロセスツリーの音声のみをRust `windows` crateから取得できるか
* Zoom/Teamsの実会議音声を分離取得できるか
* プロセス再起動(PID変化)を検出し再アタッチできるか

### 5.2 モジュール構成

```text
spike-02-app-loopback/
└─ src/
   ├─ main.rs
   ├─ process_finder.rs     # プロセス名 -> PID 探索、生存監視
   ├─ process_loopback.rs   # ActivateAudioInterfaceAsync + IAudioClient初期化
   └─ completion_handler.rs # IActivateAudioInterfaceCompletionHandler実装
```

`aggregator.rs`・CSV/WAV書き出しは`spike-common`および可能であればSPIKE-01のコードをworkspace内で再利用する(`spike-01`側のaggregator実装を`spike-common`へ格上げすることを推奨。SPIKE-01実装時点でこの前提を意識してモジュール分割する)。

### 5.3 対象プロセス探索(`process_finder.rs`)

Zoom/Teams/Chromeのようなマルチプロセスアプリでは、実行ファイル名だけでは対象PIDが一意に決まらない。そのため、名前指定に加えて明示的なPID指定・選択戦略を用意する。

```rust
pub struct ProcessMatch {
    pub pid: u32,
    pub exe_name: String,
    pub parent_pid: u32,
    pub start_time: std::time::SystemTime, // sysinfoのstart_time()から取得
}

/// プロセスツリーの選び方。Process Loopbackは対象プロセスとその子プロセスを
/// 含める仕組みのため、原則として`Root`(親を持たない、または最も祖先に近い
/// 候補)を選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProcessSelectionStrategy {
    /// 同名プロセスの中で親PIDが同名プロセス群に含まれない(ツリーの根に近い)ものを選ぶ
    Root,
    /// start_timeが最も新しいものを選ぶ
    Newest,
    /// フォアグラウンドウィンドウを持つプロセスを優先する(GetForegroundWindow +
    /// GetWindowThreadProcessId で解決。ウィンドウが見つからない場合はNewestにフォールバック)
    Foreground,
}

/// 実行ファイル名(例: "Zoom.exe", "ms-teams.exe", "chrome.exe")で
/// 起動中プロセスを検索する。sysinfo crateを使い、CreateToolhelp32Snapshotの
/// 直接呼び出しは避ける(実装コスト削減。スパイクなので十分)。
/// 複数候補が見つかった場合は全候補をログへ残し(PID・親PID・開始時刻)、
/// `strategy`に従って1件へ絞り込む。
pub fn find_process_by_name(
    exe_name: &str,
    strategy: ProcessSelectionStrategy,
) -> Option<ProcessMatch>;

/// `--target-pid`が明示された場合はこちらを使い、名前解決を経由しない。
pub fn resolve_process_by_pid(pid: u32) -> Option<ProcessMatch>;

/// 指定PIDが生存しているかをポーリングする。
/// OpenProcess(SYNCHRONIZE, false, pid) + WaitForSingleObject(handle, 0) で
/// シグナル状態(=プロセス終了)を検出する軽量実装でも良いが、
/// spike簡略化のため sysinfo による定期ポーリング(1秒間隔)を既定実装とする。
pub struct ProcessWatcher {
    target_exe_name: Option<String>, // --target-pid指定時はNone(名前では追跡しない)
    strategy: ProcessSelectionStrategy,
    current_pid: Option<u32>,
}

pub enum ProcessWatchEvent {
    StillAlive(u32),
    Exited { old_pid: u32 },
    Restarted { old_pid: u32, new_pid: u32 },
    NotFound,
}

impl ProcessWatcher {
    pub fn poll(&mut self) -> ProcessWatchEvent;
}
```

### 5.4 `ActivateAudioInterfaceAsync`呼び出し設計

これが本スパイクの最大の不確実点(spike-plan.md記載どおり)。同期APIのように使うため、コールバックを`oneshot`チャネルでブロッキング待機に変換する。この関数はcapture MTAスレッド内(§5.6の`ProcessLoopbackStream::run`)から呼ばれ、`IAudioClient`をこの関数の外へ持ち出した後もスレッドをまたがない(P0-3)。

**タイムアウトの扱いについての訂正**: 当初案は`recv_timeout`でタイムアウトしたら即座に`ActivationTimeout`エラーとして扱っていたが、`ActivateAudioInterfaceAsync`にキャンセルAPIはなく、「待つのをやめる」ことと「操作そのものを止める」ことは別である。待つのをやめた後にコールバックが遅れて到着した場合、`operation`/`handler`を先に解放していると未定義動作の恐れがある。そのため既定では**ハードタイムアウトを設けず**、`recv()`で完了まで無条件にブロックする。スパイクの手動検証中に異常な遅延に気づけるよう、別スレッドの監視ログ(5秒・15秒・30秒経過時に警告ログを出すだけで、待機自体は止めない)を添える。診断目的でハードタイムアウトを使いたい場合はオプション化し、その場合は完了ハンドラ側に「期限切れ」を伝えて安全に後始末する(後述)。

```rust
// process_loopback.rs

pub fn activate_process_loopback_client(
    target_pid: u32,
    mode: ProcessLoopbackMode, // Include | Exclude
    hard_timeout: Option<std::time::Duration>, // 既定はNone(タイムアウトなし)。§5.4本文参照
) -> Result<IAudioClient, SpikeError> {
    // 1. AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS を構築
    let params = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
        TargetProcessId: target_pid,
        ProcessLoopbackMode: match mode {
            ProcessLoopbackMode::Include => PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            ProcessLoopbackMode::Exclude => PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        },
    };
    let activation_params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 { ProcessLoopbackParams: params },
    };

    // 2. PROPVARIANT(VT_BLOB)にactivation_paramsを詰める
    let mut prop = PROPVARIANT::default();
    // vt = VT_BLOB, blob.cbSize = size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>(),
    // blob.pBlobData = &activation_params as *const _ as *mut u8
    // (windows crateではPROPVARIANTのunion操作がunsafeになるため、
    //  ヘルパー関数 make_blob_propvariant(&activation_params) を用意する)

    // 3. 完了ハンドラを用意。expiredフラグは「呼び出し元が待つのをやめたか」を
    //    ハンドラ側へ伝えるためのもの(既定のNoneモードでは常にfalseのまま)。
    let expired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<windows::core::Result<IUnknown>>();
    let handler: IActivateAudioInterfaceCompletionHandler =
        CompletionHandler::new(tx, expired.clone()).into(); // #[implement]マクロで生成した型をIUnknownへ変換

    // 4. 呼び出し
    let mut operation: Option<IActivateAudioInterfaceAsyncOperation> = None;
    unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, // PCWSTR (L"VAD\\Process_Loopback")
            &IAudioClient::IID,
            Some(&prop),
            &handler,
            &mut operation,
        )?;
    }

    // 5. 完了待ち。
    match hard_timeout {
        None => {
            // 既定経路: 完了まで無条件にブロックする。
            // 呼び出し元(main.rs)は別途、経過時間をログするだけの監視スレッドを
            // 立てておき、異常な遅延に人間が気づけるようにする(操作は止めない)。
            let unknown = rx.recv().map_err(|_| SpikeError::ActivationChannelClosed)??;
            let audio_client: IAudioClient = unknown.cast()?;
            Ok(audio_client)
        }
        Some(timeout) => {
            // 診断用オプション経路(P0-4)。
            match rx.recv_timeout(timeout) {
                Ok(result) => Ok(result?.cast()?),
                Err(_) => {
                    // `expired`を立てても`operation`/`handler`はここでdropしない。
                    // 完了ハンドラが実際に呼ばれるまで生存させる必要があるため、
                    // プロセス内の「保留中アクティベーション置き場」へ移し、
                    // 完了ハンドラ側(§5.5)がexpiredを見て後始末する設計とする。
                    expired.store(true, std::sync::atomic::Ordering::SeqCst);
                    park_pending_activation(operation, handler, expired.clone());
                    Err(SpikeError::ActivationTimeout(timeout))
                }
            }
        }
    }
}
```

`hard_timeout: None`(既定)を使う限り、`park_pending_activation`は呼ばれず、`ActivationTimeout`/`expired`まわりの後始末ロジックはコード上に存在しても実行されない。ハードタイムアウトはスパイクの初期段階では有効化せず、必要になった場合にのみ`--activation-hard-timeout-ms`(§5.8)で明示的に有効化する。

### 5.5 `IActivateAudioInterfaceCompletionHandler`実装

`windows` crateの`#[implement]`マクロ(またはバージョンにより`windows::implement!`)でCOMオブジェクトを実装する。

**エージル性についての追記(P0-3)**: `ActivateAudioInterfaceAsync`の完了コールバックは、我々の呼び出しスレッドとは別の、OS側のスレッドプール/RPCスレッドから呼ばれる。公式C++サンプルは、この呼び出し元スレッドがどのアパートメントであっても直接インターフェースを呼べるよう`FtmBase`(Free-Threaded Marshaler)を継承している。Rustでの等価な対処は、`IAgileObject`(メソードを持たないマーカーインターフェース)を追加実装し、このCOMオブジェクトがエージル(どのアパートメントからでもプロキシなしで直接呼べる)であることを宣言することである。

```rust
// completion_handler.rs

#[windows::core::implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct CompletionHandler {
    tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<windows::core::Result<IUnknown>>>>,
    /// §5.4のハードタイムアウト経路が有効な場合のみtrueになりうる。
    /// 既定(タイムアウトなし)の経路では常にfalseのまま。
    expired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CompletionHandler {
    pub fn new(
        tx: std::sync::mpsc::Sender<windows::core::Result<IUnknown>>,
        expired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self { tx: std::sync::Mutex::new(Some(tx)), expired }
    }
}

// 【実装時の訂正】`_Impl`トレイトは元の構造体(`CompletionHandler`)にではなく、
// `#[implement]`マクロが生成するラッパー型`CompletionHandler_Impl`に対して
// 実装する。`CompletionHandler_Impl`は`Deref`で`CompletionHandler`の
// フィールドへ透過的にアクセスできるため、`self.tx`/`self.expired`はそのまま
// 使える。`windows` 0.58 + `--target x86_64-pc-windows-gnu`での`cargo check`で
// 実機検証した(spikes/spike-02-app-loopback/src/completion_handler.rs)。
// `impl ... for CompletionHandler`(誤)は`CompletionHandler_Impl:
// IActivateAudioInterfaceCompletionHandler_Impl is not satisfied`という
// トレイト境界エラーになる。
impl IActivateAudioInterfaceCompletionHandler_Impl for CompletionHandler_Impl {
    fn ActivateCompleted(
        &self,
        activateoperation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| -> windows::core::Result<IUnknown> {
            let op = activateoperation.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
            let mut hr = windows::core::HRESULT(0);
            let mut activated_interface: Option<IUnknown> = None;
            unsafe { op.GetActivateResult(&mut hr, &mut activated_interface)? };
            hr.ok()?;
            activated_interface.ok_or_else(|| windows::core::Error::from(E_FAIL))
        })();

        if self.expired.load(std::sync::atomic::Ordering::SeqCst) {
            // 呼び出し元は既にタイムアウトとして処理済み(P0-4)。
            // 結果のインターフェースはdrop(=Release)するだけに留め、
            // 存在しないかもしれない受信側へは送らない。
            tracing::warn!("late ActivateAudioInterfaceAsync completion after timeout; discarding");
            // 概念上の後始末: 「保留中アクティベーション置き場」から自身を除去する。
            return Ok(());
        }

        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(result);
        }
        Ok(())
    }
}

// 同じ理由でIAgileObject_Implも生成ラッパー型に対して実装する。
impl IAgileObject_Impl for CompletionHandler_Impl {}
```

**確認ポイント → 実機確認済み(§7参照)**: `windows` 0.58での実際の検証により、以下が判明した。

1. `windows`クレートのfeatureに`"implement"`を追加しないと、`_Impl`トレイト自体が生成されず`#[implement(...)]`が使えない(featureなしではコード全体が`#[cfg(feature = "implement")]`で除外される)。
2. `#[implement]`マクロが生成するコードは`windows_core::...`という直接のクレートパスを参照するため、`windows-core`を(`windows`とは別に)直接の依存として追加する必要がある。
3. `_Impl`トレイトは元の構造体ではなく、マクロが生成する`{構造体名}_Impl`ラッパー型に対して実装する。`IAgileObject`はメソッドを持たないマーカーインターフェースだが、`IAgileObject_Impl: Sized {}`という空のトレイトへの明示的な空implが必要(「実装不要な場合が多い」という当初の想定は成立しない)。

いずれも`cargo check --target x86_64-pc-windows-gnu`(Windows実機なしでもcrateの型チェックは通せる。§7参照)で実際に検出・修正した。

### 5.6 取得後の初期化・キャプチャループ(SPIKE-01との差分)

**フォーマット決定方針の訂正**: 当初案は48kHz/2ch/float32を固定指定し、拒否されたらProcess Loopback自体のNO-GO材料にするとしていた。しかしMicrosoft公式サンプル(`ApplicationLoopback`)は、まず`GetMixFormat`を試し、固定フォーマットを使う場合は`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`(必要なら`AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`も)を付けてOSにフォーマット変換を任せる構成を取っている。`AUTOCONVERTPCM`なしで固定形式を拒否されたことは、Process Loopbackそのものの可否とは無関係な誤検知になるため、次の順序に修正する。

**`IAudioClient::Initialize`失敗後の再試行についての訂正(P0-2)**: 当初案は、`GetMixFormat`経路の`Initialize`が失敗した場合に、**同じ`IAudioClient`オブジェクトへ**固定形式で`Initialize`を再度呼んでいた。Microsoftのドキュメントは、失敗した`Initialize`の後に同じオブジェクトへ再度`Initialize`を呼ぶと`AUDCLNT_E_ALREADY_INITIALIZED`になりうると説明しており、再試行時は**必ず新しく`ActivateAudioInterfaceAsync`をやり直して別のクライアントを取得する**必要がある。

**COM所有権についての訂正(P0-3)**: `activate_process_loopback_client`から`init_and_capture_process_loopback`への`IAudioClient`の受け渡しも、呼び出し元(main.rs)を経由した「スレッド間の受け渡し」にしない。`ProcessLoopbackStream::run`という1つの関数(capture MTAスレッド上で実行される)の中で、Activate→Initialize→GetService→キャプチャループ→Stop→解放までを完結させる。

```rust
// process_loopback.rs (続き)

pub struct ProcessLoopbackStream {
    pub target_pid: u32,
    pub mode: ProcessLoopbackMode,
    pub capture_epoch: u64,
    /// §5.4参照。既定はNone(タイムアウトなし)。
    pub activation_hard_timeout: Option<std::time::Duration>,
    pub pipeline_drop_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl CaptureStream for ProcessLoopbackStream {
    fn stream_id(&self) -> StreamId { StreamId::ProcessLoopback }

    fn run(self: Box<Self>, tx: &Sender<CaptureEvent>, stop: &StopSignal) -> Result<CaptureExit, SpikeError> {
        let _com = ComApartment::new_mta()?; // このスレッドでのみCOMを初期化する

        let (audio_client, format_info) = activate_and_initialize_with_retry(
            self.target_pid, self.mode, self.activation_hard_timeout,
        )?;

        // 以降はSPIKE-01の共通ループ本体(§4.4の`init_and_capture`のステップ5以降)と同一。
        // StreamId::ProcessLoopback、target_pid: Some(self.target_pid)、capture_epochを
        // CapturedFrameRecordへ渡す点のみSPIKE-01と異なる。
        run_capture_loop(
            audio_client, StreamId::ProcessLoopback, Some(self.target_pid),
            self.capture_epoch, format_info, self.pipeline_drop_counter, tx, stop,
        )
    }
}

/// 「Activate→Initialize」を1つの再試行可能な単位として扱う。
/// GetMixFormat経路が失敗した場合、**同じクライアントを使い回さず**、
/// 新しくActivateし直したクライアントで固定形式+AUTOCONVERTPCMを試す。
fn activate_and_initialize_with_retry(
    target_pid: u32,
    mode: ProcessLoopbackMode,
    activation_hard_timeout: Option<std::time::Duration>,
) -> Result<(IAudioClient, AudioFormatInfo), SpikeError> {
    // 試行1: GetMixFormat経路
    let client_a = activate_process_loopback_client(target_pid, mode, activation_hard_timeout)?;
    let mix_format_result = unsafe { client_a.GetMixFormat() };
    if let Ok(fmt) = mix_format_result {
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if unsafe { client_a.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &fmt, None) }.is_ok() {
            return Ok((client_a, AudioFormatInfo::from_waveformatex(&fmt)));
        }
    }
    // client_aはInitialize未実施または失敗済み。ここで破棄し、
    // 二度とこのオブジェクトへInitializeを呼ばない。
    drop(client_a);

    // 試行2: 新しくActivateし直し、固定形式+AUTOCONVERTPCMを試す。
    let client_b = activate_process_loopback_client(target_pid, mode, activation_hard_timeout)?;
    let fixed_format = build_fixed_format_48k_stereo_f32();
    let fixed_flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe { client_b.Initialize(AUDCLNT_SHAREMODE_SHARED, fixed_flags, 0, 0, &fixed_format, None)? };
    Ok((client_b, AudioFormatInfo::from_waveformatex(&fixed_format)))
}
```

`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`なしで固定形式を拒否された事象だけをもってNO-GO判断はしない。GetMixFormat経路・AUTOCONVERTPCM経路の両方(=試行1・試行2の両方が、それぞれ新しいクライアントで)失敗した場合にのみ、真の環境制約としてRESULT.mdへ記録する。

### 5.7 プロセス再起動検出・再アタッチ

停止中の`WaitForSingleObject`タイムアウト(最大2秒)待ちの間に旧スレッドのイベントハンドルと新スレッドのイベントハンドルが混在する事故を避けるため、§3.8の`StopSignal`(即時停止)を前提に、次の順序を**固定**する。順序を守らず新スレッドを先に起動すると、旧COMオブジェクト解放前に新しい`ActivateAudioInterfaceAsync`を呼ぶことになり、状態の混線やハンドルリークにつながる。

§5.6のP0-3修正により、`IAudioClient`等のCOMオブジェクトはすべて各capture MTAスレッドの内部で生成・解放されるため、main.rs側は**PID・capture_epoch・タイムアウト設定といった値だけ**を持ち、生きたCOMオブジェクトは一切保持しない。

```text
1. stop signal を現在のcapture threadへ送る(StopSignal::signal)
2. capture threadのJoinHandle::join()で完全停止を待つ
   (このスレッドの内部でIAudioClient/IAudioCaptureClient/イベントハンドルの
   Stop・解放・CoUninitializeまで完結している)
3. join()が返すCaptureThreadOutcome(§3.8)を確認する
4. capture_epochをインクリメントする
5. 新PIDを持つProcessLoopbackStreamを構築し、新しいcapture threadをspawnする
   (Activate〜Initializeは新スレッドの内部で行われる。main.rsはActivateを呼ばない)
```

**ライフサイクル確認の経路についての訂正(P1)**: 当初案は「共有チャネルから届く`CaptureEvent::StreamStopped`を確認する」としていたが、そのチャネルの受信側(`rx`)は`Aggregator`が所有しており、main.rsが同じイベントを確実に見られる保証がない。そのため、main.rsの制御フローは`CaptureEvent`チャネルを経由せず、**`spawn_capture_thread`が返す`JoinHandle<CaptureThreadOutcome>`の`join()`戻り値**を直接使う(§3.8)。共有チャネル側の`StreamStopped`は、Aggregatorが統計・ログ(`mmcss_applied`を含む)を記録するためだけに使う副次的な経路として残す。

```rust
// main.rs 内のオーケストレーション(概略)

let mut capture_epoch: u64 = 0;

loop {
    match watcher.poll() {
        ProcessWatchEvent::StillAlive(_) => { /* 何もしない */ }
        ProcessWatchEvent::Exited { old_pid } => {
            log_process_event(ProcessEvent::Exited { old_pid, capture_epoch });
            // 1〜3: 既存スレッドを完全に停止させてから次のイベントへ進む。
            // COMオブジェクトの解放は当該スレッド内部で完結済み。
            // join()の戻り値(CaptureThreadOutcome)を制御フローの正とする。
            stop_signal.signal()?;
            let outcome = current_thread.take().unwrap().join();
            log_process_event(ProcessEvent::CaptureThreadJoined { capture_epoch, outcome: format!("{:?}", outcome) });
            // Remote側にsilenceを挿入すべき区間としてマーキング
            // (実際のsilence挿入自体はSPIKE-03の責務。ここではイベント記録のみ)
        }
        ProcessWatchEvent::Restarted { old_pid, new_pid } => {
            log_process_event(ProcessEvent::Restarted { old_pid, new_pid, capture_epoch });
            // 旧スレッドがまだ動いている場合は先に停止・joinする(Exitedと同じ1〜3)
            stop_signal.signal()?;
            current_thread.take().map(|h| h.join());
            // ここでもjoin()の戻り値を確認できるが、再アタッチを急ぐため
            // ログ記録のみに留める(ブロッキングはjoin()自体で既に発生している)

            // 4〜5: 新スレッドはPID/epoch/タイムアウト設定という「値」だけを受け取り、
            // Activate〜Initializeは新スレッドの内部(§5.6)で行う。
            capture_epoch += 1;
            let new_stop_signal = StopSignal::new()?;
            let stream = Box::new(ProcessLoopbackStream {
                target_pid: new_pid,
                mode,
                capture_epoch,
                activation_hard_timeout, // 既定None。CLIで明示指定した場合のみSome
                pipeline_drop_counter: pipeline_drop_counter.clone(), // ストリーム単位で使い回す
            });
            current_thread = Some(spawn_capture_thread(stream, tx.clone(), new_stop_signal.clone()));
            stop_signal = new_stop_signal;
        }
        ProcessWatchEvent::NotFound => { /* 一定回数で監視終了しユーザーへ通知 */ }
    }
    std::thread::sleep(Duration::from_secs(1));
}
```

各`CapturedFrameRecord`は`capture_epoch`と`target_pid`を保持しているため(§3.2)、CSVを走査するだけで「どの世代・どのPIDのデータか」を機械的に区別でき、再アタッチ前後でデータが混線していないかを解析時に検証できる(§4.9のP0-5修正でepoch単位に解析を分割する)。

design.md §16.2(会議アプリ再起動時のRemote探索)の本番挙動を先取り検証する位置づけであり、ここでの観測結果(無音になるか、エラーになるか、ストリームが自動継続するか)がspike-plan.md合否基準の「対象プロセス再起動を検出し再アタッチできる」を判定する材料になる。

### 5.8 CLIインターフェース

```rust
#[derive(clap::Parser)]
struct Cli {
    /// 対象プロセスの実行ファイル名(例: "Zoom.exe", "ms-teams.exe", "chrome.exe")。
    /// --target-pidと排他。
    #[arg(long, conflicts_with = "target_pid")]
    target_process: Option<String>,

    /// 対象プロセスのPIDを直接指定する。マルチプロセスアプリで名前解決の曖昧さを
    /// 避けたい場合に使う。指定時はプロセス再起動時の自動再アタッチが効かない
    /// (再起動でPIDが変わるため、その場合は--target-processを使う)。
    #[arg(long, conflicts_with = "target_process")]
    target_pid: Option<u32>,

    /// --target-process指定時に複数候補が見つかった場合の選択戦略
    #[arg(long, value_enum, default_value_t = ProcessSelectionStrategy::Root)]
    process_selection: ProcessSelectionStrategy,

    /// プロセスツリーを含めるか除外するか
    #[arg(long, value_enum, default_value_t = LoopbackModeArg::Include)]
    mode: LoopbackModeArg,

    #[arg(long, default_value_t = 600)]
    duration_secs: u64,

    /// ActivateAudioInterfaceAsyncの完了待ちにハードタイムアウトを設ける(診断用)。
    /// 省略時(既定)はタイムアウトを設けず、完了まで無条件に待つ(§5.4参照)。
    #[arg(long)]
    activation_hard_timeout_ms: Option<u64>,

    /// プロセス再起動時の自動再アタッチを有効にする(--target-process指定時のみ有効)
    #[arg(long, default_value_t = true)]
    reattach: bool,

    #[arg(long)]
    output_dir: Option<PathBuf>,
}
```

実行例:

```powershell
spike-02-app-loopback.exe --target-process Zoom.exe --mode include --duration-secs 600
spike-02-app-loopback.exe --target-process chrome.exe --process-selection foreground --duration-secs 300
spike-02-app-loopback.exe --target-pid 12345 --mode include --duration-secs 600
```

複数候補が見つかった場合(例: Chromeの多プロセス構造)、選択されなかった候補も含めて全候補(PID・親PID・開始時刻)を`process_events.jsonl`(§5.9)へ`process_candidates_found`として記録する。

### 5.9 出力成果物

```text
spikes/spike-02-app-loopback/out/{run_id}/
├─ process_loopback.csv
├─ process_loopback.wav
├─ process_events.jsonl   # process_exited/process_restarted等のイベントログ
└─ summary.json
```

`process_events.jsonl`の1行例:

```json
{"ts_ns": 1720400000000000, "type": "process_restarted", "old_pid": 4821, "new_pid": 5990}
```

`summary.json`には§4.8のスキーマに加え、Process Loopback特有の項目として次を含める。

```json
{
  "idle_timeout_count": 12,
  "idle_timeout_note": "対象アプリが無音の間、通知が来ずタイムアウトした回数。エラーではない(§4.4)"
}
```

`idle_timeout_count`が多いこと自体は失格要件ではない(対象アプリが無音であっただけの可能性が高い)。ただし、会議中に音声が流れていたはずの区間でこの値が大きい場合は、Process Loopbackが実際には音声を取得できていない兆候として、`process_loopback.wav`の該当区間を確認する。

### 5.10 実行手順(spike-plan.md手順との対応)

1. Zoomテスト会議に参加し、`--target-process Zoom.exe --mode include`で実行
2. YouTube等の別音声を同時再生し、`process_loopback.wav`に混入していないか確認
3. Chromeで同様に`--target-process chrome.exe`を実行し、他タブ音声混入の有無を`process_events.jsonl`と聴感確認から記録(混入は許容前提だが実態を記録するのが目的)
4. Zoomを再起動し、`process_events.jsonl`に`process_restarted`が記録されるか、再アタッチ後に音声が復帰するかを確認
5. `RESULT.md`へGO / CONDITIONAL-GO / NO-GOを記入(spike-plan.md §Wave1/SPIKE-02の判定基準に従う)

### 5.11 CONDITIONAL-GO時のフォールバック実装メモ

spike-plan.mdは「C++サンプル経由(C++/Rust FFI)でのみ動作する場合はCONDITIONAL-GO」としている。その場合の設計方針(実装はSPIKE-02の結果を見てから着手するため、ここでは方針のみ記載):

* C++側は公式`ApplicationLoopback`サンプルを最小限に切り出した静的ライブラリとし、`extern "C"`でPCMフレームとタイムスタンプをコールバック関数ポインタ経由でRustへ渡す
* Rust側は`build.rs` + `cc` crateでC++をビルドし、FFI境界の型は`CapturedFrameRecord`と同一構造を持つPOD構造体に変換する
* このFFI境界はdesign.md §6.1「必要箇所のみC++」の対象範囲を確定する実測材料になるため、CONDITIONAL-GOの場合はRESULT.mdに具体的な境界(関数シグネチャ)を記録する

---

## 6. 依存クレート一覧(再掲・補足)

| クレート | 用途 | 備考 |
|---|---|---|
| `windows` | WASAPI, COM, ActivateAudioInterfaceAsync | featureフラグは§2参照 |
| `sysinfo` | プロセス列挙・監視(SPIKE-02) | ToolHelp32直叩きより実装コストが低い |
| `crossbeam-channel` | キャプチャスレッド→集約スレッドのイベント転送 | `std::sync::mpsc`でも代替可 |
| `csv` | フレームメタデータ出力 | |
| `hound` | WAV書き出し(聴感確認用) | float32 WAVをサポート |
| `clap` | CLI引数 | derive機能を使用 |
| `thiserror` / `anyhow` | エラー型 | ライブラリ側はthiserror、バイナリ側はanyhow |
| `tracing` / `tracing-subscriber` | 実行ログ | 音声内容は出力しない(design.md §17.2方針をスパイクでも踏襲) |
| `serde` / `serde_json` | summary.json出力 | |

---

## 7. 既知の不確実性・実装時の確認ポイント

spike-plan.mdが「検証すべき仮説」として挙げている不確実性とは別に、詳細設計時点で判明した実装レベルの確認事項を残す。

1. **`windows` crateのAPI所在パス**: `ActivateAudioInterfaceAsync`、`AUDIOCLIENT_ACTIVATION_PARAMS`、`PROCESS_LOOPBACK_MODE`等のモジュールパス・フィールド名はcrateバージョンで変動しうる。実装開始時に固定バージョンを`Cargo.lock`で確定し、`cargo doc`で実際のシグネチャを1次情報として確認すること。本文書のコード例は設計意図を示す疑似シグネチャであり、コンパイル可能なコードそのものではない。
   * **`windows` 0.58について実機検証済みの事項**(`spikes/`ワークスペースの雛形を`cargo check --target x86_64-pc-windows-gnu`で検証して判明): `AUDCLNT_STREAMFLAGS_*`系の定数は(`HANDLE`のような newtype ラッパーではなく)素の`u32`であり`.0`アクセサは不要。`#[implement(...)]`を使うには`windows`の`"implement"` featureが必須で、かつ`_Impl`トレイトは元の構造体ではなく生成される`{構造体名}_Impl`ラッパー型に実装する(§5.5)。`windows-core`は`windows`とは別に直接の依存として追加する必要がある(§2)。`IUnknown::cast()`を呼ぶには`windows_core::Interface`(`windows::core::Interface`)をスコープに入れる必要がある。
2. **PROPVARIANT(VT_BLOB)構築の安全性**: `windows` crateの`PROPVARIANT`はunion型でありunsafe操作が必要。`windows::Win32::System::Com::StructuredStorage`配下のヘルパー(`PropVariantToXxx`系)や、生の`Anonymous`フィールド操作のどちらが安定して使えるかは実装時に検証する。
3. **Process Loopback時のフォーマット制約**: §5.6で`GetMixFormat`優先・`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`フォールバックへ修正済みだが、それでも初期化に失敗するケースが実機で起きうるかは未検証。両経路とも失敗した場合のみ環境制約として記録する。
4. **`IActivateAudioInterfaceCompletionHandler`のCOM生存期間**: `ActivateAudioInterfaceAsync`は非同期であるため、`handler`オブジェクトが完了コールバックが呼ばれるまで解放されないようにする(`Box`やAtomic参照カウントで明示的に生存期間を延ばす、または`windows` crateの生成物がIUnknown参照カウントを正しく持つことを確認する)。
5. **MMCSSタスク名の正確性**: `"Pro Audio"`はMicrosoftドキュメント記載のタスク名だが、レジストリの`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile\Tasks`に依存するため、対象実機で有効なタスク名一覧を確認する。
6. **Process Loopback対応OSビルドの記載揺れ**: Microsoft公式サンプルのREADMEは「Windows 10 build 20348以降」を要件とする一方、APIリファレンス側の記述では20438以降とされる版がある。§3.10の`check_process_loopback_support`で両閾値を記録し、`summary.json`の`os.build`を必ず残す。実機検証時にどちらの閾値が実態と一致するかをRESULT.mdへ記録し、design.md §5.1の要件記述(Windows 11前提)の妥当性を裏付ける。

---

## 8. RESULT.mdへの接続

各スパイク実行後、以下を`spikes/spike-0X-*/RESULT.md`(spike-plan.md §1.2フォーマット)へ転記する。

* SPIKE-01: `summary.json`の`acceptance`ブロック、`mic.wav`/`loopback.wav`の聴感確認結果
* SPIKE-02: `process_events.jsonl`から抽出したプロセス再起動時の挙動、Chrome他タブ混入の有無、CONDITIONAL-GO該当時はFFI境界案(§5.11)

---

## 9. 見積りとの対応

| スパイク | spike-plan.md記載タイムボックス | 本設計での想定内訳(目安) |
|---|---|---|
| SPIKE-01 | 3日 | 共通基盤(`spike-common`)構築 1日 / マイク+Loopback実装 1日 / 実機検証・解析・RESULT.md 1日 |
| SPIKE-02 | 3日 | Activate/COM実装 1.5日 / プロセス監視・実機検証(Zoom/Teams/Chrome) 1日 / 再起動シナリオ・RESULT.md 0.5日 |

`spike-common`はSPIKE-01の1日目に構築するため、SPIKE-02側の実装コストは主に`ActivateAudioInterfaceAsync`まわりに集中する見込み。ただし§10の推奨実装順序では、リスクの高い箇所(Process Loopback Activate)を先に単独で潰し、`spike-common`への抽出は両スパイクが動いた後に行う順序を取るため、日ごとの作業内容はここでの割り振りと前後する。

---

## 10. 推奨実装順序

本設計は「マイク+Endpoint Loopbackが先、Process Loopbackが後」という機能単位の説明順になっているが、実装に着手する順序は**リスクの高い箇所を先に単独で検証する**ことを優先し、次の順序を推奨する。設計セクションの記述順どおりに上から実装する必要はない。

```text
1. 公式C++ ApplicationLoopbackサンプル(Windows-classic-samples)を対象実機でそのままビルド・実行し、
   OS build・Zoom/Teams/Chromeでの挙動のベースラインを確立する(§7-6のOSビルド確認を含む)
2. windows crateでProcess LoopbackのActivateAudioInterfaceAsyncだけを行う最小プログラムを作る
   (§5.4/§5.5相当。GetBuffer/ReleaseBufferループはまだ書かない)
   → ここが最大の不確実点(§7-1, §7-2)なので、他の実装より先に成立可否を確認する
3. SPIKE-01の単一マイクキャプチャ(§4.4のinit_and_captureをMicのみで)を実装する
   → CapturePacketGuard、wake_seq/packet_seq分離、StopSignalをこの時点で作り込む
4. QPC単調性チェック・device_position_framesのgap検出(§4.9)を完成させ、
   単一ストリームで正しく統計が取れることを確認する
5. Endpoint Loopbackを追加し、2ストリーム同時実行・drift回帰(§4.9)を検証する
6. ここで初めて、マイク/Loopback/Process Loopbackで共通化できる部分を
   `spike-common`(§3)へ抽出する。抽出を最初に行わないのは、実装の初期段階では
   「何が本当に共通化できるか」が確定しておらず、時期尚早な抽象化を避けるため
7. Process Loopbackの本実装(§5.6の書式ネゴシエーション込み)とプロセス監視・
   再アタッチ(§5.7)を追加する
```

この順序は、spike-plan.mdのタイムボックス(SPIKE-01: 3日、SPIKE-02: 3日)を変えるものではないが、「共通基盤を先に作り込んでからキャプチャを実装する」のではなく「小さく動くものを積み上げてから共通化する」順序を取ることで、§7で指摘した不確実性(特にProcess Loopback Activate)を早期に検証し、NO-GO判定が必要な場合の手戻りを最小化する。
