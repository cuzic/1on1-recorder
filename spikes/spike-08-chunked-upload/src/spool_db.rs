//! spike-plan.md SPIKE-08 検証手順4: 「アップロード途中でクライアントをkill
//! → 再起動 → SQLiteの状態から再開できることを確認する」を裏付ける、実際の
//! SQLite(rusqlite, bundled)を使ったローカルスプール。
//!
//! design.md §12を単純化したもの: セグメントの「アップロード済みフラグ」だけを
//! 永続化する。プロセスが途中で死んでも、次回起動時にuploaded=0の行だけを
//! 再送すればよい。Idempotency-Keyがsession_id/track/sequenceから決定的に
//! 導出されるため(client.rs参照)、「実は既にサーバへ届いていた」行を
//! 再送してもサーバ側で重複登録されない。

use crate::error::UploadError;
use rusqlite::{params, Connection};
use std::path::Path;

pub struct SpoolDb {
    conn: Connection,
}

impl SpoolDb {
    pub fn open(path: &Path) -> Result<Self, UploadError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS segments (
                session_id TEXT NOT NULL,
                track TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                data BLOB NOT NULL,
                uploaded INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (session_id, track, sequence)
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn insert_pending_segment(
        &self,
        session_id: &str,
        track: &str,
        sequence: u64,
        data: &[u8],
    ) -> Result<(), UploadError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO segments (session_id, track, sequence, data, uploaded)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![session_id, track, sequence as i64, data],
        )?;
        Ok(())
    }

    pub fn mark_uploaded(
        &self,
        session_id: &str,
        track: &str,
        sequence: u64,
    ) -> Result<(), UploadError> {
        self.conn.execute(
            "UPDATE segments SET uploaded = 1 WHERE session_id = ?1 AND track = ?2 AND sequence = ?3",
            params![session_id, track, sequence as i64],
        )?;
        Ok(())
    }

    /// まだuploaded=0の行を(track, sequence)昇順で返す。
    /// プロセス再起動後の「再開」はこの結果を再送するだけでよい。
    pub fn pending_segments(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, u64, Vec<u8>)>, UploadError> {
        let mut stmt = self.conn.prepare(
            "SELECT track, sequence, data FROM segments
             WHERE session_id = ?1 AND uploaded = 0
             ORDER BY track, sequence",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn total_segment_count(&self, session_id: &str) -> Result<u64, UploadError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn uploaded_segment_count(&self, session_id: &str) -> Result<u64, UploadError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE session_id = ?1 AND uploaded = 1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }
}
