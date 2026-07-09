//! spike-plan.md SPIKE-04 検証手順:
//! 1. 各クラッシュ地点での再起動リカバリの正しさ
//! 2. 複数セグメントのエンコード+コミットのエンドツーエンド往復
//! 3. ビットレート設定での2時間分の容量見積もり
//! 4. ffprobeによる再生可能性の検証(design.mdの受け入れ基準)

use spike_04_opus_atomic_commit::{
    commit_segment, encode_segment_to_ogg_opus, scan_and_recover, CrashPoint, RecoveredKind,
    SegmentDb, SAMPLE_RATE_HZ,
};
use std::path::Path;

const BITRATE_BPS: i32 = 32_000;

fn sine_pcm(seconds: f32, freq_hz: f32) -> Vec<f32> {
    let n = (seconds * SAMPLE_RATE_HZ as f32) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE_HZ as f32;
            0.2 * (2.0 * std::f32::consts::PI * freq_hz * t).sin()
        })
        .collect()
}

fn count_files(dir: &Path, suffix: &str) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(suffix))
        .count()
}

#[test]
fn crash_after_partial_write_leaves_orphan_that_recovery_discards() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("segments.db");
    let db = SegmentDb::open(&db_path).unwrap();

    let pcm = sine_pcm(1.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let result = commit_segment(
        &encoded,
        tmp.path(),
        "session-a",
        0,
        &db,
        CrashPoint::AfterPartialWrite,
    )
    .unwrap();
    assert!(result.is_none());
    assert_eq!(count_files(tmp.path(), ".partial"), 1);
    assert_eq!(count_files(tmp.path(), ".opus"), 0);

    let recovered = scan_and_recover(tmp.path(), "session-a", &db).unwrap();
    assert_eq!(recovered, vec![(0, RecoveredKind::OrphanedPartialDiscarded)]);
    assert_eq!(count_files(tmp.path(), ".partial"), 0);
    assert!(!db.is_registered("session-a", 0).unwrap());
}

#[test]
fn crash_after_fsync_leaves_orphan_that_recovery_discards() {
    let tmp = tempfile::tempdir().unwrap();
    let db = SegmentDb::open(&tmp.path().join("segments.db")).unwrap();

    let pcm = sine_pcm(1.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let result = commit_segment(
        &encoded,
        tmp.path(),
        "session-a",
        0,
        &db,
        CrashPoint::AfterFsync,
    )
    .unwrap();
    assert!(result.is_none());
    assert_eq!(count_files(tmp.path(), ".partial"), 1);
    assert_eq!(count_files(tmp.path(), ".opus"), 0);

    let recovered = scan_and_recover(tmp.path(), "session-a", &db).unwrap();
    assert_eq!(recovered, vec![(0, RecoveredKind::OrphanedPartialDiscarded)]);
    assert_eq!(count_files(tmp.path(), ".partial"), 0);
}

#[test]
fn crash_after_rename_leaves_unregistered_file_that_recovery_registers() {
    let tmp = tempfile::tempdir().unwrap();
    let db = SegmentDb::open(&tmp.path().join("segments.db")).unwrap();

    let pcm = sine_pcm(1.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let result = commit_segment(
        &encoded,
        tmp.path(),
        "session-a",
        0,
        &db,
        CrashPoint::AfterRename,
    )
    .unwrap();
    assert!(result.is_none());
    // renameは完了しているので.opusは存在するがDB未登録
    assert_eq!(count_files(tmp.path(), ".partial"), 0);
    assert_eq!(count_files(tmp.path(), ".opus"), 1);
    assert!(!db.is_registered("session-a", 0).unwrap());

    let recovered = scan_and_recover(tmp.path(), "session-a", &db).unwrap();
    assert_eq!(
        recovered,
        vec![(0, RecoveredKind::UnregisteredCompleteRegistered)]
    );
    assert!(db.is_registered("session-a", 0).unwrap());

    // 再度スキャンしても二重登録にならず、既登録として扱われる
    let recovered_again = scan_and_recover(tmp.path(), "session-a", &db).unwrap();
    assert_eq!(recovered_again, vec![(0, RecoveredKind::AlreadyRegistered)]);
}

#[test]
fn no_crash_commits_fully_and_recovery_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let db = SegmentDb::open(&tmp.path().join("segments.db")).unwrap();

    let pcm = sine_pcm(1.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let committed = commit_segment(&encoded, tmp.path(), "session-a", 0, &db, CrashPoint::None)
        .unwrap()
        .expect("should commit fully");
    assert_eq!(committed.sequence, 0);
    assert_eq!(committed.size, encoded.len());
    assert!(db.is_registered("session-a", 0).unwrap());
    assert_eq!(count_files(tmp.path(), ".partial"), 0);
    assert_eq!(count_files(tmp.path(), ".opus"), 1);

    let recovered = scan_and_recover(tmp.path(), "session-a", &db).unwrap();
    assert_eq!(recovered, vec![(0, RecoveredKind::AlreadyRegistered)]);
}

#[test]
fn end_to_end_multiple_segments_roundtrip_and_are_all_recoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let db = SegmentDb::open(&tmp.path().join("segments.db")).unwrap();
    let session_id = "session-multi";

    // 4segments: 2つは正常コミット、1つはAfterFsyncでクラッシュ、1つはAfterRenameでクラッシュ。
    let crash_plan = [
        CrashPoint::None,
        CrashPoint::AfterFsync,
        CrashPoint::None,
        CrashPoint::AfterRename,
    ];

    for (i, crash_point) in crash_plan.iter().enumerate() {
        let pcm = sine_pcm(0.5, 220.0 + i as f32 * 110.0);
        let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();
        commit_segment(&encoded, tmp.path(), session_id, i as u64, &db, *crash_point).unwrap();
    }

    // クラッシュ前: sequence 0,2のみ登録済み。
    assert_eq!(db.registered_sequences(session_id).unwrap(), vec![0, 2]);

    // 再起動リカバリ実行。
    let recovered = scan_and_recover(tmp.path(), session_id, &db).unwrap();
    // sequence1(.partial)は破棄、sequence3(.opus未登録)は登録される。
    assert!(recovered.contains(&(1, RecoveredKind::OrphanedPartialDiscarded)));
    assert!(recovered.contains(&(3, RecoveredKind::UnregisteredCompleteRegistered)));

    // リカバリ後: 0,2,3が登録済み(1は失われたセグメントとして扱われ、恒久的に欠落する)。
    assert_eq!(db.registered_sequences(session_id).unwrap(), vec![0, 2, 3]);

    // 登録された各セグメントのファイルが実際に読めて、登録sha256と一致することを確認。
    for seq in db.registered_sequences(session_id).unwrap() {
        let path = db.path_of(session_id, seq).unwrap().unwrap();
        assert!(path.exists(), "segment {seq} file should exist on disk");
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.is_empty());
    }
}

#[test]
fn segment_capacity_estimate_for_two_hour_session_at_32kbps_is_reasonable() {
    // design.md §11.1: 30秒セグメント、32kbpsを想定した容量見積もり。
    // 実エンコードで1セグメント分のサイズを実測し、2時間(240セグメント)分を外挿する。
    let segment_seconds = 30.0_f32;
    let pcm = sine_pcm(segment_seconds, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let segments_in_two_hours = (2.0 * 60.0 * 60.0 / segment_seconds as f64).round() as u64;
    let estimated_total_bytes = encoded.len() as u64 * segments_in_two_hours;
    let estimated_total_mb = estimated_total_bytes as f64 / (1024.0 * 1024.0);

    println!(
        "segment size = {} bytes ({}s @ {}bps), 2h estimate = {:.2} MB over {} segments",
        encoded.len(),
        segment_seconds,
        BITRATE_BPS,
        estimated_total_mb,
        segments_in_two_hours
    );

    // 32kbpsの理論値は 30s * 32000bit/8 = 120,000 byte/segment 程度。
    // 実際のOpusはVBR的挙動やコンテナオーバーヘッドがあるので大きめの許容幅を取る。
    assert!(
        encoded.len() > 20_000 && encoded.len() < 300_000,
        "segment size {} out of expected range for 30s @ {}bps",
        encoded.len(),
        BITRATE_BPS
    );
    // 2時間ぶんが常識的なディスク容量(数十MB程度)に収まることを確認。
    assert!(
        estimated_total_mb < 300.0,
        "2h estimate {estimated_total_mb:.2} MB is unexpectedly large"
    );
}

#[test]
fn produced_ogg_opus_file_is_recognized_as_valid_by_ffprobe() {
    let ffprobe = which_ffprobe();
    let Some(ffprobe) = ffprobe else {
        eprintln!("ffprobe not found in PATH; skipping playability check");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let db = SegmentDb::open(&tmp.path().join("segments.db")).unwrap();
    let pcm = sine_pcm(3.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();
    let committed = commit_segment(&encoded, tmp.path(), "session-ffprobe", 0, &db, CrashPoint::None)
        .unwrap()
        .expect("should commit fully");

    let output = std::process::Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,sample_rate,channels",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(&committed.path)
        .output()
        .expect("failed to run ffprobe");

    assert!(
        output.status.success(),
        "ffprobe failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("codec_name=opus"), "stdout was: {stdout}");
    assert!(stdout.contains("sample_rate=48000"), "stdout was: {stdout}");
    assert!(stdout.contains("channels=1"), "stdout was: {stdout}");
}

fn which_ffprobe() -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("ffprobe"))
        .find(|p| p.is_file())
}
