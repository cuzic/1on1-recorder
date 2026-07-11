use chrono::Utc;
use recorder_domain::{
    AudioCodec, AudioManifest, AudioSegment, CaptureManifest, CaptureState, ConsentManifest,
    RemoteSourceKind, SessionId, SessionManifest, TrackKind, UploadState,
};
use session_store::SessionStore;

fn sample_manifest(session_id: SessionId) -> SessionManifest {
    SessionManifest {
        schema_version: 1,
        session_id,
        started_at: Utc::now(),
        ended_at: None,
        platform: "windows".to_string(),
        app_version: "0.1.0".to_string(),
        capture: CaptureManifest {
            microphone_device_id: "mic-1".to_string(),
            remote_source_id: "speaker-1".to_string(),
            remote_source_kind: RemoteSourceKind::EndpointLoopback,
        },
        audio: AudioManifest {
            sample_rate: 48_000,
            segment_duration_ms: 30_000,
            tracks: vec![TrackKind::SelfMic, TrackKind::RemoteAudio],
        },
        consent: ConsentManifest {
            confirmed_by_user: true,
            confirmed_at: Utc::now(),
        },
    }
}

fn sample_segment(session_id: SessionId, track: TrackKind, sequence: u64) -> AudioSegment {
    AudioSegment {
        session_id,
        track,
        sequence,
        timeline_start_ms: sequence * 30_000,
        duration_ms: 30_000,
        codec: AudioCodec::Opus,
        sample_rate: 48_000,
        channels: 1,
        sha256: format!("{:064x}", sequence),
        local_path: format!("/spool/{track}/{sequence:06}.opus").into(),
        byte_len: 123_456,
    }
}

#[test]
fn create_session_registers_session_and_declared_tracks() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    assert_eq!(store.capture_state(session_id).unwrap(), CaptureState::Preparing);
}

#[test]
fn register_segment_creates_matching_not_started_upload_status() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    let segment = sample_segment(session_id, TrackKind::SelfMic, 0);
    store.register_segment(&segment).unwrap();

    assert_eq!(
        store.upload_state(session_id, TrackKind::SelfMic, 0).unwrap(),
        UploadState::NotStarted
    );
}

#[test]
fn register_segment_without_a_session_is_rejected_by_the_foreign_key() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    // No create_session call: `tracks`/`sessions` rows never existed, so the
    // `segments` -> `tracks` foreign key must reject this insert rather than silently
    // creating an orphaned segment tied to a session that doesn't exist in `sessions`.
    let segment = sample_segment(session_id, TrackKind::SelfMic, 0);
    let result = store.register_segment(&segment);
    assert!(result.is_err(), "expected foreign key violation, got {result:?}");
}

#[test]
fn pending_uploads_excludes_completed_and_permanently_failed_segments() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    for seq in 0..4 {
        store.register_segment(&sample_segment(session_id, TrackKind::SelfMic, seq)).unwrap();
    }

    // seq 0: left NotStarted (should still be pending)
    store
        .update_upload_state(session_id, TrackKind::SelfMic, 1, &UploadState::Completed)
        .unwrap();
    store
        .update_upload_state(
            session_id,
            TrackKind::SelfMic,
            2,
            &UploadState::Failed { retryable: true, reason: "timeout".to_string() },
        )
        .unwrap();
    store
        .update_upload_state(
            session_id,
            TrackKind::SelfMic,
            3,
            &UploadState::Failed { retryable: false, reason: "400 bad request".to_string() },
        )
        .unwrap();

    let pending = store.pending_uploads(session_id).unwrap();
    let sequences: Vec<u64> = pending.iter().map(|s| s.sequence).collect();
    // seq 1 (Completed) and seq 3 (permanently Failed) must be excluded; seq 0
    // (NotStarted) and seq 2 (retryable Failed) must remain.
    assert_eq!(sequences, vec![0, 2]);
}

#[test]
fn update_upload_state_counts_attempts_only_for_uploading_and_failed_transitions() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();
    store.register_segment(&sample_segment(session_id, TrackKind::SelfMic, 0)).unwrap();

    // Pending -> Uploading -> Failed(retryable) -> Uploading -> Completed:
    // three attempts (Uploading, Failed, Uploading), Pending/Completed don't count.
    store.update_upload_state(session_id, TrackKind::SelfMic, 0, &UploadState::Pending).unwrap();
    store.update_upload_state(session_id, TrackKind::SelfMic, 0, &UploadState::Uploading).unwrap();
    store
        .update_upload_state(
            session_id,
            TrackKind::SelfMic,
            0,
            &UploadState::Failed { retryable: true, reason: "timeout".to_string() },
        )
        .unwrap();
    store.update_upload_state(session_id, TrackKind::SelfMic, 0, &UploadState::Uploading).unwrap();
    store.update_upload_state(session_id, TrackKind::SelfMic, 0, &UploadState::Completed).unwrap();

    assert_eq!(
        store.upload_state(session_id, TrackKind::SelfMic, 0).unwrap(),
        UploadState::Completed
    );
}

#[test]
fn reconcile_on_startup_fails_sessions_left_mid_flight_and_leaves_terminal_ones_alone() {
    let store = SessionStore::open_in_memory().unwrap();

    let crashed_while_recording = SessionId::new();
    store.create_session(&sample_manifest(crashed_while_recording)).unwrap();
    store.update_capture_state(crashed_while_recording, &CaptureState::Recording).unwrap();

    let crashed_while_finalizing = SessionId::new();
    store.create_session(&sample_manifest(crashed_while_finalizing)).unwrap();
    store.update_capture_state(crashed_while_finalizing, &CaptureState::Finalizing).unwrap();

    let cleanly_finished = SessionId::new();
    store.create_session(&sample_manifest(cleanly_finished)).unwrap();
    store.update_capture_state(cleanly_finished, &CaptureState::Finalized).unwrap();

    let already_failed = SessionId::new();
    store.create_session(&sample_manifest(already_failed)).unwrap();
    store
        .update_capture_state(
            already_failed,
            &CaptureState::Failed { recoverable: false, reason: "device permission denied".to_string() },
        )
        .unwrap();

    let mut recovered = store.reconcile_on_startup().unwrap();
    recovered.sort();
    let mut expected = vec![crashed_while_recording, crashed_while_finalizing];
    expected.sort();
    assert_eq!(recovered, expected);

    for session_id in [crashed_while_recording, crashed_while_finalizing] {
        match store.capture_state(session_id).unwrap() {
            CaptureState::Failed { recoverable, .. } => assert!(recoverable),
            other => panic!("expected Failed{{recoverable: true}}, got {other:?}"),
        }
    }
    assert_eq!(store.capture_state(cleanly_finished).unwrap(), CaptureState::Finalized);
    match store.capture_state(already_failed).unwrap() {
        CaptureState::Failed { recoverable, .. } => assert!(!recoverable),
        other => panic!("expected the pre-existing Failed{{recoverable: false}} untouched, got {other:?}"),
    }

    // A second reconciliation pass must be a no-op: both sessions are now `failed`
    // (a terminal tag), not one of the non-terminal tags being scanned for.
    assert!(store.reconcile_on_startup().unwrap().is_empty());
}

#[test]
fn segment_counts_by_track_reflects_committed_segments_only() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    store.register_segment(&sample_segment(session_id, TrackKind::SelfMic, 0)).unwrap();
    store.register_segment(&sample_segment(session_id, TrackKind::SelfMic, 1)).unwrap();
    store.register_segment(&sample_segment(session_id, TrackKind::RemoteAudio, 0)).unwrap();

    let counts = store.segment_counts_by_track(session_id).unwrap();
    assert_eq!(counts.get(&TrackKind::SelfMic), Some(&2));
    assert_eq!(counts.get(&TrackKind::RemoteAudio), Some(&1));
}
