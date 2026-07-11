// spike-plan.md SPIKE-03 検証手順3: 「2時間分を高速実行し、出力2トラックの
// 長さ一致・位相ずれを自動計測する」を実行するCLI。
//
// 実機のマイク/スピーカーを一切使わない疑似音源シミュレーションのため、
// 追加の準備なしに実行するだけで合否判定まで完結する。

mod report;

use clap::Parser;
use spike_03_timeline_drift::ScenarioConfig;
use std::path::PathBuf;

#[derive(clap::Parser)]
struct Cli {
    /// シミュレーション対象時間(秒)。spike-plan.md既定は2時間(7200秒)。
    #[arg(long, default_value_t = 2.0 * 3600.0)]
    duration_secs: f64,

    #[arg(long, default_value_t = 50.0)]
    self_drift_ppm: f64,

    #[arg(long, default_value_t = -50.0)]
    remote_drift_ppm: f64,

    #[arg(long, default_value_t = 10)]
    self_packet_ms: u32,

    #[arg(long, default_value_t = 20)]
    remote_packet_ms: u32,

    #[arg(long, default_value_t = 0.001)]
    packet_loss_probability: f64,

    #[arg(long, default_value_t = 0.0005)]
    discontinuity_probability: f64,

    #[arg(long)]
    output_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    report::print_banner(
        "SPIKE-03",
        "共通タイムライン整列・drift補正(疑似音源)",
        &[
            "実機のマイク/スピーカーは使いません。疑似音源での高速シミュレーションです。",
            &format!(
                "対象時間 {:.0}秒 分のシミュレーションを実行します(リリースビルドなら数十秒〜2分程度で終わります)。",
                cli.duration_secs
            ),
            "終わるまでそのままお待ちください。",
        ],
    );

    let config = ScenarioConfig {
        duration_secs: cli.duration_secs,
        self_drift_ppm: cli.self_drift_ppm,
        remote_drift_ppm: cli.remote_drift_ppm,
        self_packet_ms: cli.self_packet_ms,
        remote_packet_ms: cli.remote_packet_ms,
        packet_loss_probability: cli.packet_loss_probability,
        discontinuity_probability: cli.discontinuity_probability,
        ..ScenarioConfig::default()
    };

    println!(
        "running {:.0}s simulation (self drift={}ppm/{}ms packets, remote drift={}ppm/{}ms packets)...",
        config.duration_secs,
        config.self_drift_ppm,
        config.self_packet_ms,
        config.remote_drift_ppm,
        config.remote_packet_ms
    );

    let (self_track, remote_track, report_data) = spike_03_timeline_drift::run_simulation(&config);

    let acceptance = serde_json::json!({
        "lengths_match": report_data.length_diff_frames == 0,
        "length_diff_frames": report_data.length_diff_frames,
        "sync_within_100ms": report_data.sync_lag_ms_max_abs <= 100.0,
        "sync_within_20ms_target": report_data.sync_lag_ms_max_abs <= 20.0,
        "realtime_speedup_at_least_10x": report_data.realtime_speedup_factor >= 10.0,
    });

    let output = serde_json::json!({
        "report": report_data,
        "acceptance": acceptance,
    });

    let text = serde_json::to_string_pretty(&output)?;

    if let Some(out_dir) = &cli.output_dir {
        std::fs::create_dir_all(out_dir)?;
        std::fs::write(out_dir.join("summary.json"), &text)?;

        // 聴感確認用にWAVも書き出す(design.mdのSelf/Remote相当)。
        write_wav(&out_dir.join("self.wav"), &self_track, spike_03_timeline_drift::NOMINAL_RATE_HZ)?;
        write_wav(&out_dir.join("remote.wav"), &remote_track, spike_03_timeline_drift::NOMINAL_RATE_HZ)?;
    }

    report::print_acceptance_report(
        "SPIKE-03",
        "共通タイムライン整列・drift補正(疑似音源)",
        &acceptance,
    );
    report::pause_before_exit();

    Ok(())
}

fn write_wav(path: &std::path::Path, samples: &[f32], sample_rate: u32) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample(s)?;
    }
    writer.finalize()?;
    Ok(())
}
