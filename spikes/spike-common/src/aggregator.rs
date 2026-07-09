// spike-windows-01-02-detail-design.md §4.6/§5.2
//
// AudioFormatInfoはStreamStartedイベントが届くまで分からないため、CSVは即座に
// 開けてもWAVライターはStreamStarted受信後まで生成できない。両者をOptionで
// 保持するStreamSinkにまとめる。
//
// SPIKE-01(マイク/Endpoint Loopback)専用に実装したものを、SPIKE-02(Process
// Loopback)からも再利用できるよう、`Aggregator::new`が任意の(StreamId, ファイル名)
// の組を受け取る形へ一般化してspike-commonへ格上げした(§5.2の推奨どおり)。

use crate::csv_log::FrameCsvWriter;
use crate::frame_record::{CapturedFrameRecord, StreamId};
use crate::wav_writer::PcmWavWriter;
use crate::{AudioFormatInfo, CaptureEvent};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Default)]
pub struct StreamStats {
    pub wake_events: u64,
    pub packet_events: u64,
    pub discontinuity_count: u64,
    pub silent_count: u64,
    pub timestamp_error_count: u64,
    /// wake_seqごとのwake_qpc_100ns差分列(コールバックジッタ計算用)。
    /// packet_seq側の重複値は含めない。
    pub wake_qpc_100ns_series: Vec<u64>,
    pub last_wake_seq: Option<u64>,
    /// (capture_qpc_100ns, device_position_frames) の系列。
    /// gap検出とクロックdrift回帰の両方に使う(§4.9)。単調性違反も、この系列の
    /// qpc値を呼び出し側でanalyze::detect_monotonic_violationsへ渡して計算する
    /// (StreamStats自身では集計しない)。
    pub position_series: Vec<(u64, u64)>,
    pub total_frames_captured: u64,
    /// CaptureEvent::StreamStopped受信時に埋める。スレッド開始時点では未確定
    /// なため、Option<bool>にして「まだ終了していない」と「MMCSS適用に失敗した」
    /// を区別する。
    pub mmcss_applied: Option<bool>,
    /// Process Loopbackで対象アプリが無音の間、通知が来ずタイムアウトを
    /// 繰り返した回数(§5.9)。マイク/Endpoint Loopbackでは常に0のまま。
    pub idle_timeout_count: u64,
    /// SPIKE-09: IAudioSessionEvents::OnSessionDisconnectedを観測した回数と
    /// 直近の理由(生値)。デバイス切断・スリープ・排他モード奪取等の検出に使う。
    pub session_disconnected_count: u64,
    pub last_session_disconnect_reason_raw: Option<i32>,
}

impl StreamStats {
    pub fn update(&mut self, record: &CapturedFrameRecord) {
        self.packet_events += 1;
        if self.last_wake_seq != Some(record.wake_seq) {
            self.wake_events += 1;
            self.wake_qpc_100ns_series.push(record.wake_qpc_100ns);
            self.last_wake_seq = Some(record.wake_seq);
        }
        if record.discontinuity {
            self.discontinuity_count += 1;
        }
        if record.silent {
            self.silent_count += 1;
        }
        if record.timestamp_error {
            self.timestamp_error_count += 1;
        }
        self.total_frames_captured += record.frame_count as u64;
        self.position_series
            .push((record.capture_qpc_100ns, record.device_position_frames));
    }
}

pub struct StreamSink {
    csv: FrameCsvWriter,
    /// StreamStarted受信時に生成する
    wav: Option<PcmWavWriter>,
    format: Option<AudioFormatInfo>,
    device_id: Option<String>,
    device_friendly_name: Option<String>,
    pub stats: StreamStats,
    /// analyze::group_records_by_epoch等、CapturedFrameRecordのスライスを
    /// 要求する関数へそのまま渡すために、CSVへ書いたレコードを複製して保持する
    /// (§4.9)。CSVを書き出し後に読み直す構成にはしていない。
    records: Vec<CapturedFrameRecord>,
    wav_path: PathBuf,
}

/// Aggregator::runが返す、summary.json構築に必要なストリームごとの結果。
pub struct StreamCaptureResult {
    pub format: Option<AudioFormatInfo>,
    pub device_id: Option<String>,
    pub device_friendly_name: Option<String>,
    pub stats: StreamStats,
    pub records: Vec<CapturedFrameRecord>,
}

impl Default for StreamCaptureResult {
    fn default() -> Self {
        Self {
            format: None,
            device_id: None,
            device_friendly_name: None,
            stats: StreamStats::default(),
            records: Vec::new(),
        }
    }
}

pub struct Aggregator {
    sinks: HashMap<StreamId, StreamSink>,
}

impl Aggregator {
    /// `streams`: (StreamId, CSV/WAVファイル名の接頭辞)の組。
    /// SPIKE-01は`[(StreamId::Mic, "mic"), (StreamId::EndpointLoopback, "loopback")]`、
    /// SPIKE-02は`[(StreamId::ProcessLoopback, "process_loopback")]`を渡す。
    pub fn new(out_dir: &std::path::Path, streams: &[(StreamId, &str)]) -> anyhow::Result<Self> {
        let mut sinks = HashMap::new();
        for (stream, name) in streams.iter().copied() {
            let csv = FrameCsvWriter::create(&out_dir.join(format!("{name}.csv")))?;
            sinks.insert(
                stream,
                StreamSink {
                    csv,
                    wav: None,
                    format: None,
                    device_id: None,
                    device_friendly_name: None,
                    stats: StreamStats::default(),
                    records: Vec::new(),
                    wav_path: out_dir.join(format!("{name}.wav")),
                },
            );
        }
        Ok(Self { sinks })
    }

    pub fn run(
        mut self,
        rx: crossbeam_channel::Receiver<CaptureEvent>,
    ) -> anyhow::Result<HashMap<StreamId, StreamCaptureResult>> {
        for event in rx {
            match event {
                CaptureEvent::StreamStarted {
                    stream,
                    format,
                    qpc_freq_hz: _,
                    device_id,
                    device_friendly_name,
                } => {
                    let Some(sink) = self.sinks.get_mut(&stream) else {
                        tracing::warn!(?stream, "StreamStarted for unregistered stream; ignoring");
                        continue;
                    };
                    sink.format = Some(format.clone());
                    sink.device_id = Some(device_id);
                    sink.device_friendly_name = Some(device_friendly_name);
                    // 全ストリーム共通でinterleaved float32(IEEE float WAV)に
                    // 統一する。ネイティブビット幅を保存する経路は用意しない。
                    sink.wav = Some(PcmWavWriter::create_from_format(
                        &sink.wav_path,
                        format.channels,
                        format.sample_rate,
                    )?);
                }
                CaptureEvent::Frame { record, samples } => {
                    let Some(sink) = self.sinks.get_mut(&record.stream) else {
                        tracing::warn!(stream = ?record.stream, "Frame for unregistered stream; ignoring");
                        continue;
                    };
                    sink.csv.write(&record)?;
                    if let Some(wav) = sink.wav.as_mut() {
                        wav.write_samples(&samples)?;
                    }
                    sink.stats.update(&record);
                    sink.records.push(record);
                }
                CaptureEvent::StreamError { stream, error } => {
                    tracing::warn!(?stream, %error, "stream error");
                }
                CaptureEvent::StreamStopped {
                    stream,
                    exit,
                    mmcss_applied,
                } => {
                    tracing::info!(?stream, ?exit, mmcss_applied, "stream stopped");
                    if let Some(sink) = self.sinks.get_mut(&stream) {
                        sink.stats.mmcss_applied = Some(mmcss_applied);
                    }
                }
                CaptureEvent::IdleTimeoutObserved {
                    stream,
                    idle_timeout_count,
                } => {
                    if let Some(sink) = self.sinks.get_mut(&stream) {
                        sink.stats.idle_timeout_count = idle_timeout_count;
                    }
                }
                CaptureEvent::SessionDisconnected { stream, reason_raw } => {
                    tracing::warn!(?stream, reason_raw, "session disconnected");
                    if let Some(sink) = self.sinks.get_mut(&stream) {
                        sink.stats.session_disconnected_count += 1;
                        sink.stats.last_session_disconnect_reason_raw = Some(reason_raw);
                    }
                }
            }
        }

        let mut results = HashMap::new();
        for (stream, mut sink) in self.sinks.into_iter() {
            sink.csv.flush().ok();
            if let Some(wav) = sink.wav {
                wav.finalize().ok();
            }
            results.insert(
                stream,
                StreamCaptureResult {
                    format: sink.format,
                    device_id: sink.device_id,
                    device_friendly_name: sink.device_friendly_name,
                    stats: sink.stats,
                    records: sink.records,
                },
            );
        }
        Ok(results)
    }
}
