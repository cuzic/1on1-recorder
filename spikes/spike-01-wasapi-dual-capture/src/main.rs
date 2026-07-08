// spike-windows-01-02-detail-design.md §4.7/§4.11

mod aggregator;
mod device_select;
mod loopback_stream;
mod mic_stream;
mod wasapi_common;

use clap::Parser;
use device_select::DeviceRole;
use loopback_stream::EndpointLoopbackStream;
use mic_stream::MicCaptureStream;
use spike_common::spawn_capture_thread;
use spike_common::StopSignal;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

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
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let out_dir = cli
        .output_dir
        .unwrap_or_else(|| PathBuf::from("out").join("run"));
    std::fs::create_dir_all(&out_dir)?;

    let (tx, rx) = crossbeam_channel::bounded(256);
    let stop = Arc::new(StopSignal::new()?);
    let mic_drop_counter = Arc::new(AtomicU64::new(0));
    let loopback_drop_counter = Arc::new(AtomicU64::new(0));

    let mic_stream = Box::new(MicCaptureStream {
        device_id_or_default: cli.mic_device,
        role: cli.device_role,
        pipeline_drop_counter: mic_drop_counter.clone(),
    });
    let loopback_stream = Box::new(EndpointLoopbackStream {
        device_id_or_default: cli.render_device,
        role: cli.device_role,
        pipeline_drop_counter: loopback_drop_counter.clone(),
    });

    let mic_handle = spawn_capture_thread(mic_stream, tx.clone(), stop.clone());
    let loopback_handle = spawn_capture_thread(loopback_stream, tx.clone(), stop.clone());
    drop(tx); // Aggregator側のrxはキャプチャスレッド側のtxが尽きればループを抜ける

    let aggregator = aggregator::Aggregator::new(&out_dir)?;
    let aggregator_handle = std::thread::spawn(move || aggregator.run(rx));

    std::thread::sleep(Duration::from_secs(cli.duration_secs));
    stop.signal()?;

    let _mic_outcome = mic_handle.join().expect("mic thread panicked");
    let _loopback_outcome = loopback_handle.join().expect("loopback thread panicked");
    aggregator_handle
        .join()
        .expect("aggregator thread panicked")?;

    // TODO(§4.8): summary.jsonの構築(devices/streams/acceptanceブロック)は
    // spike_common::analyzeの各関数をCSVへ適用してから書き出す。

    Ok(())
}
