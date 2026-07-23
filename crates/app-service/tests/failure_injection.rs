//! Task #13: integration tests for failure scenarios a real deployment can hit —
//! disk-write failure during recording, a network outage that lasts through the
//! end of a session, and resuming/idempotently re-sending after a restart. Builds
//! directly on stage 3's `session_lifecycle`/`upload_worker` (task #11).

use app_service::pseudo_source::{generate_frames, nominal_frame_interval_ns, PseudoSourceConfig};
use app_service::run_pipeline;
use app_service::{begin_session, end_session, recover_incomplete_sessions};
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
const NO_FAULTS: FaultConfig = FaultConfig { pre_process_fault_probability: 0.0, post_process_fault_probability: 0.0, timeout_simulation_probability: 0.0, timeout_sleep: Duration::from_millis(0) };

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

/// A base URL nothing is listening on — any request against it fails with a
/// connection error (`reqwest`'s equivalent of "the network/server is
/// unreachable"), classified by `upload-client` as `UploadError::Transport`
/// (retryable) or, for `create_session`, surfaced directly as an error.
async fn unreachable_base_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // nothing is listening at this address once dropped
    format!("http://{addr}")
}

#[cfg(unix)]
#[tokio::test]
async fn disk_write_failure_during_recording_leaves_a_recoverable_session() {
    use std::os::unix::fs::PermissionsExt;

    let (base_url, _server_state) = spawn_test_server(NO_FAULTS).await;
    let adapter = HttpUploadClient::new(base_url, Duration::from_secs(5), Arc::new(StaticTokenProvider("test-token".to_string())));

    let store = SessionStore::open_in_memory().unwrap();
    let sessions_root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let manifest = manifest(session_id);
    let session_dir = sessions_root.path().join(session_id.to_string());
    std::fs::create_dir_all(&session_dir).unwrap();
    // Read+execute only: segment-store's commit_segment tries to create a file
    // under here and gets a permission error — the same failure mode as running
    // out of disk space (the OS refuses the write), without needing a real
    // size-limited filesystem to reproduce.
    std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let duration_secs = 30u32; // 1 segment per track
    let config = PseudoSourceConfig { duration_secs, frame_interval_ms: 20, sample_rate: SAMPLE_RATE, channels: 1, tone_freq_hz: 440.0 };
    let self_frames = generate_frames(TrackKind::SelfMic, &config);
    let remote_frames = generate_frames(TrackKind::RemoteAudio, &config);
    let total_duration_ns = duration_secs as u64 * 1_000_000_000;

    let result = run_pipeline(
        &manifest,
        &self_frames,
        &remote_frames,
        nominal_frame_interval_ns(&config),
        nominal_frame_interval_ns(&config),
        total_duration_ns,
        &session_dir,
        32_000,
        &store,
        &adapter,
    )
    .await;

    assert!(result.is_err(), "commit_segment should fail against a read-only session directory");
    // begin_session already ran, so the session exists locally, but it never got
    // past Recording — commit_and_upload_track's `?` on commit_segment's error
    // aborts before end_session (Stopping/Finalizing/Finalized) ever runs.
    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Recording);

    // "The disk issue is fixed": restore write permission, then recover at
    // (simulated) startup exactly as a real restart would.
    std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let recovered = recover_incomplete_sessions(&store, &adapter, sessions_root.path(), SEGMENT_DURATION_MS as u64, SAMPLE_RATE, 1, Duration::from_millis(10), 10)
        .await
        .expect("recovery should succeed once the directory is writable again");

    // No segments were ever committed before the failure (the very first commit
    // attempt is what failed), so there is nothing to recover into segment-store —
    // but the session itself is still correctly recognized as needing recovery and
    // gets driven to Finalized (persisting "zero segments captured" rather than
    // being stuck forever in Recording).
    assert_eq!(recovered, vec![session_id]);
    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Finalized);
}

#[tokio::test]
async fn network_outage_from_session_start_captures_locally_and_is_recovered_once_the_network_is_back() {
    let unreachable = unreachable_base_url().await;
    let adapter = HttpUploadClient::new(unreachable, Duration::from_millis(200), Arc::new(StaticTokenProvider("test-token".to_string()))).with_max_attempts(1);

    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    let manifest = manifest(session_id);

    let remote = begin_session(&store, &adapter, &manifest).await.unwrap();
    assert!(remote.is_none(), "create_session failing should not fail begin_session — local capture proceeds");

    // The local ledger already has the session (from session-store's own
    // create_session call, which happens before the remote one) — remote
    // registration failing is non-fatal, so it still advances to Recording
    // (local capture has no dependency on the remote API), just with no
    // remote_session_id yet.
    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Recording);
    assert_eq!(store.remote_session_id(session_id).unwrap(), None);

    // Once the network is back, recovery reconstructs this session's manifest
    // (session_manifest) and retries create_session (try_register_remote_session
    // in session_lifecycle.rs) — no segments were ever captured in this test (the
    // failure happened before any capture started), so finalizing succeeds
    // immediately with zero segments.
    let (working_base_url, _server_state) = spawn_test_server(NO_FAULTS).await;
    let working_adapter = HttpUploadClient::new(working_base_url, Duration::from_secs(5), Arc::new(StaticTokenProvider("test-token".to_string())));
    let sessions_root = tempfile::tempdir().unwrap();
    let recovered = recover_incomplete_sessions(&store, &working_adapter, sessions_root.path(), SEGMENT_DURATION_MS as u64, SAMPLE_RATE, 1, Duration::from_millis(10), 10)
        .await
        .unwrap();

    assert_eq!(recovered, vec![session_id]);
    assert!(store.remote_session_id(session_id).unwrap().is_some(), "recovery should have registered a remote session");
    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Finalized);
}

/// The direct regression test for #4: a remote API that is unreachable for the
/// *entire* session (both `begin_session`'s initial attempt and `run_pipeline`'s
/// one extra attempt right before finalizing) must not lose any locally
/// captured audio — every segment should still land in segment-store and on
/// disk, and `run_pipeline` should return a real `Ok(summary)`, not an error.
#[tokio::test]
async fn run_pipeline_survives_remote_registration_failure_and_preserves_local_capture() {
    let unreachable = unreachable_base_url().await;
    let adapter = HttpUploadClient::new(unreachable, Duration::from_millis(200), Arc::new(StaticTokenProvider("test-token".to_string()))).with_max_attempts(1);

    let store = SessionStore::open_in_memory().unwrap();
    let sessions_root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let manifest = manifest(session_id);
    let session_dir = sessions_root.path().join(session_id.to_string());

    let duration_secs = 30u32; // 1 segment per track
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
        &session_dir,
        32_000,
        &store,
        &adapter,
    )
    .await
    .expect("local capture must succeed even though the remote API is never reachable");

    assert_eq!(summary.segment_counts_by_track.get(&TrackKind::SelfMic), Some(&1));
    assert_eq!(summary.segment_counts_by_track.get(&TrackKind::RemoteAudio), Some(&1));
    assert_eq!(summary.total_duration_ms, duration_secs as u64 * 1000);

    for track in [TrackKind::SelfMic, TrackKind::RemoteAudio] {
        let segments = store.segments_for_track(session_id, track).unwrap();
        assert_eq!(segments.len(), 1);
        assert!(segments[0].local_path.exists(), "segment file should exist on disk: {:?}", segments[0].local_path);
    }

    assert_eq!(store.remote_session_id(session_id).unwrap(), None);
    // update_capture_state's ended_at CASE WHEN must cover 'failed', not just
    // 'finalized' — otherwise a session marked Failed here would keep
    // ended_at NULL in the ledger forever, even though the in-memory
    // SessionSummary above already reports a real ended_at.
    let listed = store.list_sessions().unwrap();
    let listed_session = listed.iter().find(|s| s.session_id == session_id).expect("session should be listed");
    assert!(listed_session.ended_at.is_some(), "ended_at should be set once the session is marked Failed, not left NULL");
    match store.capture_state(session_id).unwrap() {
        CaptureState::Failed { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Failed{{recoverable: true}} (never registered remotely), got {other:?}"),
    }

    // The actual point of `recoverable: true`: once the remote API is reachable
    // again — even in a *later* process (simulated here via a fresh
    // `recover_incomplete_sessions` call) — this session's real captured audio
    // gets registered, uploaded, and finalized, instead of sitting stranded
    // forever with no remote_session_id.
    let (working_base_url, server_state) = spawn_test_server(NO_FAULTS).await;
    let working_adapter = HttpUploadClient::new(working_base_url, Duration::from_secs(5), Arc::new(StaticTokenProvider("test-token".to_string())));
    let recovered = recover_incomplete_sessions(&store, &working_adapter, sessions_root.path(), SEGMENT_DURATION_MS as u64, SAMPLE_RATE, 1, Duration::from_millis(10), 10)
        .await
        .unwrap();

    assert_eq!(recovered, vec![session_id]);
    assert!(store.remote_session_id(session_id).unwrap().is_some());
    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Finalized);
    assert!(store.pending_uploads(session_id).unwrap().is_empty());

    let stats = server_state.stats.lock().unwrap();
    assert_eq!(stats.segment_write_counts.len(), 2, "both previously-local-only segments should have been uploaded during recovery");
}

#[tokio::test]
async fn network_outage_through_end_of_session_is_recovered_and_finalized_idempotently_after_restart() {
    let (working_base_url, server_state) = spawn_test_server(NO_FAULTS).await;
    let working_adapter = HttpUploadClient::new(working_base_url, Duration::from_secs(5), Arc::new(StaticTokenProvider("test-token".to_string())));

    let unreachable = unreachable_base_url().await;
    let broken_adapter = HttpUploadClient::new(unreachable, Duration::from_millis(100), Arc::new(StaticTokenProvider("test-token".to_string()))).with_max_attempts(1);

    let store = SessionStore::open_in_memory().unwrap();
    let sessions_root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let manifest = manifest(session_id);
    let session_dir = sessions_root.path().join(session_id.to_string());

    // Session starts while the network is up: create_session succeeds, one
    // segment commits and uploads fine.
    let remote = begin_session(&store, &working_adapter, &manifest).await.unwrap().expect("working adapter should register a remote session");
    let pcm = vec![0.0f32; SAMPLE_RATE as usize];
    let encoded = encode_segment_to_ogg_opus(&pcm, 32_000).unwrap();
    let seq0 = SegmentRequest { session_id, track: TrackKind::SelfMic, sequence: 0, timeline_start_ms: 0, sample_rate: SAMPLE_RATE, channels: 1 };
    let segment0 = commit_segment(&encoded, &session_dir, &seq0, &store, CrashPoint::None).unwrap().unwrap();
    working_adapter.upload_segment(&remote, &segment0).await.unwrap();
    store
        .update_upload_state(session_id, TrackKind::SelfMic, 0, &recorder_domain::UploadState::Completed)
        .unwrap();

    // Network drops before the second segment can be uploaded, and stays down
    // through the rest of the recording and the attempted finalize.
    let seq1 = SegmentRequest { session_id, track: TrackKind::SelfMic, sequence: 1, timeline_start_ms: SEGMENT_DURATION_MS as u64, sample_rate: SAMPLE_RATE, channels: 1 };
    let segment1 = commit_segment(&encoded, &session_dir, &seq1, &store, CrashPoint::None).unwrap().unwrap();
    assert!(broken_adapter.upload_segment(&remote, &segment1).await.is_err());
    store
        .update_upload_state(session_id, TrackKind::SelfMic, 1, &recorder_domain::UploadState::Failed { retryable: true, reason: "network unreachable".to_string() })
        .unwrap();

    let end_result = end_session(&store, &broken_adapter, &remote, session_id, 60_000, Duration::from_millis(10), 3).await;
    assert!(end_result.is_err(), "finalize_session should fail while the network is still down (segment 1 never uploaded)");
    assert_ne!(store.capture_state(session_id).unwrap(), CaptureState::Finalized);

    // "Process restart" with the network back: recovery resumes and finishes.
    let recovered = recover_incomplete_sessions(&store, &working_adapter, sessions_root.path(), SEGMENT_DURATION_MS as u64, SAMPLE_RATE, 1, Duration::from_millis(10), 10)
        .await
        .unwrap();

    assert_eq!(recovered, vec![session_id]);
    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Finalized);
    assert!(store.pending_uploads(session_id).unwrap().is_empty());

    // Segment 0 was uploaded once; segment 1 was attempted (and failed) once
    // against the broken adapter and once more (successfully) during recovery —
    // the server's Idempotency-Key-based dedup means its actual write still only
    // happened once.
    let stats = server_state.stats.lock().unwrap();
    assert_eq!(stats.segment_write_counts.len(), 2);
    for count in stats.segment_write_counts.values() {
        assert_eq!(*count, 1);
    }
}
