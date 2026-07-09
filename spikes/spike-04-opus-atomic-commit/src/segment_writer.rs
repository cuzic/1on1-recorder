//! design.md §12.2: セグメントのアトミックコミット手順。
//! (1) .partial書き込み -> (2) flush -> (3) fsync -> (4) SHA-256 ->
//! (5) atomic rename -> (6) SQLite登録。
//!
//! `CrashPoint`は各手順の直後にプロセスがクラッシュした場合を模擬するための
//! テストフックで、指定した地点まで処理した後に`Ok(None)`を返して打ち切る
//! (実プロセスkillの代わりに、spike-08で確立した「途中で止める」パターンを踏襲)。

use crate::db::SegmentDb;
use crate::hash::sha256_hex;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// クラッシュなし。最後まで正常にコミットする。
    None,
    /// .partial書き込み+flushの直後(fsync前)にクラッシュ。
    AfterPartialWrite,
    /// fsyncの直後(rename前)にクラッシュ。
    AfterFsync,
    /// renameの直後(DB登録前)にクラッシュ。
    AfterRename,
}

#[derive(Debug, Clone)]
pub struct CommittedSegment {
    pub sequence: u64,
    pub path: PathBuf,
    pub sha256: String,
    pub size: usize,
}

fn partial_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("{sequence:06}.partial"))
}

fn final_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("{sequence:06}.opus"))
}

/// エンコード済みOgg Opusバイト列を、design.md §12.2の手順でアトミックにコミットする。
/// `crash_point`で指定した地点で打ち切った場合は`Ok(None)`を返す
/// (呼び出し側はディレクトリに`.partial`や未登録の`.opus`が残った状態になる)。
pub fn commit_segment(
    encoded: &[u8],
    dir: &Path,
    session_id: &str,
    sequence: u64,
    db: &SegmentDb,
    crash_point: CrashPoint,
) -> anyhow::Result<Option<CommittedSegment>> {
    let partial = partial_path(dir, sequence);
    let final_p = final_path(dir, sequence);

    {
        let mut f = File::create(&partial)?;
        f.write_all(encoded)?;
        f.flush()?;

        if crash_point == CrashPoint::AfterPartialWrite {
            return Ok(None);
        }

        f.sync_all()?;
    }

    if crash_point == CrashPoint::AfterFsync {
        return Ok(None);
    }

    let sha256 = sha256_hex(encoded);
    std::fs::rename(&partial, &final_p)?;

    if crash_point == CrashPoint::AfterRename {
        return Ok(None);
    }

    db.register_ready(session_id, sequence, &final_p, &sha256, encoded.len())?;

    Ok(Some(CommittedSegment {
        sequence,
        path: final_p,
        sha256,
        size: encoded.len(),
    }))
}
