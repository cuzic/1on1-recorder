//! Stage 3's acceptance tests (task #11): an upload failure during recording
//! doesn't abort the pipeline (it's drained at finalize time instead), and a
//! session a previous process instance left mid-flight is recovered at startup —
//! its interrupted segment commit is picked up by `segment-store`'s restart scan,
//! any still-pending uploads are sent, and the session is finalized.

use app_service::pseudo_source::{generate_frames, nominal_frame_interval_ns, PseudoSourceConfig};
use app_service::{recover_incomplete_sessions, run_pipeline};
use chrono::Utc;
use recorder_domain::{
    AudioManifest, CaptureManifest, CaptureState, ConsentManifest, RemoteSourceKind, SessionId,
    SessionManifest, TrackKind, UploadAdapter,
};
use segment_store::{commit_segment, encode_segment_to_ogg_opus, CrashPoint, SegmentRequest};
use session_store::SessionStore;
use std::sync::Arc;
use std::time::Duration;
use upload_client::mock_server::{spawn_test_server, FaultConfig};
use upload_client::{HttpUploadClient, StaticTokenProvider};

const SAMPLE_RATE: u32 = 48_000;
const SEGMENT_DURATION_MS: u32 = 30_000;

fn manifest(session_id: SessionId) -> SessionManifest {
    SessionManifest {
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
    }
}

#[tokio::test]
async fn upload_failures_during_recording_are_drained_at_finalize_instead_of_aborting() {
    // High fault rates and no internal retry (max_attempts=1): some segments will
    // fail their in-pipeline upload attempt. The pipeline must still commit every
    // segment and reach finalize, catching failed ones up via the upload-drain
    // pass baked into `end_session`.
    let fault_config = FaultConfig { pre_process_fault_probability: 0.4, post_process_fault_probability: 0.2, timeout_simulation_probability: 0.0, timeout_sleep: Duration::from_millis(0) };
    let (base_url, server_state) = spawn_test_server(fault_config).await;
    let adapter = HttpUploadClient::new(base_url, Duration::from_secs(5), Arc::new(StaticTokenProvider("test-token".to_string()))).with_max_attempts(1);

    let store = SessionStore::open_in_memory().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let manifest = manifest(session_id);

    let duration_secs = 60u32; // 2 segments per track
    let config = PseudoSourceConfig { duration_secs, frame_interval_ms: 20, sample_rate: SAMPLE_RATE, channels: 1, tone_freq_hz: 440.0 };
    let self_frames = generate_frames(TrackKind::SelfMic, &config);
    let remote_frames = generate_frames(TrackKind::RemoteAudio, &config);
    let total_duration_ns = duration_secs as u64 * 1_000_000_000;

    let summary = run_pipeline(
        &manifest,
        &self_frames,
        &remote_frames,
        nominal_frame_interval_ns(&config),
        nominal_frame_interval_ns(&config),
        total_duration_ns,
        session_dir.path(),
        32_000,
        &store,
        &adapter,
    )
    .await
    .expect("pipeline should reach finalize despite in-pipeline upload faults");

    assert_eq!(summary.segment_counts_by_track.get(&TrackKind::SelfMic), Some(&2));
    assert_eq!(summary.segment_counts_by_track.get(&TrackKind::RemoteAudio), Some(&2));
    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Finalized);

    // Every segment ends up uploaded, whether it succeeded on the first in-pipeline
    // attempt or was drained afterward by end_session's retry pass.
    assert!(store.pending_uploads(session_id).unwrap().is_empty());
    let stats = server_state.stats.lock().unwrap();
    assert_eq!(stats.segment_write_counts.len(), 4); // 2 tracks x 2 sequences
    for count in stats.segment_write_counts.values() {
        assert_eq!(*count, 1);
    }
}

#[tokio::test]
async fn a_session_interrupted_mid_commit_is_recovered_uploaded_and_finalized_at_startup() {
    let (base_url, server_state) = spawn_test_server(FaultConfig { pre_process_fault_probability: 0.0, post_process_fault_probability: 0.0, timeout_simulation_probability: 0.0, timeout_sleep: Duration::from_millis(0) }).await;
    let adapter = HttpUploadClient::new(base_url, Duration::from_secs(5), Arc::new(StaticTokenProvider("test-token".to_string())));

    let store = SessionStore::open_in_memory().unwrap();
    let sessions_root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let manifest = manifest(session_id);
    let session_dir = sessions_root.path().join(session_id.to_string());

    // --- "Before the crash": a session got as far as registering its manifest,
    // creating the remote session, starting to record, and committing one segment
    // fully — then a second segment's commit made it as far as the atomic rename
    // but never reached DB registration before the process died.
    {
        store.create_session(&manifest).unwrap();
        let remote = adapter.create_session(&manifest).await.expect("create_session failed");
        store.set_remote_session_id(session_id, &remote.remote_session_id).unwrap();
        store.update_capture_state(session_id, &CaptureState::Recording).unwrap();

        let pcm = vec![0.0f32; SAMPLE_RATE as usize]; // 1s of silence per segment, content doesn't matter here
        let encoded = encode_segment_to_ogg_opus(&pcm, 32_000).unwrap();

        let seq0 = SegmentRequest { session_id, track: TrackKind::SelfMic, sequence: 0, timeline_start_ms: 0, sample_rate: SAMPLE_RATE, channels: 1 };
        commit_segment(&encoded, &session_dir, &seq0, &store, CrashPoint::None).unwrap();

        let seq1 = SegmentRequest { session_id, track: TrackKind::SelfMic, sequence: 1, timeline_start_ms: SEGMENT_DURATION_MS as u64, sample_rate: SAMPLE_RATE, channels: 1 };
        let crashed = commit_segment(&encoded, &session_dir, &seq1, &store, CrashPoint::AfterRename).unwrap();
        assert!(crashed.is_none(), "CrashPoint::AfterRename should short-circuit before registration");
        assert!(!store.segment_exists(session_id, TrackKind::SelfMic, 1).unwrap(), "segment 1 must not be registered yet — that's the point of the crash simulation");

        // Process "dies" here: capture_state is left at Recording (non-terminal),
        // and nothing ever calls end_session.
    }

    // --- "Restart": a fresh SessionStore handle is not needed here since
    // open_in_memory can't survive a real process restart anyway — the important
    // simulated fact is that nothing after the block above ran before this call.
    let recovered = recover_incomplete_sessions(
        &store,
        &adapter,
        sessions_root.path(),
        SEGMENT_DURATION_MS as u64,
        SAMPLE_RATE,
        1,
        Duration::from_millis(10),
        10,
    )
    .await
    .expect("recovery should succeed");

    assert_eq!(recovered, vec![session_id]);
    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Finalized);

    // Segment 1's commit is now complete: scan_and_recover found the renamed-but-
    // unregistered .opus file and registered it.
    let segments = store.segments_for_track(session_id, TrackKind::SelfMic).unwrap();
    assert_eq!(segments.iter().map(|s| s.sequence).collect::<Vec<_>>(), vec![0, 1]);

    // Both segments got uploaded during recovery's drain pass.
    assert!(store.pending_uploads(session_id).unwrap().is_empty());
    let stats = server_state.stats.lock().unwrap();
    assert_eq!(stats.segment_write_counts.len(), 2);
    for count in stats.segment_write_counts.values() {
        assert_eq!(*count, 1);
    }
}
