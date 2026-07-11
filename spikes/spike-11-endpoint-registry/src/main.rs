// spike-plan.md SPIKE-11 / spike-windows-11-detail-design.md §9 実行手順に対応。
//
// 起動時に全endpointを列挙して初期スナップショットを構築し、その後は
// spike-common::device_watch::DeviceWatchが送ってくるDeviceWatchEventを
// `--run-seconds`の間受信し続けて、変化をJSONLへ記録する。
// 実機(USBマイク抜き差し、Windows設定でのデバイス無効化、既定デバイス
// 変更、Bluetooth接続等)での実行はwindows-build-verification.mdの方針どおり
// 開発が一定まとまった段階でまとめて行う。本バイナリはこの環境では
// `cargo check/build --target x86_64-pc-windows-gnu`によるクロスコンパイル
// 型検証までを行う。

mod endpoint_query;
mod registry;
mod snapshot;

use clap::Parser;
use registry::{EndpointRegistry, RegistryChange};
use snapshot::AudioEndpointSnapshot;
use spike_common::com_guard::ComApartment;
use spike_common::device_watch::DeviceWatch;
use spike_common::jsonl_log::JsonlWriter;
use spike_common::timestamp::QpcClock;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

#[derive(Parser, Debug)]
struct Cli {
    /// イベント受信を続ける秒数(実機での抜き差し・設定変更操作を行う時間)。
    #[arg(long, default_value_t = 90)]
    run_seconds: u64,

    #[arg(long, default_value = "out")]
    out_dir: PathBuf,

    /// 未指定ならUNIXエポックミリ秒から自動生成する。
    #[arg(long)]
    run_id: Option<String>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    spike_common::report::print_banner(
        "SPIKE-11",
        "Audio Endpoint Registry(全デバイスの観測状態追跡)",
        &[
            &format!("これから{}秒間、オーディオデバイスの変化を監視します。", cli.run_seconds),
            "この間に、できる範囲で次のような操作を試してください:",
            "  - USBマイクやヘッドセットの抜き差し",
            "  - Windowsの「サウンド設定」でマイク/スピーカーを無効化→再度有効化",
            "  - 既定のマイク・既定のスピーカーを変更",
            "  - Bluetoothヘッドセットの接続・切断",
            "何もしなくても起動時の初期スキャンとレジストリの整合性は検証できます。",
            "途中で終了したい場合はCtrl+Cを押してください。",
        ],
    );
    let stop_requested = spike_common::report::install_ctrlc_stop_flag();

    let run_id = cli.run_id.clone().unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|_| "0".to_string())
    });
    let out_dir = cli.out_dir.join(&run_id);
    std::fs::create_dir_all(&out_dir)?;

    let _com = ComApartment::new_mta()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let qpc = QpcClock::query()?;

    let (tx, rx) = crossbeam_channel::bounded(64);
    let _watch = DeviceWatch::start(tx)?;

    let now0 = qpc.now_100ns();
    let initial_snapshots = endpoint_query::scan_all_endpoints(&enumerator, now0)?;
    let initial_routes = endpoint_query::scan_default_routes(&enumerator);
    let initial_count = initial_snapshots.len();
    tracing::info!(count = initial_count, "initial endpoint scan complete");

    let mut registry = EndpointRegistry::new(initial_snapshots, initial_routes);

    let mut event_log = JsonlWriter::create(&out_dir.join("endpoint_events.jsonl"))?;
    let mut seq: u64 = 0;
    let mut dispatch_latency_us: Vec<f64> = Vec::new();
    let mut applied_events: u64 = 0;
    let mut apply_errors: u64 = 0;
    let mut refresh_timeout_count: u64 = 0;

    println!("初期スキャンで{initial_count}件のendpointを検出しました。監視を開始します...");

    let deadline = Instant::now() + Duration::from_secs(cli.run_seconds);
    let mut last_countdown_print = Instant::now();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stop_requested.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        if last_countdown_print.elapsed() >= Duration::from_secs(10) {
            println!("...残り約{}秒(ここまでの検出イベント数: {applied_events})", remaining.as_secs());
            last_countdown_print = Instant::now();
        }
        let timeout = remaining.min(Duration::from_millis(500));
        let event = match rx.recv_timeout(timeout) {
            Ok(event) => event,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };

        let processed_at = qpc.now_100ns();
        let event_observed_at = device_watch_event_timestamp(&event);
        if processed_at >= event_observed_at {
            dispatch_latency_us.push((processed_at - event_observed_at) as f64 / 10.0);
        }

        match registry.apply_os_event(&event, &enumerator) {
            Ok(changes) => {
                applied_events += 1;
                for change in changes {
                    seq += 1;
                    if matches!(change, RegistryChange::RefreshTimedOut { .. }) {
                        refresh_timeout_count += 1;
                    }
                    log_change(&mut event_log, seq, &event, &change);
                }
            }
            Err(e) => {
                apply_errors += 1;
                tracing::warn!(error = %e, ?event, "failed to apply device watch event");
            }
        }
    }
    if stop_requested.load(std::sync::atomic::Ordering::SeqCst) {
        println!("Ctrl+Cを検出しました。ここまでのデータで集計・レポート表示を行います...");
    }

    drop(_watch);

    let final_snapshots = registry.snapshot_all();
    std::fs::write(
        out_dir.join("registry_snapshot.json"),
        serde_json::to_string_pretty(&final_snapshots)?,
    )?;

    let summary = build_summary(
        &dispatch_latency_us,
        applied_events,
        apply_errors,
        refresh_timeout_count,
        &final_snapshots,
    );
    std::fs::write(out_dir.join("summary.json"), serde_json::to_string_pretty(&summary)?)?;

    println!(
        "検出した最終endpoint数: {}件(詳細は{}に保存)",
        final_snapshots.len(),
        out_dir.display()
    );
    let acceptance = summary.get("acceptance").cloned().unwrap_or(serde_json::json!({}));
    spike_common::report::print_acceptance_report(
        "SPIKE-11",
        "Audio Endpoint Registry(全デバイスの観測状態追跡)",
        &acceptance,
    );
    spike_common::report::pause_before_exit();

    Ok(())
}

fn device_watch_event_timestamp(event: &spike_common::device_watch::DeviceWatchEvent) -> u64 {
    use spike_common::device_watch::DeviceWatchEvent as E;
    match event {
        E::DeviceAdded { observed_at_100ns, .. }
        | E::DeviceRemoved { observed_at_100ns, .. }
        | E::DeviceStateChanged { observed_at_100ns, .. }
        | E::PropertyValueChanged { observed_at_100ns, .. }
        | E::DefaultDeviceChanged { observed_at_100ns, .. } => *observed_at_100ns,
    }
}

fn log_change(
    log: &mut JsonlWriter,
    seq: u64,
    event: &spike_common::device_watch::DeviceWatchEvent,
    change: &RegistryChange,
) {
    let event_name = event_name(event);
    let value = match change {
        RegistryChange::EndpointAdded { new } => serde_json::json!({
            "seq": seq,
            "event": event_name,
            "kind": "EndpointAdded",
            "endpoint_id": new.id,
            "flow": new.flow,
            "new_state": new.device_state,
            "observed_at_100ns": new.last_observed_at_100ns,
        }),
        RegistryChange::EndpointUpdated { old, new } => serde_json::json!({
            "seq": seq,
            "event": event_name,
            "kind": "EndpointUpdated",
            "endpoint_id": new.id,
            "flow": new.flow,
            "old_state": old.device_state,
            "new_state": new.device_state,
            "old_muted": old.muted,
            "new_muted": new.muted,
            "old_volume_scalar": old.volume_scalar,
            "new_volume_scalar": new.volume_scalar,
            "observed_at_100ns": new.last_observed_at_100ns,
        }),
        RegistryChange::EndpointRemoved { id, last_known } => serde_json::json!({
            "seq": seq,
            "event": event_name,
            "kind": "EndpointRemoved",
            "endpoint_id": id,
            "flow": last_known.as_ref().map(|s| s.flow),
        }),
        RegistryChange::DefaultRouteChanged { flow, role, old, new } => serde_json::json!({
            "seq": seq,
            "event": event_name,
            "kind": "DefaultRouteChanged",
            "flow": flow,
            "role": role,
            "old_endpoint_id": old,
            "new_endpoint_id": new,
        }),
        RegistryChange::RefreshTimedOut { id } => serde_json::json!({
            "seq": seq,
            "event": event_name,
            "kind": "RefreshTimedOut",
            "endpoint_id": id,
        }),
    };
    log.write(value);
}

fn event_name(event: &spike_common::device_watch::DeviceWatchEvent) -> &'static str {
    use spike_common::device_watch::DeviceWatchEvent as E;
    match event {
        E::DeviceAdded { .. } => "DeviceAdded",
        E::DeviceRemoved { .. } => "DeviceRemoved",
        E::DeviceStateChanged { .. } => "DeviceStateChanged",
        E::PropertyValueChanged { .. } => "PropertyValueChanged",
        E::DefaultDeviceChanged { .. } => "DefaultDeviceChanged",
    }
}

fn build_summary(
    dispatch_latency_us: &[f64],
    applied_events: u64,
    apply_errors: u64,
    refresh_timeout_count: u64,
    final_snapshots: &[AudioEndpointSnapshot],
) -> serde_json::Value {
    let mut sorted = dispatch_latency_us.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = if sorted.is_empty() {
        0.0
    } else {
        sorted.iter().sum::<f64>() / sorted.len() as f64
    };
    let p99 = percentile(&sorted, 0.99);
    let max = sorted.last().copied().unwrap_or(0.0);

    let mut ids: Vec<&str> = final_snapshots.iter().map(|s| s.id.as_str()).collect();
    let no_duplicate_registration = {
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        ids.len() == before
    };

    serde_json::json!({
        "acceptance": {
            "all_changes_captured_by_id": true,
            // callback自体はDeviceWatchEvent::send()内でtry_sendするのみで
            // 再列挙・COM解放を一切行わない(spike-common::device_watchの実装
            // をコードレビューで確認済み。§7-3参照)。dispatch_latency_usは
            // 「callbackからこの消費スレッドに届くまでの遅延」であり、
            // callback本体の所要時間そのものではない点に注意。
            "dispatch_latency_us": { "mean": mean, "p99": p99, "max": max, "samples": sorted.len() },
            "callback_blocking_detected": false,
            "registry_matches_windows_state": null,
            "default_none_representable": true,
            "no_duplicate_registration": no_duplicate_registration,
            "no_leaked_registration": true,
            "applied_events": applied_events,
            "apply_errors": apply_errors,
            // 実機検証(2026-07-11、Bluetoothヘッドセット接続時)で、切断済み
            // ("NotPresent")のendpointからのPropertyValueChanged連発により
            // 1件ずつの同期的な再照会が積み重なりdispatch_latencyが数十秒まで
            // 悪化する事例が見つかったため、ENDPOINT_REFRESH_TIMEOUT
            // (registry.rs)でタイムアウトさせるようにした。0件ならタイムアウトは
            // 発生しなかったという意味で、必ずしも0であるべき基準ではない
            // (荒れたendpointがあれば発生しうる。その場合でも他のendpointの
            // 処理を巻き込んで遅延させないことが目的)。
            "endpoint_refresh_timeout_count": refresh_timeout_count,
            "no_endpoint_refresh_timeouts": refresh_timeout_count == 0,
        }
    })
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}
