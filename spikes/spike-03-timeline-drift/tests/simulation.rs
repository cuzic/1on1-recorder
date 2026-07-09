//! spike-plan.md SPIKE-03 検証手順3・合否基準の自動化。
//! `cargo test`既定(debugビルド)では短時間のシナリオで高速に確認する
//! (debugは数値計算主体のコードで10〜50倍遅くなるため、既定テストの時間は
//! 数秒で終わる長さに抑える)。実際の2時間相当のフルシミュレーションと
//! release速度検証は`--ignored`で明示的に実行する
//! (`cargo test -p spike-03-timeline-drift --release --test simulation -- --ignored`)。

use spike_03_timeline_drift::ScenarioConfig;

// リアルタイム10倍速の合否基準(design.md/spike-plan.md)は最適化コンパイルを
// 前提にした性能特性であり、`cargo test`既定のdebugビルドでは(数値計算主体の
// コードは典型的に10〜50倍遅くなるため)満たせない。実際にdebugで実行した
// ところ約7.6倍でこの基準を割り込んだため、速度検証はアルゴリズム正しさの
// 検証(長さ一致・同期差)とは分離し、`cargo test --release`または本体のCLI
// (`cargo run --release`)で確認する運用にする。
fn assert_correctness(config: &ScenarioConfig) {
    let (_self_track, _remote_track, report) = spike_03_timeline_drift::run_simulation(config);

    assert_eq!(
        report.length_diff_frames, 0,
        "Self/Remoteのトラック長が一致すること"
    );
    assert!(
        report.sync_lag_ms_max_abs <= 100.0,
        "同期差100ms以内(design.md §3.2)であること: {}ms",
        report.sync_lag_ms_max_abs
    );
    // 実際にpacket loss/discontinuityが注入されたことを確認する(0件のまま
    // だとテストが何も検証していないことになるため)。
    assert!(report.self_stats.gaps_detected > 0);
    assert!(report.self_stats.hard_jumps > 0);
}

#[test]
fn short_scenario_meets_acceptance_criteria() {
    let config = ScenarioConfig {
        duration_secs: 120.0,
        ..ScenarioConfig::default()
    };
    assert_correctness(&config);
}

#[test]
fn no_drift_no_loss_scenario_is_still_length_exact_and_in_sync() {
    // ドリフト・欠落が一切ないベースラインでも、異なるコールバック周期
    // (10ms vs 20ms)だけでアライメントが破綻しないことを確認する。
    let config = ScenarioConfig {
        duration_secs: 60.0,
        self_drift_ppm: 0.0,
        remote_drift_ppm: 0.0,
        packet_loss_probability: 0.0,
        discontinuity_probability: 0.0,
        ..ScenarioConfig::default()
    };
    let (_self_track, _remote_track, report) = spike_03_timeline_drift::run_simulation(&config);
    assert_eq!(report.length_diff_frames, 0);
    assert_eq!(report.self_stats.gaps_detected, 0);
    assert_eq!(report.self_stats.hard_jumps, 0);
    assert!(report.sync_lag_ms_max_abs <= 20.0);
}

#[test]
#[ignore = "2時間相当の完全なシミュレーション。手動実行用(spike-plan.md検証手順3)"]
fn full_two_hour_scenario_meets_acceptance_criteria() {
    let config = ScenarioConfig::default();
    assert_correctness(&config);
}

#[test]
#[ignore = "release ビルドで実行すること: cargo test -p spike-03-timeline-drift --release --test simulation -- --ignored realtime_speedup"]
fn realtime_speedup_is_at_least_10x_in_release_build() {
    let config = ScenarioConfig {
        duration_secs: 600.0,
        ..ScenarioConfig::default()
    };
    let (_self_track, _remote_track, report) = spike_03_timeline_drift::run_simulation(&config);
    assert!(
        report.realtime_speedup_factor >= 10.0,
        "リアルタイムの10倍速以上で処理できること(releaseビルドで実行すること): {}x",
        report.realtime_speedup_factor
    );
}
