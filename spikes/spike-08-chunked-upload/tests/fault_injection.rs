//! spike-plan.md SPIKE-08 検証手順3・4、合否基準の自動化。

use spike_08_chunked_upload::{spawn_test_server, FaultConfig, SpoolDb, UploadClient};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn hundred_segments_with_30_percent_faults_completes_with_no_duplicates_and_no_loss() {
    // 合計故障率およそ30%(pre 10% + post 10% + timeout 10%)。
    let fault_config = FaultConfig {
        pre_process_fault_probability: 0.10,
        post_process_fault_probability: 0.10,
        timeout_simulation_probability: 0.10,
        timeout_sleep: Duration::from_millis(300),
    };
    let (base_url, state) = spawn_test_server(fault_config, 42).await;
    let client = Arc::new(UploadClient::new(base_url, Duration::from_millis(150)));

    let session_id = "session-100seg";
    client
        .create_session(&serde_json::json!({ "session_id": session_id }))
        .await
        .expect("create_session failed");

    const N: u64 = 100;
    let mut join_set = tokio::task::JoinSet::new();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(8));
    for seq in 0..N {
        let client = client.clone();
        let semaphore = semaphore.clone();
        join_set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.unwrap();
            let data = format!("segment-data-{seq}").into_bytes();
            client.upload_segment(session_id, "self", seq, &data).await
        });
    }
    while let Some(res) = join_set.join_next().await {
        res.expect("upload task panicked")
            .expect("upload_segment should eventually succeed despite fault injection");
    }

    client
        .finalize_session(session_id, N)
        .await
        .expect("finalize_session failed");

    let stats = state.stats.lock().unwrap();
    assert_eq!(
        stats.segment_write_counts.len(),
        N as usize,
        "サーバ側の登録がちょうどN件であること(欠落0件)"
    );
    for (key, count) in stats.segment_write_counts.iter() {
        assert_eq!(
            *count, 1,
            "各セグメントの実書き込みは1回だけであること(重複登録0件): {key:?}"
        );
    }
    assert!(
        stats.faults_injected > 0,
        "故障注入が実際に発生していること(このアサーションが常にtrueだと \
         テストが何も検証していないことになる)"
    );
}

#[tokio::test]
async fn resumes_after_simulated_crash_without_duplicate_registration() {
    let fault_config = FaultConfig::default();
    let (base_url, state) = spawn_test_server(fault_config, 99).await;
    let client = Arc::new(UploadClient::new(base_url, Duration::from_millis(200)));

    let session_id = "session-crash-test";
    client
        .create_session(&serde_json::json!({ "session_id": session_id }))
        .await
        .expect("create_session failed");

    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = tmp_dir.path().join("spool.sqlite3");

    // 事前に100segment分をローカルスプール(SQLite)へ積んでおく
    // (design.md §12: 録音の正本は常にローカルスプールに置く、の簡略版)。
    {
        let db = SpoolDb::open(&db_path).expect("failed to open spool db");
        for seq in 0..100u64 {
            let data = format!("segment-{seq}").into_bytes();
            db.insert_pending_segment(session_id, "self", seq, &data)
                .expect("insert_pending_segment failed");
        }
    }

    // "Run 1": 39個目まではネットワーク送信・DBマーク付けまで正常に完了させる。
    // 40個目(index=39)は**サーバへの送信自体は成功させるが、
    // ローカルDBへuploaded=1を書き込む前にプロセスが死んだ**状況を模し、
    // そこで処理を打ち切る(41個目以降は一度も試行しない)。
    {
        let db = SpoolDb::open(&db_path).expect("failed to open spool db (run1)");
        let pending = db.pending_segments(session_id).expect("pending_segments failed");
        assert_eq!(pending.len(), 100);
        for (i, (track, seq, data)) in pending.iter().enumerate() {
            if i >= 40 {
                break; // クラッシュ: これ以降は一切試行されない
            }
            client
                .upload_segment(session_id, track, *seq, data)
                .await
                .expect("upload_segment failed (run1)");
            if i == 39 {
                // サーバには届いたが、ローカルDBへのmark_uploadedを行う前に
                // プロセスが死んだことを表現するため、あえて呼ばない。
                break;
            }
            db.mark_uploaded(session_id, track, *seq)
                .expect("mark_uploaded failed (run1)");
        }
    }

    // "クラッシュ"直後のローカルDB状態を確認する。
    {
        let db = SpoolDb::open(&db_path).expect("failed to open spool db (post-crash check)");
        assert_eq!(
            db.uploaded_segment_count(session_id).unwrap(),
            39,
            "39個目(index)はサーバ送信成功済みだがローカルではuploaded=0のまま"
        );
    }

    // "Run 2": プロセス再起動を模す。同じファイルへ新しいSpoolDbハンドルで
    // 接続し直し、uploaded=0の行(39個目の再送 + 40〜99個目の初回送信)を
    // まとめて再開する。
    {
        let db = SpoolDb::open(&db_path).expect("failed to open spool db (run2)");
        let pending = db.pending_segments(session_id).expect("pending_segments failed (run2)");
        assert_eq!(
            pending.len(),
            61,
            "再開対象は100-39=61件(39個目の再送を含む)であること"
        );
        for (track, seq, data) in pending {
            client
                .upload_segment(session_id, &track, seq, &data)
                .await
                .expect("upload_segment failed (run2)");
            db.mark_uploaded(session_id, &track, seq)
                .expect("mark_uploaded failed (run2)");
        }
    }

    client
        .finalize_session(session_id, 100)
        .await
        .expect("finalize_session failed");

    // 39個目はクライアント視点で2回(run1, run2)送信されているが、
    // サーバ側の実書き込みはIdempotency-Keyにより1回だけであること
    // (これが「再起動後の再開で二重送信が発生してもAPI上は冪等に処理される」
    // という合否基準そのもの)。
    let stats = state.stats.lock().unwrap();
    assert_eq!(stats.segment_write_counts.len(), 100);
    for (key, count) in stats.segment_write_counts.iter() {
        assert_eq!(*count, 1, "二重送信があってもサーバ側の登録は1回のみ: {key:?}");
    }

    let db = SpoolDb::open(&db_path).expect("failed to open spool db (final check)");
    assert_eq!(db.uploaded_segment_count(session_id).unwrap(), 100);
    assert_eq!(db.total_segment_count(session_id).unwrap(), 100);
}
