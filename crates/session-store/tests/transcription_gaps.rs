use chrono::Utc;
use recorder_domain::{
    AudioManifest, CaptureManifest, ConsentManifest, RemoteSourceKind, SessionId, SessionManifest, TrackKind,
};
use session_store::{SessionStore, TranscriptionGap};

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

#[test]
fn gaps_for_session_is_empty_for_a_session_with_none() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    assert!(store.gaps_for_session(session_id).unwrap().is_empty());
}

#[test]
fn record_gap_start_leaves_end_ms_none_until_record_gap_end_is_called() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    let gap_id = store.record_gap_start(session_id, TrackKind::RemoteAudio, 10_000).unwrap();

    let gaps = store.gaps_for_session(session_id).unwrap();
    assert_eq!(
        gaps,
        vec![TranscriptionGap { id: gap_id, session_id, track: TrackKind::RemoteAudio, start_ms: 10_000, end_ms: None }]
    );

    store.record_gap_end(gap_id, 15_000).unwrap();

    let gaps = store.gaps_for_session(session_id).unwrap();
    assert_eq!(
        gaps,
        vec![TranscriptionGap { id: gap_id, session_id, track: TrackKind::RemoteAudio, start_ms: 10_000, end_ms: Some(15_000) }]
    );
}

#[test]
fn gaps_for_session_returns_every_track_oldest_first() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    let self_gap = store.record_gap_start(session_id, TrackKind::SelfMic, 1_000).unwrap();
    let remote_gap = store.record_gap_start(session_id, TrackKind::RemoteAudio, 2_000).unwrap();
    store.record_gap_end(self_gap, 1_500).unwrap();
    store.record_gap_end(remote_gap, 4_000).unwrap();

    let gaps = store.gaps_for_session(session_id).unwrap();
    assert_eq!(gaps.len(), 2);
    assert_eq!(gaps[0].id, self_gap);
    assert_eq!(gaps[0].track, TrackKind::SelfMic);
    assert_eq!(gaps[1].id, remote_gap);
    assert_eq!(gaps[1].track, TrackKind::RemoteAudio);
}

#[test]
fn discard_gap_removes_it_without_ever_recording_an_end() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    let short_gap = store.record_gap_start(session_id, TrackKind::RemoteAudio, 1_000).unwrap();
    let real_gap = store.record_gap_start(session_id, TrackKind::RemoteAudio, 5_000).unwrap();
    store.record_gap_end(real_gap, 20_000).unwrap();

    store.discard_gap(short_gap).unwrap();

    let gaps = store.gaps_for_session(session_id).unwrap();
    assert_eq!(gaps, vec![TranscriptionGap { id: real_gap, session_id, track: TrackKind::RemoteAudio, start_ms: 5_000, end_ms: Some(20_000) }]);
}

#[test]
fn record_gap_end_on_an_unknown_id_is_a_harmless_no_op() {
    let store = SessionStore::open_in_memory().unwrap();
    // No session created at all — `gap_id` 999 can't possibly exist.
    assert!(store.record_gap_end(999, 1_000).is_ok());
}

#[test]
fn record_gap_start_without_a_session_is_rejected_by_the_foreign_key() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();

    let result = store.record_gap_start(session_id, TrackKind::SelfMic, 0);
    assert!(result.is_err(), "expected foreign key violation, got {result:?}");
}
