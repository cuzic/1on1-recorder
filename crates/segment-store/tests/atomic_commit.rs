//! Ported from spike-04-opus-atomic-commit's `tests/atomic_commit.rs`, adapted to:
//! - register into a shared `session_store::SessionStore` instead of a standalone
//!   `SegmentDb`, keyed by `(session_id, track, sequence)`.
//! - commit under a per-track subdirectory (`session_dir/{self,remote}/`).
//! - derive `duration_ms` from the encoded audio's own granule position instead of
//!   trusting an out-of-band value.

use chrono::Utc;
use recorder_domain::{
    AudioManifest, CaptureManifest, ConsentManifest, RemoteSourceKind, SessionId, SessionManifest,
    TrackKind,
};
use segment_store::{commit_segment, encode_segment_to_ogg_opus, scan_and_recover, CrashPoint, RecoveredKind, SegmentRequest, SAMPLE_RATE_HZ};
use session_store::SessionStore;
use std::path::Path;

const BITRATE_BPS: i32 = 32_000;
const SEGMENT_DURATION_MS: u64 = 30_000;

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
    if !dir.exists() {
        return 0;
    }
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(suffix))
        .count()
}

fn open_session(store: &SessionStore, session_id: SessionId) {
    let manifest = SessionManifest {
        schema_version: 1,
        session_id,
        started_at: Utc::now(),
        ended_at: None,
        platform: "linux".to_string(),
        app_version: "0.1.0".to_string(),
        capture: CaptureManifest {
            microphone_device_id: "mic-1".to_string(),
            remote_source_id: "speaker-1".to_string(),
            remote_source_kind: RemoteSourceKind::EndpointLoopback,
        },
        audio: AudioManifest {
            sample_rate: SAMPLE_RATE_HZ,
            segment_duration_ms: SEGMENT_DURATION_MS as u32,
            tracks: vec![TrackKind::SelfMic, TrackKind::RemoteAudio],
        },
        consent: ConsentManifest { confirmed_by_user: true, confirmed_at: Utc::now() },
    };
    store.create_session(&manifest).unwrap();
}

fn request(session_id: SessionId, track: TrackKind, sequence: u64) -> SegmentRequest {
    SegmentRequest {
        session_id,
        track,
        sequence,
        timeline_start_ms: sequence * SEGMENT_DURATION_MS,
        sample_rate: SAMPLE_RATE_HZ,
        channels: 1,
    }
}

#[test]
fn crash_after_partial_write_leaves_orphan_that_recovery_discards() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    open_session(&store, session_id);

    let pcm = sine_pcm(1.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let result = commit_segment(
        &encoded,
        tmp.path(),
        &request(session_id, TrackKind::SelfMic, 0),
        &store,
        CrashPoint::AfterPartialWrite,
    )
    .unwrap();
    assert!(result.is_none());

    let track_dir = tmp.path().join("self");
    assert_eq!(count_files(&track_dir, ".partial"), 1);
    assert_eq!(count_files(&track_dir, ".opus"), 0);

    let recovered = scan_and_recover(tmp.path(), session_id, TrackKind::SelfMic, SEGMENT_DURATION_MS, SAMPLE_RATE_HZ, 1, &store).unwrap();
    assert_eq!(recovered, vec![(0, RecoveredKind::OrphanedPartialDiscarded)]);
    assert_eq!(count_files(&track_dir, ".partial"), 0);
    assert!(!store.segment_exists(session_id, TrackKind::SelfMic, 0).unwrap());
}

#[test]
fn crash_after_fsync_leaves_orphan_that_recovery_discards() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    open_session(&store, session_id);

    let pcm = sine_pcm(1.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let result = commit_segment(
        &encoded,
        tmp.path(),
        &request(session_id, TrackKind::SelfMic, 0),
        &store,
        CrashPoint::AfterFsync,
    )
    .unwrap();
    assert!(result.is_none());

    let track_dir = tmp.path().join("self");
    assert_eq!(count_files(&track_dir, ".partial"), 1);
    assert_eq!(count_files(&track_dir, ".opus"), 0);

    let recovered = scan_and_recover(tmp.path(), session_id, TrackKind::SelfMic, SEGMENT_DURATION_MS, SAMPLE_RATE_HZ, 1, &store).unwrap();
    assert_eq!(recovered, vec![(0, RecoveredKind::OrphanedPartialDiscarded)]);
    assert_eq!(count_files(&track_dir, ".partial"), 0);
}

#[test]
fn crash_after_rename_leaves_unregistered_file_that_recovery_registers() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    open_session(&store, session_id);

    let pcm = sine_pcm(1.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let result = commit_segment(
        &encoded,
        tmp.path(),
        &request(session_id, TrackKind::SelfMic, 0),
        &store,
        CrashPoint::AfterRename,
    )
    .unwrap();
    assert!(result.is_none());

    let track_dir = tmp.path().join("self");
    // rename completed, so .opus exists but is not yet registered
    assert_eq!(count_files(&track_dir, ".partial"), 0);
    assert_eq!(count_files(&track_dir, ".opus"), 1);
    assert!(!store.segment_exists(session_id, TrackKind::SelfMic, 0).unwrap());

    let recovered = scan_and_recover(tmp.path(), session_id, TrackKind::SelfMic, SEGMENT_DURATION_MS, SAMPLE_RATE_HZ, 1, &store).unwrap();
    assert_eq!(recovered, vec![(0, RecoveredKind::UnregisteredCompleteRegistered)]);
    assert!(store.segment_exists(session_id, TrackKind::SelfMic, 0).unwrap());

    // scanning again must not double-register
    let recovered_again = scan_and_recover(tmp.path(), session_id, TrackKind::SelfMic, SEGMENT_DURATION_MS, SAMPLE_RATE_HZ, 1, &store).unwrap();
    assert_eq!(recovered_again, vec![(0, RecoveredKind::AlreadyRegistered)]);
}

#[test]
fn no_crash_commits_fully_and_recovery_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    open_session(&store, session_id);

    let pcm = sine_pcm(1.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    let committed = commit_segment(&encoded, tmp.path(), &request(session_id, TrackKind::SelfMic, 0), &store, CrashPoint::None)
        .unwrap()
        .expect("should commit fully");
    assert_eq!(committed.sequence, 0);
    assert_eq!(committed.byte_len, encoded.len() as u64);
    // 1 second of audio should be reconstructed as ~1000ms from the granule position.
    assert!((900..=1100).contains(&committed.duration_ms), "duration_ms was {}", committed.duration_ms);
    assert!(store.segment_exists(session_id, TrackKind::SelfMic, 0).unwrap());

    let track_dir = tmp.path().join("self");
    assert_eq!(count_files(&track_dir, ".partial"), 0);
    assert_eq!(count_files(&track_dir, ".opus"), 1);

    let recovered = scan_and_recover(tmp.path(), session_id, TrackKind::SelfMic, SEGMENT_DURATION_MS, SAMPLE_RATE_HZ, 1, &store).unwrap();
    assert_eq!(recovered, vec![(0, RecoveredKind::AlreadyRegistered)]);
}

#[test]
fn self_and_remote_tracks_with_the_same_sequence_do_not_collide() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    open_session(&store, session_id);

    let pcm = sine_pcm(0.5, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();

    commit_segment(&encoded, tmp.path(), &request(session_id, TrackKind::SelfMic, 0), &store, CrashPoint::None).unwrap();
    commit_segment(&encoded, tmp.path(), &request(session_id, TrackKind::RemoteAudio, 0), &store, CrashPoint::None).unwrap();

    assert!(store.segment_exists(session_id, TrackKind::SelfMic, 0).unwrap());
    assert!(store.segment_exists(session_id, TrackKind::RemoteAudio, 0).unwrap());

    let counts = store.segment_counts_by_track(session_id).unwrap();
    assert_eq!(counts.get(&TrackKind::SelfMic), Some(&1));
    assert_eq!(counts.get(&TrackKind::RemoteAudio), Some(&1));
}

#[test]
fn end_to_end_multiple_segments_roundtrip_and_are_all_recoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    open_session(&store, session_id);

    // 4 segments: 2 commit cleanly, 1 crashes AfterFsync, 1 crashes AfterRename.
    let crash_plan = [CrashPoint::None, CrashPoint::AfterFsync, CrashPoint::None, CrashPoint::AfterRename];

    for (i, crash_point) in crash_plan.iter().enumerate() {
        let pcm = sine_pcm(0.5, 220.0 + i as f32 * 110.0);
        let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();
        commit_segment(&encoded, tmp.path(), &request(session_id, TrackKind::SelfMic, i as u64), &store, *crash_point).unwrap();
    }

    // before recovery: sequences 0 and 2 are registered
    for seq in [0u64, 2] {
        assert!(store.segment_exists(session_id, TrackKind::SelfMic, seq).unwrap());
    }
    for seq in [1u64, 3] {
        assert!(!store.segment_exists(session_id, TrackKind::SelfMic, seq).unwrap());
    }

    let recovered = scan_and_recover(tmp.path(), session_id, TrackKind::SelfMic, SEGMENT_DURATION_MS, SAMPLE_RATE_HZ, 1, &store).unwrap();
    // sequence 1 (.partial) discarded, sequence 3 (unregistered .opus) registered
    assert!(recovered.contains(&(1, RecoveredKind::OrphanedPartialDiscarded)));
    assert!(recovered.contains(&(3, RecoveredKind::UnregisteredCompleteRegistered)));

    // after recovery: 0, 2, 3 registered (1 is permanently lost)
    for seq in [0u64, 2, 3] {
        assert!(store.segment_exists(session_id, TrackKind::SelfMic, seq).unwrap());
    }
    assert!(!store.segment_exists(session_id, TrackKind::SelfMic, 1).unwrap());

    for segment in store.pending_uploads(session_id).unwrap() {
        assert!(segment.local_path.exists(), "segment {} file should exist on disk", segment.sequence);
        let bytes = std::fs::read(&segment.local_path).unwrap();
        assert!(!bytes.is_empty());
    }
}

#[test]
fn segment_capacity_estimate_for_two_hour_session_at_32kbps_is_reasonable() {
    // design.md §11.1: 30-second segments at 32kbps. Encode one real segment and
    // extrapolate to a 2-hour (240-segment) session.
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

    assert!(
        encoded.len() > 20_000 && encoded.len() < 300_000,
        "segment size {} out of expected range for 30s @ {}bps",
        encoded.len(),
        BITRATE_BPS
    );
    assert!(estimated_total_mb < 300.0, "2h estimate {estimated_total_mb:.2} MB is unexpectedly large");
}

#[test]
fn produced_ogg_opus_file_is_recognized_as_valid_by_ffprobe() {
    let Some(ffprobe) = which_ffprobe() else {
        eprintln!("ffprobe not found in PATH; skipping playability check");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    open_session(&store, session_id);

    let pcm = sine_pcm(3.0, 440.0);
    let encoded = encode_segment_to_ogg_opus(&pcm, BITRATE_BPS).unwrap();
    let committed = commit_segment(&encoded, tmp.path(), &request(session_id, TrackKind::SelfMic, 0), &store, CrashPoint::None)
        .unwrap()
        .expect("should commit fully");

    let output = std::process::Command::new(&ffprobe)
        .args(["-v", "error", "-show_entries", "stream=codec_name,sample_rate,channels", "-of", "default=noprint_wrappers=1"])
        .arg(&committed.local_path)
        .output()
        .expect("failed to run ffprobe");

    assert!(output.status.success(), "ffprobe failed: stderr={}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("codec_name=opus"), "stdout was: {stdout}");
    assert!(stdout.contains("sample_rate=48000"), "stdout was: {stdout}");
    assert!(stdout.contains("channels=1"), "stdout was: {stdout}");
}

fn which_ffprobe() -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).map(|dir| dir.join("ffprobe")).find(|p| p.is_file())
}
