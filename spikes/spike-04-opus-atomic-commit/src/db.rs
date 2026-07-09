//! design.md §12.2 手順6: セグメント確定後のSQLite登録先。
//! 「readyになったセグメント」の台帳。recovery.rsの再起動スキャンもここを参照する。

use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

pub struct SegmentDb {
    conn: Connection,
}

impl SegmentDb {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS segments (
                session_id TEXT NOT NULL,
                sequence   INTEGER NOT NULL,
                path       TEXT NOT NULL,
                sha256     TEXT NOT NULL,
                size       INTEGER NOT NULL,
                status     TEXT NOT NULL DEFAULT 'ready',
                PRIMARY KEY (session_id, sequence)
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn register_ready(
        &self,
        session_id: &str,
        sequence: u64,
        path: &Path,
        sha256: &str,
        size: usize,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO segments (session_id, sequence, path, sha256, size, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'ready')",
            params![
                session_id,
                sequence as i64,
                path.to_string_lossy(),
                sha256,
                size as i64
            ],
        )?;
        Ok(())
    }

    pub fn is_registered(&self, session_id: &str, sequence: u64) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE session_id = ?1 AND sequence = ?2",
            params![session_id, sequence as i64],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn registered_sequences(&self, session_id: &str) -> Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT sequence FROM segments WHERE session_id = ?1 ORDER BY sequence")?;
        let rows = stmt.query_map(params![session_id], |row| row.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r? as u64);
        }
        Ok(out)
    }

    pub fn path_of(&self, session_id: &str, sequence: u64) -> Result<Option<PathBuf>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM segments WHERE session_id = ?1 AND sequence = ?2")?;
        let mut rows = stmt.query(params![session_id, sequence as i64])?;
        if let Some(row) = rows.next()? {
            let p: String = row.get(0)?;
            Ok(Some(PathBuf::from(p)))
        } else {
            Ok(None)
        }
    }
}
