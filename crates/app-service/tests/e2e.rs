//! Stage 1's acceptance test (task #7): the full `capture -> align -> segment ->
//! encode -> commit -> upload -> finalize` pipeline, driven entirely by a pseudo
//! capture source, with no real OS capture backend or network involved (the upload
//! side is `upload-client`'s mock server). Runs on any OS, including this Linux dev
//! environment.

use app_service::pseudo_source::{generate_frames, nominal_frame_interval_ns, PseudoSourceConfig};
use app_service::run_pipeline;
use chrono::Utc;
use recorder_domain::{AudioManifest, CaptureManifest, ConsentManifest, RemoteSourceKind, SessionId, SessionManifest, TrackKind};
use session_store::SessionStore;
use std::sync::Arc;
use std::time::Duration;
use upload_client::mock_server::{spawn_test_server, FaultConfig};
use upload_client::{HttpUploadClient, StaticTokenProvider};

const SAMPLE_RATE: u32 = 48_000;
const SEGMENT_DURATION_MS: u32 = 30_000;
const DURATION_SECS: u32 = 90; // exactly 3 segments at 30s each

#[tokio::test]
async fn pseudo_source_pipeline_produces_synchronized_self_and_remote_segments_end_to_end() {
    // No faults injected: stage 1 is about proving the wiring works, not re-proving
    // upload-client's own retry/idempotency guarantees (already covered by that
    // crate's fault-injection tests).
    let (base_url, server_state) = spawn_test_server(FaultConfig { pre_process_fault_probability: 0.0, post_process_fault_probability: 0.0, timeout_simulation_probability: 0.0, timeout_sleep: Duration::from_millis(0) }).await;
    let adapter = HttpUploadClient::new(base_url, Duration::from_secs(5), Arc::new(StaticTokenProvider("test-token".to_string())));

    let store = SessionStore::open_in_memory().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();

    let manifest = SessionManifest {
        schema_version: 1,
        session_id,
        started_at: Utc::now(),
        ended_at: None,
        platform: "linux".to_string(),
        app_version: "0.1.0".to_string(),
        capture: CaptureManifest {
            microphone_device_id: "pseudo-mic".to_string(),
            remote_source_id: "pseudo-speaker".to_string(),
            remote_source_kind: RemoteSourceKind::EndpointLoopback,
        },
        audio: AudioManifest { sample_rate: SAMPLE_RATE, segment_duration_ms: SEGMENT_DURATION_MS, tracks: vec![TrackKind::SelfMic, TrackKind::RemoteAudio] },
        consent: ConsentManifest { confirmed_by_user: true, confirmed_at: Utc::now() },
    };

    // Self: captured natively at 48kHz mono (the normalization layer is a no-op here).
    let self_config = PseudoSourceConfig { duration_secs: DURATION_SECS, frame_interval_ms: 20, sample_rate: SAMPLE_RATE, channels: 1, tone_freq_hz: 440.0 };
    // Remote: captured at 44.1kHz stereo, exercising the downmix + resample path.
    let remote_config = PseudoSourceConfig { duration_secs: DURATION_SECS, frame_interval_ms: 20, sample_rate: 44_100, channels: 2, tone_freq_hz: 220.0 };

    let self_frames = generate_frames(TrackKind::SelfMic, &self_config);
    let remote_frames = generate_frames(TrackKind::RemoteAudio, &remote_config);
    let total_duration_ns = DURATION_SECS as u64 * 1_000_000_000;

    let summary = run_pipeline(
        &manifest,
        &self_frames,
        &remote_frames,
        nominal_frame_interval_ns(&self_config),
        nominal_frame_interval_ns(&remote_config),
        total_duration_ns,
        session_dir.path(),
        32_000,
        &store,
        &adapter,
    )
    .await
    .expect("pipeline should complete without error");

    assert_eq!(summary.segment_counts_by_track.get(&TrackKind::SelfMic), Some(&3));
    assert_eq!(summary.segment_counts_by_track.get(&TrackKind::RemoteAudio), Some(&3));
    assert_eq!(summary.total_duration_ms, DURATION_SECS as u64 * 1000);

    // Everything the local ledger knows about is fully uploaded.
    assert!(store.pending_uploads(session_id).unwrap().is_empty());

    // Both tracks cut at identical sequence/timeline_start_ms boundaries, and every
    // committed file actually exists with the metadata the ledger claims.
    for track in [TrackKind::SelfMic, TrackKind::RemoteAudio] {
        let segments = store.segments_for_track(session_id, track).unwrap();
        assert_eq!(segments.len(), 3);
        for (i, segment) in segments.iter().enumerate() {
            assert_eq!(segment.sequence, i as u64);
            assert_eq!(segment.timeline_start_ms, i as u64 * SEGMENT_DURATION_MS as u64);
            assert_eq!(segment.duration_ms, SEGMENT_DURATION_MS);
            assert_eq!(segment.sample_rate, SAMPLE_RATE);
            assert_eq!(segment.channels, 1);
            assert!(segment.local_path.exists(), "segment file should exist on disk: {:?}", segment.local_path);
        }
    }

    // The mock server actually received all 6 segments (2 tracks x 3 sequences),
    // each written exactly once, and finalize was accepted.
    let stats = server_state.stats.lock().unwrap();
    assert_eq!(stats.segment_write_counts.len(), 6);
    for count in stats.segment_write_counts.values() {
        assert_eq!(*count, 1);
    }
}
