// spike-windows-01-02-detail-design.md §4.7/§4.11
// spike-plan.md SPIKE-09: デバイス変更・スリープ・Bluetooth の挙動観測。
// 「SPIKE-01のハーネスへイベントログを追加」する形で、以下を実装している。
//   - IMMNotificationClientによるデバイス変更観測(device_events.jsonl)
//   - IAudioSessionEvents::OnSessionDisconnectedの観測(capture_loop側)
//   - AUDCLNT_E_DEVICE_INVALIDATEDの検出→CaptureExit::DeviceLostへの変換
//   - 検出後の「停止→再解決→再初期化」という最小限の自動復帰

mod device_select;
mod loopback_stream;
mod mic_stream;
mod wasapi_common;

use clap::Parser;
use device_select::DeviceRole;
use loopback_stream::EndpointLoopbackStream;
use mic_stream::MicCaptureStream;
use spike_common::aggregator::{Aggregator, StreamCaptureResult, StreamStats};
use spike_common::analyze;
use spike_common::device_watch::{DeviceWatch, DeviceWatchEvent};
use spike_common::frame_record::StreamId;
use spike_common::jsonl_log::JsonlWriter;
use spike_common::os_check;
use spike_common::spawn_capture_thread;
use spike_common::{CaptureEvent, CaptureExit, CaptureThreadOutcome, CaptureStream, StopSignal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    #[arg(long, value_enum, default_value_t = DeviceRole::Console)]
    device_role: DeviceRole,

    /// 出力先ディレクトリ。省略時は out/{timestamp} を自動生成
    #[arg(long)]
    output_dir: Option<PathBuf>,

    /// キャプチャコールバックのタイムアウト(ms)。WaitForMultipleObjectsに渡す。
    #[arg(long, default_value_t = 2000)]
    callback_timeout_ms: u32,

    /// 利用可能なcapture/renderデバイス一覧をJSONで標準出力へ表示して終了する
    /// (§4.3)。--mic-device/--render-deviceへ渡すidを確認する用途。
    #[arg(long, default_value_t = false)]
    list_devices: bool,

    /// spike-plan.md SPIKE-09: デバイス消失(AUDCLNT_E_DEVICE_INVALIDATED /
    /// session disconnect)を検出したあと、同じdevice_id_or_defaultで
    /// 再解決・再初期化を試みる最大回数(ストリームごと)。0で無効化。
    #[arg(long, default_value_t = 10)]
    max_recovery_attempts: u32,
}

/// spike-plan.md SPIKE-01の合否基準/design.md §3.2から逆算した閾値。
/// §4.10(spike-windows-01-02-detail-design.md)の訂正どおり、
/// 600秒 × 167ppm ≒ 100msが「無補正で許容するdrift」の目安値。
const DRIFT_PPM_TARGET_ABS: f64 = 167.0;
/// wake jitter p99の初期目安(バッファ長の2倍未満、§4.10)。
/// 本実装ではhnsBufferDuration=0でOSにバッファ長を委ねておりGetDevicePeriod
/// を問い合わせていないため、代わりに一般的な既定エンジン周期(10ms)の2倍を
/// 暫定閾値として使う。
const WAKE_JITTER_P99_MS_TARGET: f64 = 20.0;

fn role_str(role: DeviceRole) -> &'static str {
    match role {
        DeviceRole::Console => "console",
        DeviceRole::Multimedia => "multimedia",
        DeviceRole::Communications => "communications",
    }
}

/// StreamCaptureResultからsummary.jsonの`streams.<name>`ブロックを構築する。
/// SPIKE-09の復帰シナリオでは1ストリームが複数epochを持ちうる(デバイス消失の
/// たびに+1)ため、group_records_by_epochで世代ごとに分割してから解析する
/// (§4.9のP0-5)。
fn build_stream_summary(result: &StreamCaptureResult) -> serde_json::Value {
    let stats = &result.stats;
    let groups = analyze::group_records_by_epoch(&result.records);
    let group = groups.first();

    let position_gaps = groups
        .iter()
        .map(|g| analyze::detect_position_gaps(&g.records))
        .fold(analyze::PositionGapStats::default(), |mut acc, g| {
            acc.gap_frames_total += g.gap_frames_total;
            acc.overlap_frames_total += g.overlap_frames_total;
            acc.gap_events += g.gap_events;
            acc.overlap_events += g.overlap_events;
            acc
        });

    let nominal_sample_rate = result.format.as_ref().map(|f| f.sample_rate).unwrap_or(0);
    let clock_drift = analyze::estimate_clock_drift(&stats.position_series, nominal_sample_rate);

    let qpc_series: Vec<u64> = stats.position_series.iter().map(|(q, _)| *q).collect();
    let monotonic_violations = analyze::detect_monotonic_violations(&qpc_series);

    // 真のexpected interval(GetDevicePeriodの問い合わせ)は未実装のため、
    // 観測された平均wake間隔を「期待値」としてjitterを計算する(2パス)。
    let rough = analyze::compute_wake_timing(&stats.wake_qpc_100ns_series, 0.0);
    let wake_timing =
        analyze::compute_wake_timing(&stats.wake_qpc_100ns_series, rough.interval.mean_ms);

    let packet_age = group
        .map(|g| analyze::compute_packet_age_at_wake(&g.records))
        .unwrap_or_default();

    serde_json::json!({
        "wake_events": stats.wake_events,
        "packet_events": stats.packet_events,
        "total_frames_captured": stats.total_frames_captured,
        "discontinuity_count": stats.discontinuity_count,
        "silent_count": stats.silent_count,
        "timestamp_error_count": stats.timestamp_error_count,
        "expected_wake_interval_ms": wake_timing.expected_interval_ms,
        "wake_interval_ms": {
            "mean": wake_timing.interval.mean_ms,
            "p95": wake_timing.interval.p95_ms,
            "p99": wake_timing.interval.p99_ms,
            "max": wake_timing.interval.max_ms,
        },
        "wake_jitter_ms": {
            "mean": wake_timing.jitter.mean_ms,
            "p95": wake_timing.jitter.p95_ms,
            "p99": wake_timing.jitter.p99_ms,
            "max": wake_timing.jitter.max_ms,
        },
        "packet_age_at_wake_ms": {
            "mean": packet_age.mean_ms,
            "p95": packet_age.p95_ms,
            "p99": packet_age.p99_ms,
            "max": packet_age.max_ms,
        },
        "position_gap_frames_total": position_gaps.gap_frames_total,
        "position_overlap_frames_total": position_gaps.overlap_frames_total,
        "position_gap_events": position_gaps.gap_events,
        "monotonic_violations": monotonic_violations,
        "clock_drift_ppm_vs_qpc": clock_drift.drift_ppm,
        "mmcss_applied": stats.mmcss_applied.unwrap_or(false),
        // SPIKE-09の観測対象。epoch_countが2以上なら、実行中に少なくとも
        // 1回はデバイス消失→再アタッチが起きたことを意味する。
        "epoch_count": groups.len(),
        "session_disconnected_count": stats.session_disconnected_count,
        "last_session_disconnect_reason_raw": stats.last_session_disconnect_reason_raw,
    })
}

#[derive(Clone, Copy)]
enum StreamKind {
    Mic,
    Loopback,
}

#[allow(clippy::too_many_arguments)]
fn spawn_stream(
    kind: StreamKind,
    device_id_or_default: &str,
    role: DeviceRole,
    callback_timeout_ms: u32,
    capture_epoch: u64,
    drop_counter: Arc<AtomicU64>,
    tx: crossbeam_channel::Sender<CaptureEvent>,
    stop: Arc<StopSignal>,
) -> std::thread::JoinHandle<CaptureThreadOutcome> {
    let stream: Box<dyn CaptureStream> = match kind {
        StreamKind::Mic => Box::new(MicCaptureStream {
            device_id_or_default: device_id_or_default.to_string(),
            role,
            pipeline_drop_counter: drop_counter,
            callback_timeout_ms,
            capture_epoch,
        }),
        StreamKind::Loopback => Box::new(EndpointLoopbackStream {
            device_id_or_default: device_id_or_default.to_string(),
            role,
            pipeline_drop_counter: drop_counter,
            callback_timeout_ms,
            capture_epoch,
        }),
    };
    spawn_capture_thread(stream, tx, stop)
}

/// マイク/Endpoint Loopbackそれぞれの「現在の1本のキャプチャスレッド」を監督する。
/// spike-windows-01-02-detail-design.md §3.8のP1方針(JoinHandleの戻り値を
/// 制御フローの正とする)をSPIKE-01側でも踏襲し、共有チャネル経由の
/// CaptureEvent::StreamStoppedはAggregatorの統計記録用の副次経路に留める。
struct StreamSupervisor {
    kind: StreamKind,
    label: &'static str,
    device_id_or_default: String,
    role: DeviceRole,
    callback_timeout_ms: u32,
    drop_counter: Arc<AtomicU64>,
    capture_epoch: u64,
    stop_signal: Arc<StopSignal>,
    thread: Option<std::thread::JoinHandle<CaptureThreadOutcome>>,
    recovery_attempts: u32,
}

impl StreamSupervisor {
    fn start(
        kind: StreamKind,
        label: &'static str,
        device_id_or_default: String,
        role: DeviceRole,
        callback_timeout_ms: u32,
        drop_counter: Arc<AtomicU64>,
        tx: crossbeam_channel::Sender<CaptureEvent>,
    ) -> anyhow::Result<Self> {
        let stop_signal = Arc::new(StopSignal::new()?);
        let thread = Some(spawn_stream(
            kind,
            &device_id_or_default,
            role,
            callback_timeout_ms,
            0,
            drop_counter.clone(),
            tx,
            stop_signal.clone(),
        ));
        Ok(Self {
            kind,
            label,
            device_id_or_default,
            role,
            callback_timeout_ms,
            drop_counter,
            capture_epoch: 0,
            stop_signal,
            thread,
            recovery_attempts: 0,
        })
    }

    /// キャプチャスレッドが(stopを要求していないのに)自然終了していれば
    /// outcomeを取り出す。
    fn poll_finished(&mut self) -> Option<CaptureThreadOutcome> {
        if self.thread.as_ref()?.is_finished() {
            self.thread
                .take()
                .map(|h| h.join().expect("capture thread panicked"))
        } else {
            None
        }
    }

    fn respawn(&mut self, tx: crossbeam_channel::Sender<CaptureEvent>) -> anyhow::Result<()> {
        self.capture_epoch += 1;
        self.stop_signal = Arc::new(StopSignal::new()?);
        self.thread = Some(spawn_stream(
            self.kind,
            &self.device_id_or_default,
            self.role,
            self.callback_timeout_ms,
            self.capture_epoch,
            self.drop_counter.clone(),
            tx,
            self.stop_signal.clone(),
        ));
        Ok(())
    }

    fn stop_and_join(&mut self) -> anyhow::Result<()> {
        self.stop_signal.signal()?;
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
        Ok(())
    }
}

/// SPIKE-09: `poll_finished`が返したoutcomeを見て、デバイス消失なら
/// device_events.jsonlへ記録し、上限内であれば再解決・再初期化する。
fn handle_supervisor_tick(
    sup: &mut StreamSupervisor,
    tx: &crossbeam_channel::Sender<CaptureEvent>,
    device_events_log: &mut JsonlWriter,
    max_recovery_attempts: u32,
) -> anyhow::Result<()> {
    let Some(outcome) = sup.poll_finished() else {
        return Ok(());
    };

    let (reason, retryable) = match &outcome {
        CaptureThreadOutcome::Stopped {
            exit: CaptureExit::DeviceLost,
            ..
        } => ("device_lost", true),
        CaptureThreadOutcome::Stopped {
            exit: CaptureExit::StoppedByRequest,
            ..
        } => ("unexpected_stop", false),
        CaptureThreadOutcome::Errored { .. } => ("errored", true),
    };
    let error_message = match &outcome {
        CaptureThreadOutcome::Errored { error, .. } => Some(error.to_string()),
        _ => None,
    };

    tracing::warn!(
        stream = sup.label,
        reason,
        capture_epoch = sup.capture_epoch,
        error = ?error_message,
        "capture thread finished unexpectedly"
    );
    device_events_log.write(serde_json::json!({
        "ts_ns": JsonlWriter::now_ns(),
        "type": "capture_thread_finished",
        "stream": sup.label,
        "reason": reason,
        "capture_epoch": sup.capture_epoch,
        "error": error_message,
    }));

    if !retryable {
        return Ok(());
    }

    if sup.recovery_attempts >= max_recovery_attempts {
        tracing::error!(
            stream = sup.label,
            attempts = sup.recovery_attempts,
            "recovery attempts exhausted; giving up on this stream for the rest of the run"
        );
        device_events_log.write(serde_json::json!({
            "ts_ns": JsonlWriter::now_ns(),
            "type": "capture_recovery_abandoned",
            "stream": sup.label,
            "attempts": sup.recovery_attempts,
        }));
        return Ok(());
    }

    sup.recovery_attempts += 1;
    tracing::warn!(
        stream = sup.label,
        attempt = sup.recovery_attempts,
        "re-resolving and restarting capture"
    );
    sup.respawn(tx.clone())?;
    device_events_log.write(serde_json::json!({
        "ts_ns": JsonlWriter::now_ns(),
        "type": "capture_restarted",
        "stream": sup.label,
        "new_capture_epoch": sup.capture_epoch,
        "attempt": sup.recovery_attempts,
    }));
    Ok(())
}

fn device_watch_event_to_json(event: &DeviceWatchEvent) -> serde_json::Value {
    let ts_ns = JsonlWriter::now_ns();
    match event {
        DeviceWatchEvent::DeviceAdded {
            endpoint_id,
            observed_at_100ns,
        } => serde_json::json!({
            "ts_ns": ts_ns,
            "type": "device_added",
            "endpoint_id": endpoint_id,
            "observed_at_100ns": observed_at_100ns,
        }),
        DeviceWatchEvent::DeviceRemoved {
            endpoint_id,
            observed_at_100ns,
        } => serde_json::json!({
            "ts_ns": ts_ns,
            "type": "device_removed",
            "endpoint_id": endpoint_id,
            "observed_at_100ns": observed_at_100ns,
        }),
        DeviceWatchEvent::DeviceStateChanged {
            endpoint_id,
            new_state_raw,
            observed_at_100ns,
        } => serde_json::json!({
            "ts_ns": ts_ns,
            "type": "device_state_changed",
            "endpoint_id": endpoint_id,
            "new_state_raw": new_state_raw,
            "observed_at_100ns": observed_at_100ns,
        }),
        DeviceWatchEvent::PropertyValueChanged {
            endpoint_id,
            property_key_fmtid,
            property_key_pid,
            observed_at_100ns,
        } => serde_json::json!({
            "ts_ns": ts_ns,
            "type": "property_value_changed",
            "endpoint_id": endpoint_id,
            "property_key_fmtid": format!("{property_key_fmtid:?}"),
            "property_key_pid": property_key_pid,
            "observed_at_100ns": observed_at_100ns,
        }),
        DeviceWatchEvent::DefaultDeviceChanged {
            flow_raw,
            role_raw,
            endpoint_id,
            observed_at_100ns,
        } => serde_json::json!({
            "ts_ns": ts_ns,
            "type": "default_device_changed",
            "flow_raw": flow_raw,
            "role_raw": role_raw,
            "endpoint_id": endpoint_id,
            "observed_at_100ns": observed_at_100ns,
        }),
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    if cli.list_devices {
        let to_json = |d: device_select::DeviceInfo| {
            serde_json::json!({
                "id": d.id,
                "friendly_name": d.friendly_name,
                "is_default_for_role": d.is_default_for_role.map(|r| format!("{r:?}")),
            })
        };
        let devices = serde_json::json!({
            "capture": device_select::enumerate_capture_devices()?
                .into_iter()
                .map(to_json)
                .collect::<Vec<_>>(),
            "render": device_select::enumerate_render_devices()?
                .into_iter()
                .map(to_json)
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&devices)?);
        return Ok(());
    }

    let out_dir = cli
        .output_dir
        .unwrap_or_else(|| PathBuf::from("out").join("run"));
    std::fs::create_dir_all(&out_dir)?;
    let device_role = cli.device_role;

    let mut device_events_log = JsonlWriter::create(&out_dir.join("device_events.jsonl"))?;

    // spike-plan.md SPIKE-09: IMMNotificationClientによるデバイス変更観測。
    // 登録に失敗しても録音自体は継続する(ベストエフォート)。このスレッド
    // (main)がDeviceWatchより先に終了しないよう、ガードはmain()の最後まで
    // 保持する。
    let (device_tx, device_rx) = crossbeam_channel::unbounded();
    let _device_watch = match DeviceWatch::start(device_tx) {
        Ok(watch) => Some(watch),
        Err(e) => {
            tracing::warn!(error = %e, "DeviceWatch::start failed; device change events won't be observed");
            None
        }
    };

    let (tx, rx) = crossbeam_channel::bounded(256);
    let mic_drop_counter = Arc::new(AtomicU64::new(0));
    let loopback_drop_counter = Arc::new(AtomicU64::new(0));

    let mut mic_sup = StreamSupervisor::start(
        StreamKind::Mic,
        "mic",
        cli.mic_device.clone(),
        cli.device_role,
        cli.callback_timeout_ms,
        mic_drop_counter.clone(),
        tx.clone(),
    )?;
    let mut loopback_sup = StreamSupervisor::start(
        StreamKind::Loopback,
        "loopback",
        cli.render_device.clone(),
        cli.device_role,
        cli.callback_timeout_ms,
        loopback_drop_counter.clone(),
        tx.clone(),
    )?;

    let aggregator = Aggregator::new(
        &out_dir,
        &[
            (StreamId::Mic, "mic"),
            (StreamId::EndpointLoopback, "loopback"),
        ],
    )?;
    let aggregator_handle = std::thread::spawn(move || aggregator.run(rx));

    let wall_start = Instant::now();
    let cpu_start = analyze::ProcessTimes::query_current().ok();

    // 固定時間sleepではなく1秒周期のtickにする(SPIKE-09: デバイス消失検出→
    // 再初期化をduration_secs内で行うため、spike-02のプロセス監視ループと
    // 同じ構成にする)。
    let run_start = Instant::now();
    while run_start.elapsed() < Duration::from_secs(cli.duration_secs) {
        while let Ok(event) = device_rx.try_recv() {
            device_events_log.write(device_watch_event_to_json(&event));
        }

        handle_supervisor_tick(
            &mut mic_sup,
            &tx,
            &mut device_events_log,
            cli.max_recovery_attempts,
        )?;
        handle_supervisor_tick(
            &mut loopback_sup,
            &tx,
            &mut device_events_log,
            cli.max_recovery_attempts,
        )?;

        std::thread::sleep(Duration::from_secs(1));
    }

    mic_sup.stop_and_join()?;
    loopback_sup.stop_and_join()?;
    drop(tx);

    let mut results = aggregator_handle
        .join()
        .expect("aggregator thread panicked")?;

    let wall_secs = wall_start.elapsed().as_secs_f64();
    let cpu_end = analyze::ProcessTimes::query_current().ok();
    let cpu_percent = match (cpu_start, cpu_end) {
        (Some(start), Some(end)) => analyze::measure_cpu_percent(start, end, wall_secs),
        _ => 0.0,
    };
    let peak_working_set_bytes = analyze::measure_peak_working_set_bytes();

    let os_info = os_check::query_os_version().ok();
    let qpc_freq_hz = spike_common::timestamp::QpcClock::query()?.freq_hz();

    let empty_result = || StreamCaptureResult {
        format: None,
        device_id: None,
        device_friendly_name: None,
        stats: StreamStats::default(),
        records: Vec::new(),
    };
    let mic_result = results.remove(&StreamId::Mic).unwrap_or_else(empty_result);
    let loopback_result = results
        .remove(&StreamId::EndpointLoopback)
        .unwrap_or_else(empty_result);

    let mic_summary = build_stream_summary(&mic_result);
    let loopback_summary = build_stream_summary(&loopback_result);

    let relative_drift_ppm = match (
        mic_result.format.as_ref(),
        loopback_result.format.as_ref(),
    ) {
        (Some(_), Some(_)) => {
            let mic_drift = analyze::estimate_clock_drift(
                &mic_result.stats.position_series,
                mic_result.format.as_ref().unwrap().sample_rate,
            );
            let loopback_drift = analyze::estimate_clock_drift(
                &loopback_result.stats.position_series,
                loopback_result.format.as_ref().unwrap().sample_rate,
            );
            Some(analyze::relative_drift_ppm(&mic_drift, &loopback_drift))
        }
        _ => None,
    };

    let pipeline_drop_count = mic_drop_counter.load(Ordering::Relaxed)
        + loopback_drop_counter.load(Ordering::Relaxed);
    let qpc_monotonic = mic_summary["monotonic_violations"].as_u64().unwrap_or(0) == 0
        && loopback_summary["monotonic_violations"].as_u64().unwrap_or(0) == 0;
    let position_gap_count = mic_summary["position_gap_events"].as_u64().unwrap_or(0)
        + loopback_summary["position_gap_events"].as_u64().unwrap_or(0);
    let discontinuity_count =
        mic_result.stats.discontinuity_count + loopback_result.stats.discontinuity_count;
    let drift_within_target = relative_drift_ppm
        .map(|ppm| ppm.abs() < DRIFT_PPM_TARGET_ABS)
        .unwrap_or(false);
    let wake_jitter_within_target = mic_summary["wake_jitter_ms"]["p99"].as_f64().unwrap_or(f64::MAX)
        < WAKE_JITTER_P99_MS_TARGET
        && loopback_summary["wake_jitter_ms"]["p99"].as_f64().unwrap_or(f64::MAX)
            < WAKE_JITTER_P99_MS_TARGET;
    let cpu_under_10_percent = cpu_percent < 10.0;
    let mmcss_applied = mic_result.stats.mmcss_applied.unwrap_or(false)
        && loopback_result.stats.mmcss_applied.unwrap_or(false);
    let overall_suggestion = if qpc_monotonic
        && position_gap_count == 0
        && pipeline_drop_count == 0
        && cpu_under_10_percent
    {
        "GO"
    } else {
        "CONDITIONAL-GO"
    };

    let summary = serde_json::json!({
        "run_id": out_dir.file_name().and_then(|s| s.to_str()).unwrap_or("run"),
        "duration_secs": cli.duration_secs,
        "os": os_info,
        "qpc_freq_hz": qpc_freq_hz,
        "devices": {
            "mic": {
                "id": mic_result.device_id,
                "friendly_name": mic_result.device_friendly_name,
                "role": role_str(device_role),
            },
            "loopback_render": {
                "id": loopback_result.device_id,
                "friendly_name": loopback_result.device_friendly_name,
                "role": role_str(device_role),
            },
        },
        "streams": {
            "mic": mic_summary,
            "loopback": loopback_summary,
        },
        "relative_drift_ppm_mic_vs_loopback": relative_drift_ppm,
        "process_cpu_percent_estimate": cpu_percent,
        "process_peak_working_set_bytes": peak_working_set_bytes,
        // SPIKE-09: 実行中に何回デバイス消失からの再アタッチを行ったか。
        "recovery_attempts": {
            "mic": mic_sup.recovery_attempts,
            "loopback": loopback_sup.recovery_attempts,
        },
        "acceptance": {
            "qpc_monotonic": qpc_monotonic,
            // 検出の仕組みそのものは常に有効(discontinuityフラグ・gap検出は
            // データの有無によらず動作する)。実測値は各カウントを参照する。
            "discontinuity_detection_operational": true,
            "discontinuity_count": discontinuity_count,
            "position_gap_count": position_gap_count,
            "pipeline_drop_count": pipeline_drop_count,
            "drift_within_target": drift_within_target,
            "drift_target_ppm_abs": DRIFT_PPM_TARGET_ABS,
            "wake_jitter_within_target": wake_jitter_within_target,
            "wake_jitter_p99_target_ms": WAKE_JITTER_P99_MS_TARGET,
            "cpu_under_10_percent": cpu_under_10_percent,
            "mmcss_applied": mmcss_applied,
            // SPIKE-01自体にはOSビルド要件がない(Process LoopbackのみSPIKE-02で要件あり)。
            "os_build_supported": true,
            "overall_suggestion": overall_suggestion,
        },
    });

    let summary_path = out_dir.join("summary.json");
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)?;
    tracing::info!(path = %summary_path.display(), "summary.json written");

    Ok(())
}
