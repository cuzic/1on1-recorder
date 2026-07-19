use chrono::{Duration, Utc};
use recorder_domain::{
    AudioManifest, CaptureManifest, ConsentManifest, RemoteSourceKind, SessionId, SessionManifest, TrackKind,
};
use session_store::{Summary, TranscriptSegment};
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

#[test]
fn transcript_segments_round_trip_in_insertion_order() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    let interim = TranscriptSegment {
        session_id,
        track: Some(TrackKind::RemoteAudio),
        speaker: Some(1),
        text: "hello the".to_string(),
        start_ms: Some(0),
        end_ms: Some(500),
        is_final: false,
        is_retranscribed: false,
    };
    let finalized = TranscriptSegment {
        session_id,
        track: Some(TrackKind::RemoteAudio),
        speaker: Some(1),
        text: "hello there".to_string(),
        start_ms: Some(0),
        end_ms: Some(900),
        is_final: true,
        is_retranscribed: false,
    };
    // No diarization/track info at all, e.g. a provider without diarization support.
    let untracked = TranscriptSegment {
        session_id,
        track: None,
        speaker: None,
        text: "general kenobi".to_string(),
        start_ms: None,
        end_ms: None,
        is_final: true,
        is_retranscribed: false,
    };

    store.insert_transcript_segment(&interim).unwrap();
    store.insert_transcript_segment(&finalized).unwrap();
    store.insert_transcript_segment(&untracked).unwrap();

    let segments = store.list_transcript_segments(session_id).unwrap();
    assert_eq!(segments, vec![interim, finalized, untracked]);
}

/// Task #91: a row from the manual gap re-transcription pass must round-trip
/// `is_retranscribed: true` distinctly from ordinary live rows (`DEFAULT 0` in
/// the schema covers the column's *default*, not this crate's own read path).
#[test]
fn transcript_segments_round_trip_the_is_retranscribed_marker() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    let live = TranscriptSegment {
        session_id,
        track: Some(TrackKind::SelfMic),
        speaker: None,
        text: "live utterance".to_string(),
        start_ms: Some(0),
        end_ms: Some(500),
        is_final: true,
        is_retranscribed: false,
    };
    let retranscribed = TranscriptSegment {
        session_id,
        track: Some(TrackKind::RemoteAudio),
        speaker: Some(0),
        text: "recovered utterance".to_string(),
        start_ms: Some(500),
        end_ms: Some(1_200),
        is_final: true,
        is_retranscribed: true,
    };

    store.insert_transcript_segment(&live).unwrap();
    store.insert_transcript_segment(&retranscribed).unwrap();

    let segments = store.list_transcript_segments(session_id).unwrap();
    assert_eq!(segments, vec![live, retranscribed]);
}

#[test]
fn list_transcript_segments_is_empty_for_a_session_with_none() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    assert!(store.list_transcript_segments(session_id).unwrap().is_empty());
}

#[test]
fn insert_transcript_segment_without_a_session_is_rejected_by_the_foreign_key() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    let segment = TranscriptSegment {
        session_id,
        track: None,
        speaker: None,
        text: "orphaned".to_string(),
        start_ms: None,
        end_ms: None,
        is_final: true,
        is_retranscribed: false,
    };

    let result = store.insert_transcript_segment(&segment);
    assert!(result.is_err(), "expected foreign key violation, got {result:?}");
}

#[test]
fn get_latest_summary_is_none_until_one_is_inserted() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    assert_eq!(store.get_latest_summary(session_id).unwrap(), None);
}

#[test]
fn get_latest_summary_returns_the_most_recently_generated_one() {
    let store = SessionStore::open_in_memory().unwrap();
    let session_id = SessionId::new();
    store.create_session(&sample_manifest(session_id)).unwrap();

    let now = Utc::now();
    let older = Summary {
        session_id,
        text: "first pass summary".to_string(),
        provider_model: "openai/gpt-4o-mini".to_string(),
        generated_at: now - Duration::hours(1),
    };
    let newer = Summary {
        session_id,
        text: "re-summarized with a better model".to_string(),
        provider_model: "anthropic/claude-sonnet-5".to_string(),
        generated_at: now,
    };

    store.insert_summary(&older).unwrap();
    store.insert_summary(&newer).unwrap();

    assert_eq!(store.get_latest_summary(session_id).unwrap(), Some(newer));
}
