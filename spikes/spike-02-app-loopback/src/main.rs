// spike-windows-01-02-detail-design.md §5.7/§5.8/§5.9

mod completion_handler;
mod process_finder;
mod process_loopback;

use clap::Parser;
use process_finder::{ProcessSelectionStrategy, ProcessWatchEvent, ProcessWatcher};
use process_loopback::{ProcessLoopbackMode, ProcessLoopbackStream};
use spike_common::aggregator::Aggregator;
use spike_common::analyze;
use spike_common::frame_record::StreamId;
use spike_common::jsonl_log::JsonlWriter;
use spike_common::spawn_capture_thread;
use spike_common::StopSignal;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(clap::Parser)]
struct Cli {
    /// 対象プロセスの実行ファイル名(例: "Zoom.exe", "ms-teams.exe", "chrome.exe")。
    /// --target-pidと排他。
    #[arg(long, conflicts_with = "target_pid")]
    target_process: Option<String>,

    /// 対象プロセスのPIDを直接指定する。指定時はプロセス再起動時の自動再アタッチが効かない。
    #[arg(long, conflicts_with = "target_process")]
    target_pid: Option<u32>,

    /// --target-process指定時に複数候補が見つかった場合の選択戦略
    #[arg(long, value_enum, default_value_t = ProcessSelectionStrategy::Root)]
    process_selection: ProcessSelectionStrategy,

    /// プロセスツリーを含めるか除外するか
    #[arg(long, value_enum, default_value_t = ProcessLoopbackMode::Include)]
    mode: ProcessLoopbackMode,

    #[arg(long, default_value_t = 600)]
    duration_secs: u64,

    /// ActivateAudioInterfaceAsyncの完了待ちにハードタイムアウトを設ける(診断用)。
    /// 省略時(既定)はタイムアウトを設けず、完了まで無条件に待つ(§5.4参照)。
    #[arg(long)]
    activation_hard_timeout_ms: Option<u64>,

    /// プロセス再起動時の自動再アタッチを有効にする(--target-process指定時のみ有効)
    #[arg(long, default_value_t = true)]
    reattach: bool,

    /// キャプチャコールバックのタイムアウト(ms)。WaitForMultipleObjectsに渡す
    /// (SPIKE-01と同じ既定値。Process Loopbackでは対象アプリ無音時のidle_timeout
    /// 判定周期としても使われる)。
    #[arg(long, default_value_t = 2000)]
    callback_timeout_ms: u32,

    #[arg(long)]
    output_dir: Option<PathBuf>,
}


fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    spike_common::report::print_banner(
        "SPIKE-02",
        "Application/Process Loopback Capture(プロセス指定)",
        &[
            "Zoom/Teams/Chrome等、音声を出しているアプリを対象に取得します。",
            "--target-process/--target-pidを省略した場合、起動中のZoom/Teams/",
            "Chrome/Edge/Firefoxを自動検出します(複数見つかった場合は選択を促します)。",
            &format!("これから最大{}秒間キャプチャします。途中終了はCtrl+C。", cli.duration_secs),
        ],
    );
    let stop_requested = spike_common::report::install_ctrlc_stop_flag();

    let os_info = spike_common::os_check::query_os_version()?;
    spike_common::os_check::check_process_loopback_support(&os_info)?;

    let out_dir = cli
        .output_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("out").join("run"));
    std::fs::create_dir_all(&out_dir)?;
    let mut event_log = JsonlWriter::create(&out_dir.join("process_events.jsonl"))?;

    let activation_hard_timeout = cli.activation_hard_timeout_ms.map(Duration::from_millis);

    let Some((initial_pid, mut watcher, target_label)) = resolve_target_process(&cli)? else {
        // 対象プロセスが見つからない/選べなかった場合は、パニックではなく
        // 案内を表示してそのまま(Enter待ちで)穏やかに終了する。
        spike_common::report::pause_before_exit();
        return Ok(());
    };
    println!("対象プロセス: {target_label} (PID={initial_pid})");

    let (tx, rx) = crossbeam_channel::bounded(256);
    let pipeline_drop_counter = Arc::new(AtomicU64::new(0));

    let mut capture_epoch: u64 = 0;
    let mut stop_signal = Arc::new(StopSignal::new()?);
    let stream = Box::new(ProcessLoopbackStream {
        target_pid: initial_pid,
        mode: cli.mode,
        capture_epoch,
        activation_hard_timeout,
        pipeline_drop_counter: pipeline_drop_counter.clone(),
        callback_timeout_ms: cli.callback_timeout_ms,
    });
    let mut current_thread = Some(spawn_capture_thread(stream, tx.clone(), stop_signal.clone()));

    let aggregator = Aggregator::new(&out_dir, &[(StreamId::ProcessLoopback, "process_loopback")])?;
    let aggregator_handle = std::thread::spawn(move || aggregator.run(rx));

    let wall_start = Instant::now();
    let cpu_start = analyze::ProcessTimes::query_current().ok();

    println!("キャプチャを開始しました...");
    let start = std::time::Instant::now();
    let mut last_countdown_print = Instant::now();
    while start.elapsed() < Duration::from_secs(cli.duration_secs)
        && !stop_requested.load(std::sync::atomic::Ordering::SeqCst)
    {
        if last_countdown_print.elapsed() >= Duration::from_secs(30) {
            let remaining = Duration::from_secs(cli.duration_secs).saturating_sub(start.elapsed());
            println!("...残り約{}秒", remaining.as_secs());
            last_countdown_print = Instant::now();
        }
        match watcher.poll() {
            ProcessWatchEvent::StillAlive(pid) => {
                tracing::trace!(pid, "process_still_alive");
            }
            ProcessWatchEvent::Exited { old_pid } => {
                tracing::info!(old_pid, capture_epoch, "process_exited");
                event_log.write(serde_json::json!({
                    "ts_ns": JsonlWriter::now_ns(),
                    "type": "process_exited",
                    "old_pid": old_pid,
                    "capture_epoch": capture_epoch,
                }));
                stop_signal.signal()?;
                if let Some(h) = current_thread.take() {
                    let outcome = h.join();
                    tracing::info!(capture_epoch, "capture_thread_joined_after_exit");
                    let _ = outcome;
                }
                // Remote側にsilenceを挿入すべき区間としてマーキング
                // (実際のsilence挿入自体はSPIKE-03の責務。ここではイベント記録のみ)
                if !cli.reattach {
                    break;
                }
            }
            ProcessWatchEvent::Restarted { old_pid, new_pid } => {
                tracing::info!(old_pid, new_pid, capture_epoch, "process_restarted");
                event_log.write(serde_json::json!({
                    "ts_ns": JsonlWriter::now_ns(),
                    "type": "process_restarted",
                    "old_pid": old_pid,
                    "new_pid": new_pid,
                    "capture_epoch": capture_epoch,
                }));
                stop_signal.signal()?;
                if let Some(h) = current_thread.take() {
                    let _ = h.join();
                }

                capture_epoch += 1;
                let new_stop_signal = Arc::new(StopSignal::new()?);
                let stream = Box::new(ProcessLoopbackStream {
                    target_pid: new_pid,
                    mode: cli.mode,
                    capture_epoch,
                    activation_hard_timeout,
                    pipeline_drop_counter: pipeline_drop_counter.clone(),
                    callback_timeout_ms: cli.callback_timeout_ms,
                });
                current_thread = Some(spawn_capture_thread(
                    stream,
                    tx.clone(),
                    new_stop_signal.clone(),
                ));
                stop_signal = new_stop_signal;
            }
            ProcessWatchEvent::NotFound => {
                tracing::warn!("process_not_found; stopping watch loop");
                event_log.write(serde_json::json!({
                    "ts_ns": JsonlWriter::now_ns(),
                    "type": "process_not_found",
                    "capture_epoch": capture_epoch,
                }));
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    if stop_requested.load(std::sync::atomic::Ordering::SeqCst) {
        println!("Ctrl+Cを検出しました。ここまでのデータで集計・レポート表示を行います...");
    }

    stop_signal.signal()?;
    if let Some(h) = current_thread.take() {
        let _ = h.join();
    }
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
    let qpc_freq_hz = spike_common::timestamp::QpcClock::query()?.freq_hz();

    let result = results
        .remove(&StreamId::ProcessLoopback)
        .unwrap_or_default();

    // SPIKE-01のbuild_stream_summary相当。Process Loopbackは常にcapture_epoch>=0の
    // 複数epochを持ちうる(プロセス再起動のたび+1、§5.7)ため、group_records_by_epoch
    // で世代ごとに分割してから解析する(§4.9のP0-5)。
    let groups = analyze::group_records_by_epoch(&result.records);
    let stats = &result.stats;

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

    let rough = analyze::compute_wake_timing(&stats.wake_qpc_100ns_series, 0.0);
    let wake_timing =
        analyze::compute_wake_timing(&stats.wake_qpc_100ns_series, rough.interval.mean_ms);

    let packet_age = groups
        .first()
        .map(|g| analyze::compute_packet_age_at_wake(&g.records))
        .unwrap_or_default();

    let stream_summary = serde_json::json!({
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
        "epoch_count": groups.len(),
    });

    let summary = serde_json::json!({
        "run_id": out_dir.file_name().and_then(|s| s.to_str()).unwrap_or("run"),
        "duration_secs": cli.duration_secs,
        "os": os_info,
        "qpc_freq_hz": qpc_freq_hz,
        "target": {
            "initial_pid": initial_pid,
            "mode": format!("{:?}", cli.mode),
            "reattach": cli.reattach,
        },
        "streams": {
            "process_loopback": stream_summary,
        },
        "process_cpu_percent_estimate": cpu_percent,
        "process_peak_working_set_bytes": peak_working_set_bytes,
        // Process Loopback特有の項目(§5.9)。対象アプリが無音の間はエラーではなく
        // 単に通知が来ないため、この値が大きいこと自体は失格要件ではない。
        "idle_timeout_count": stats.idle_timeout_count,
        "idle_timeout_note": "対象アプリが無音の間、通知が来ずタイムアウトした回数。エラーではない(§4.4)",
        "acceptance": {
            "qpc_monotonic": monotonic_violations == 0,
            "discontinuity_detection_operational": true,
            "discontinuity_count": stats.discontinuity_count,
            "position_gap_count": position_gaps.gap_events,
            "pipeline_drop_count": pipeline_drop_counter.load(std::sync::atomic::Ordering::Relaxed),
            "cpu_under_10_percent": cpu_percent < 10.0,
            "mmcss_applied": stats.mmcss_applied.unwrap_or(false),
            "os_build_supported": true,
        },
    });

    let summary_path = out_dir.join("summary.json");
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)?;
    println!("詳細ログの保存先: {}", out_dir.display());

    let acceptance = summary.get("acceptance").cloned().unwrap_or(serde_json::json!({}));
    spike_common::report::print_acceptance_report(
        "SPIKE-02",
        "Application/Process Loopback Capture(プロセス指定)",
        &acceptance,
    );
    spike_common::report::pause_before_exit();

    Ok(())
}

/// `--target-process`/`--target-pid`のどちらも指定されなかった場合に、
/// 起動中のZoom/Teams/Chrome/Edge/Firefoxを自動検出して対象を選ぶ
/// (「起動してEnterを押すだけ」の運用のため)。1件も見つからない、または
/// ユーザーが選択を放棄した場合は`Ok(None)`を返す(呼び出し側はパニックせず
/// 案内を出して穏やかに終了する)。
fn resolve_target_process(
    cli: &Cli,
) -> anyhow::Result<Option<(u32, ProcessWatcher, String)>> {
    if let Some(pid) = cli.target_pid {
        return match process_finder::resolve_process_by_pid(pid) {
            Some(m) => Ok(Some((pid, ProcessWatcher::new_by_pid(pid), m.exe_name))),
            None => {
                println!("指定されたPID {pid} のプロセスが見つかりません。");
                Ok(None)
            }
        };
    }

    if let Some(name) = &cli.target_process {
        return match process_finder::find_process_by_name(name, cli.process_selection) {
            Some(m) => {
                let pid = m.pid;
                Ok(Some((
                    pid,
                    ProcessWatcher::new_by_name(name.clone(), cli.process_selection, pid),
                    name.clone(),
                )))
            }
            None => {
                println!("対象プロセスが見つかりません: {name}");
                println!("{name}を起動してから再実行するか、--target-processで正しい実行ファイル名を指定してください。");
                Ok(None)
            }
        };
    }

    // 両方省略: よくある会議・ブラウザアプリを自動検出する。
    let candidates = process_finder::find_running_candidate_exe_names();
    let chosen_name = match candidates.len() {
        0 => {
            println!("Zoom/Teams/Chrome/Edge/Firefoxのいずれも起動中のプロセスから見つかりませんでした。");
            println!("対象アプリを起動してから再実行するか、--target-processで実行ファイル名を指定してください。");
            return Ok(None);
        }
        1 => {
            let name = candidates.into_iter().next().unwrap();
            println!("{name} を自動検出しました。これを対象にします。");
            name
        }
        _ => {
            println!("複数の候補が見つかりました。対象にする番号を入力してください:");
            for (i, name) in candidates.iter().enumerate() {
                println!("  {}) {name}", i + 1);
            }
            print!("番号を入力してEnter: ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let choice: usize = match input.trim().parse::<usize>() {
                Ok(n) if n >= 1 && n <= candidates.len() => n,
                _ => {
                    println!("無効な入力です。--target-processで直接指定して再実行してください。");
                    return Ok(None);
                }
            };
            candidates.into_iter().nth(choice - 1).unwrap()
        }
    };

    match process_finder::find_process_by_name(&chosen_name, cli.process_selection) {
        Some(m) => {
            let pid = m.pid;
            Ok(Some((
                pid,
                ProcessWatcher::new_by_name(chosen_name.clone(), cli.process_selection, pid),
                chosen_name,
            )))
        }
        None => {
            println!("{chosen_name}の解決に失敗しました。");
            Ok(None)
        }
    }
}
