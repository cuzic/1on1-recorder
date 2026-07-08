// spike-windows-01-02-detail-design.md §4.6
//
// AudioFormatInfoはStreamStartedイベントが届くまで分からないため、CSVは即座に
// 開けてもWAVライターはStreamStarted受信後まで生成できない。両者をOptionで
// 保持するStreamSinkにまとめる。

use spike_common::csv_log::FrameCsvWriter;
use spike_common::frame_record::{CapturedFrameRecord, StreamId};
use spike_common::wav_writer::PcmWavWriter;
use spike_common::{AudioFormatInfo, CaptureEvent};
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
    /// packet_seq側の重複値は含めない(§4.4参照)。
    pub wake_qpc_100ns_series: Vec<u64>,
    pub last_wake_seq: Option<u64>,
    /// (capture_qpc_100ns, device_position_frames) の系列。
    /// gap検出とクロックdrift回帰の両方に使う(§4.9)。
    pub position_series: Vec<(u64, u64)>,
    pub monotonic_violations: u64,
    pub total_frames_captured: u64,
    /// CaptureEvent::StreamStopped受信時に埋める。スレッド開始時点では未確定
    /// なため、Option<bool>にして「まだ終了していない」と「MMCSS適用に失敗した」
    /// を区別する。
    pub mmcss_applied: Option<bool>,
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
    pub stats: StreamStats,
    wav_path: PathBuf,
}

pub struct Aggregator {
    sinks: HashMap<StreamId, StreamSink>,
}

impl Aggregator {
    pub fn new(out_dir: &std::path::Path) -> anyhow::Result<Self> {
        let mut sinks = HashMap::new();
        for (stream, name) in [
            (StreamId::Mic, "mic"),
            (StreamId::EndpointLoopback, "loopback"),
        ] {
            let csv = FrameCsvWriter::create(&out_dir.join(format!("{name}.csv")))?;
            sinks.insert(
                stream,
                StreamSink {
                    csv,
                    wav: None,
                    format: None,
                    stats: StreamStats::default(),
                    wav_path: out_dir.join(format!("{name}.wav")),
                },
            );
        }
        Ok(Self { sinks })
    }

    pub fn run(mut self, rx: crossbeam_channel::Receiver<CaptureEvent>) -> anyhow::Result<()> {
        for event in rx {
            match event {
                CaptureEvent::StreamStarted {
                    stream,
                    format,
                    qpc_freq_hz: _,
                } => {
                    let sink = self.sinks.get_mut(&stream).unwrap();
                    sink.format = Some(format.clone());
                    // 全ストリーム共通でinterleaved float32(IEEE float WAV)に
                    // 統一する。ネイティブビット幅を保存する経路は用意しない。
                    sink.wav = Some(PcmWavWriter::create_from_format(
                        &sink.wav_path,
                        format.channels,
                        format.sample_rate,
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
                CaptureEvent::StreamStopped {
                    stream,
                    exit,
                    mmcss_applied,
                } => {
                    tracing::info!(?stream, ?exit, mmcss_applied, "stream stopped");
                    self.sinks.get_mut(&stream).unwrap().stats.mmcss_applied = Some(mmcss_applied);
                }
            }
        }

        for (_, mut sink) in self.sinks.into_iter() {
            sink.csv.flush().ok();
            if let Some(wav) = sink.wav {
                wav.finalize().ok();
            }
        }
        Ok(())
    }
}
