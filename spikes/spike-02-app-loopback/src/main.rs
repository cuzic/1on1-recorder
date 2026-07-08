// spike-windows-01-02-detail-design.md §5.7/§5.8

mod completion_handler;
mod process_finder;
mod process_loopback;

use clap::Parser;
use process_finder::{ProcessSelectionStrategy, ProcessWatchEvent, ProcessWatcher};
use process_loopback::{ProcessLoopbackMode, ProcessLoopbackStream};
use spike_common::spawn_capture_thread;
use spike_common::StopSignal;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

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

    #[arg(long)]
    output_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let os_info = spike_common::os_check::query_os_version()?;
    spike_common::os_check::check_process_loopback_support(&os_info)?;

    let out_dir = cli
        .output_dir
        .unwrap_or_else(|| PathBuf::from("out").join("run"));
    std::fs::create_dir_all(&out_dir)?;

    let activation_hard_timeout = cli.activation_hard_timeout_ms.map(Duration::from_millis);

    let (initial_pid, mut watcher) = if let Some(pid) = cli.target_pid {
        (pid, ProcessWatcher::new_by_pid(pid))
    } else {
        let name = cli
            .target_process
            .clone()
            .expect("clapのconflicts_withによりtarget_processかtarget_pidのどちらかは必須");
        let m = process_finder::find_process_by_name(&name, cli.process_selection)
            .ok_or_else(|| anyhow::anyhow!("対象プロセスが見つかりません: {name}"))?;
        let pid = m.pid;
        (pid, ProcessWatcher::new_by_name(name, cli.process_selection, pid))
    };

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
    });
    let mut current_thread = Some(spawn_capture_thread(stream, tx.clone(), stop_signal.clone()));

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(cli.duration_secs) {
        match watcher.poll() {
            ProcessWatchEvent::StillAlive(_) => {}
            ProcessWatchEvent::Exited { old_pid } => {
                tracing::info!(old_pid, capture_epoch, "process_exited");
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
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    stop_signal.signal()?;
    if let Some(h) = current_thread.take() {
        let _ = h.join();
    }
    drop(tx);

    // TODO(§5.9): Aggregator(spike-01と共有可能な形へ抽出予定)でCSV/WAV/
    // process_events.jsonl/summary.jsonを書き出す。
    let _ = rx;

    Ok(())
}
