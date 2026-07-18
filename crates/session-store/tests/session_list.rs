use chrono::{DateTime, Duration, Utc};
use recorder_domain::{
    AudioManifest, CaptureManifest, CaptureState, ConsentManifest, RemoteSourceKind, SessionId, SessionManifest,
    TrackKind,
};
use session_store::SessionStore;

fn sample_manifest(session_id: SessionId, started_at: DateTime<Utc>) -> SessionManifest {
    SessionManifest {
        schema_version: 1,
        session_id,
        started_at,
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
            confirmed_at: started_at,
        },
    }
}

#[test]
fn list_sessions_is_empty_when_no_sessions_exist() {
    let store = SessionStore::open_in_memory().unwrap();
    assert_eq!(store.list_sessions().unwrap(), vec![]);
}

#[test]
fn list_sessions_returns_newest_started_at_first() {
    let store = SessionStore::open_in_memory().unwrap();
    let now = Utc::now();

    let oldest = SessionId::new();
    store.create_session(&sample_manifest(oldest, now - Duration::hours(2))).unwrap();
    let newest = SessionId::new();
    store.create_session(&sample_manifest(newest, now)).unwrap();
    let middle = SessionId::new();
    store.create_session(&sample_manifest(middle, now - Duration::hours(1))).unwrap();

    let items = store.list_sessions().unwrap();
    let ids: Vec<SessionId> = items.iter().map(|i| i.session_id).collect();
    assert_eq!(ids, vec![newest, middle, oldest]);
}

#[test]
fn list_sessions_reports_started_and_ended_at_and_capture_state() {
    let store = SessionStore::open_in_memory().unwrap();
    let now = Utc::now();

    let in_progress = SessionId::new();
    store.create_session(&sample_manifest(in_progress, now)).unwrap();
    store.update_capture_state(in_progress, &CaptureState::Recording).unwrap();

    let finished = SessionId::new();
    store.create_session(&sample_manifest(finished, now - Duration::hours(1))).unwrap();
    store.update_capture_state(finished, &CaptureState::Finalized).unwrap();

    let items = store.list_sessions().unwrap();
    assert_eq!(items.len(), 2);

    let in_progress_item = items.iter().find(|i| i.session_id == in_progress).unwrap();
    assert_eq!(in_progress_item.capture_state, CaptureState::Recording);
    // Recording never transitioned to `finalized`, so `ended_at` stays unset —
    // see `update_capture_state`'s `CASE WHEN :tag = 'finalized' ...` clause.
    assert_eq!(in_progress_item.ended_at, None);

    let finished_item = items.iter().find(|i| i.session_id == finished).unwrap();
    assert_eq!(finished_item.capture_state, CaptureState::Finalized);
    assert!(finished_item.ended_at.is_some(), "a finalized session should have ended_at set");
}

#[test]
fn list_sessions_marks_crashed_sessions_as_failed_recoverable_after_reconcile() {
    let store = SessionStore::open_in_memory().unwrap();
    let crashed = SessionId::new();
    store.create_session(&sample_manifest(crashed, Utc::now())).unwrap();
    store.update_capture_state(crashed, &CaptureState::Recording).unwrap();

    store.reconcile_on_startup().unwrap();

    let items = store.list_sessions().unwrap();
    let item = items.iter().find(|i| i.session_id == crashed).unwrap();
    match &item.capture_state {
        CaptureState::Failed { recoverable, .. } => assert!(*recoverable, "reconciled session should be marked recoverable"),
        other => panic!("expected Failed{{recoverable: true}}, got {other:?}"),
    }
}
