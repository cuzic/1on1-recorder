//! Ported from spike-08-chunked-upload's `tests/fault_injection.rs`, adapted to the
//! real architecture: segments-to-upload and their status live in
//! `session_store::SessionStore` (not a spike-only `SpoolDb`), and `HttpUploadClient`
//! implements `recorder_domain::UploadAdapter` instead of exposing its own ad hoc
//! methods.

use chrono::Utc;
use recorder_domain::{
    AudioCodec, AudioManifest, AudioSegment, CaptureManifest, ConsentManifest, RemoteSourceKind,
    SessionId, SessionManifest, SessionSummary, TrackKind, UploadAdapter, UploadState,
};
use session_store::SessionStore;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use upload_client::mock_server::{spawn_test_server, FaultConfig};
use upload_client::{HttpUploadClient, StaticTokenProvider};

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn write_segment_file(dir: &Path, sequence: u64, content: &[u8]) -> std::path::PathBuf {
    let path = dir.join(format!("{sequence:06}.opus"));
    std::fs::write(&path, content).unwrap();
    path
}

fn manifest(session_id: SessionId) -> SessionManifest {
    SessionManifest {
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
        audio: AudioManifest { sample_rate: 48_000, segment_duration_ms: 30_000, tracks: vec![TrackKind::SelfMic] },
        consent: ConsentManifest { confirmed_by_user: true, confirmed_at: Utc::now() },
    }
}

fn client(base_url: String) -> HttpUploadClient {
    HttpUploadClient::new(base_url, Duration::from_millis(150), Arc::new(StaticTokenProvider("test-token".to_string())))
}

#[tokio::test]
async fn hundred_segments_with_30_percent_faults_completes_with_no_duplicates_and_no_loss() {
    let fault_config = FaultConfig {
        pre_process_fault_probability: 0.10,
        post_process_fault_probability: 0.10,
        timeout_simulation_probability: 0.10,
        timeout_sleep: Duration::from_millis(300),
    };
    let (base_url, state) = spawn_test_server(fault_config).await;
    let client = Arc::new(client(base_url));

    let store = Arc::new(SessionStore::open_in_memory().unwrap());
    let session_id = SessionId::new();
    store.create_session(&manifest(session_id)).unwrap();

    let remote = client.create_session(&manifest(session_id)).await.expect("create_session failed");
    store.set_remote_session_id(session_id, &remote.remote_session_id).unwrap();

    let files_dir = tempfile::tempdir().unwrap();
    const N: u64 = 100;
    for seq in 0..N {
        let content = format!("segment-data-{seq}").into_bytes();
        let path = write_segment_file(files_dir.path(), seq, &content);
        store
            .register_segment(&AudioSegment {
                session_id,
                track: TrackKind::SelfMic,
                sequence: seq,
                timeline_start_ms: seq * 30_000,
                duration_ms: 30_000,
                codec: AudioCodec::Opus,
                sample_rate: 48_000,
                channels: 1,
                sha256: sha256_hex(&content),
                local_path: path,
                byte_len: content.len() as u64,
            })
            .unwrap();
    }

    let pending = store.pending_uploads(session_id).unwrap();
    assert_eq!(pending.len(), N as usize);

    let mut join_set = tokio::task::JoinSet::new();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(8));
    for segment in pending {
        let client = client.clone();
        let store = store.clone();
        let remote = remote.clone();
        let semaphore = semaphore.clone();
        join_set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.unwrap();
            client.upload_segment(&remote, &segment).await.expect("upload_segment should eventually succeed despite fault injection");
            store.update_upload_state(session_id, segment.track, segment.sequence, &UploadState::Completed).unwrap();
        });
    }
    while let Some(res) = join_set.join_next().await {
        res.expect("upload task panicked");
    }

    assert!(store.pending_uploads(session_id).unwrap().is_empty());

    let summary = SessionSummary {
        session_id,
        ended_at: Utc::now(),
        segment_counts_by_track: store.segment_counts_by_track(session_id).unwrap(),
        total_duration_ms: N * 30_000,
    };
    client.finalize_session(&remote, &summary).await.expect("finalize_session failed");

    let stats = state.stats.lock().unwrap();
    assert_eq!(stats.segment_write_counts.len(), N as usize, "server should register exactly N segments (zero lost)");
    for (key, count) in stats.segment_write_counts.iter() {
        assert_eq!(*count, 1, "each segment's actual write should happen exactly once (zero duplicates): {key:?}");
    }
    assert!(stats.faults_injected > 0, "fault injection should actually have fired, or this test proves nothing");
}

#[tokio::test]
async fn resumes_after_simulated_crash_without_duplicate_registration() {
    let (base_url, state) = spawn_test_server(FaultConfig::default()).await;
    let client = client(base_url);

    let tmp_dir = tempfile::tempdir().unwrap();
    let db_path = tmp_dir.path().join("session.sqlite3");
    let files_dir = tempfile::tempdir().unwrap();

    let session_id = SessionId::new();
    let remote = {
        let store = SessionStore::open(&db_path).unwrap();
        store.create_session(&manifest(session_id)).unwrap();
        let remote = client.create_session(&manifest(session_id)).await.expect("create_session failed");
        store.set_remote_session_id(session_id, &remote.remote_session_id).unwrap();

        // Pre-load 100 segments into the local ledger (design.md §12: the local spool
        // is always the source of truth for what needs to be sent).
        for seq in 0..100u64 {
            let content = format!("segment-{seq}").into_bytes();
            let path = write_segment_file(files_dir.path(), seq, &content);
            store
                .register_segment(&AudioSegment {
                    session_id,
                    track: TrackKind::SelfMic,
                    sequence: seq,
                    timeline_start_ms: seq * 30_000,
                    duration_ms: 30_000,
                    codec: AudioCodec::Opus,
                    sample_rate: 48_000,
                    channels: 1,
                    sha256: sha256_hex(&content),
                    local_path: path,
                    byte_len: content.len() as u64,
                })
                .unwrap();
        }
        remote
    };

    // "Run 1": segments 0..39 are uploaded and marked Completed normally. Segment 39
    // is uploaded successfully (the server has it) but the process is simulated to
    // die *before* the local `update_upload_state` call — segments 40..99 are never
    // attempted at all.
    {
        let store = SessionStore::open(&db_path).unwrap();
        let pending = store.pending_uploads(session_id).unwrap();
        assert_eq!(pending.len(), 100);
        for (i, segment) in pending.iter().enumerate() {
            if i >= 40 {
                break;
            }
            client.upload_segment(&remote, segment).await.expect("upload_segment failed (run1)");
            if i == 39 {
                break; // crash: update_upload_state deliberately not called
            }
            store.update_upload_state(session_id, segment.track, segment.sequence, &UploadState::Completed).unwrap();
        }
    }

    // Post-crash: exactly 39 are marked Completed locally.
    {
        let store = SessionStore::open(&db_path).unwrap();
        let pending = store.pending_uploads(session_id).unwrap();
        assert_eq!(pending.len(), 61, "resume set should be 100-39=61 (including segment 39's resend)");
    }

    // "Run 2": a fresh SessionStore handle on the same file simulates a process
    // restart. Resend everything still pending (segment 39's resend + 40..99's
    // first attempt).
    {
        let store = SessionStore::open(&db_path).unwrap();
        let pending = store.pending_uploads(session_id).unwrap();
        for segment in pending {
            client.upload_segment(&remote, &segment).await.expect("upload_segment failed (run2)");
            store.update_upload_state(session_id, segment.track, segment.sequence, &UploadState::Completed).unwrap();
        }

        let summary = SessionSummary {
            session_id,
            ended_at: Utc::now(),
            segment_counts_by_track: store.segment_counts_by_track(session_id).unwrap(),
            total_duration_ms: 100 * 30_000,
        };
        client.finalize_session(&remote, &summary).await.expect("finalize_session failed");

        assert!(store.pending_uploads(session_id).unwrap().is_empty());
    }

    // Segment 39 was sent by the client twice (run 1 and run 2), but the server's
    // Idempotency-Key-based dedup must mean its actual write only happened once —
    // this is the guarantee a restart-resend must not violate.
    let stats = state.stats.lock().unwrap();
    assert_eq!(stats.segment_write_counts.len(), 100);
    for (key, count) in stats.segment_write_counts.iter() {
        assert_eq!(*count, 1, "duplicate client-side sends must not cause duplicate server-side writes: {key:?}");
    }
}
