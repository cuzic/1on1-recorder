//! design.md §12.2: 再起動時のセグメントディレクトリスキャン。
//! - `.partial`ファイル: 孤立(コミット未完了)なので破棄する。
//! - DB未登録の`.opus`ファイル: renameは完了したがDB登録前にクラッシュしたケース。
//!   再ハッシュしてDBに登録する。
//! - DB登録済みの`.opus`ファイル: 何もしない。

use crate::db::SegmentDb;
use crate::hash::sha256_hex;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredKind {
    OrphanedPartialDiscarded,
    UnregisteredCompleteRegistered,
    AlreadyRegistered,
}

pub fn scan_and_recover(
    dir: &Path,
    session_id: &str,
    db: &SegmentDb,
) -> anyhow::Result<Vec<(u64, RecoveredKind)>> {
    let mut results = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if let Some(seq_str) = file_name.strip_suffix(".partial") {
            if let Ok(seq) = seq_str.parse::<u64>() {
                std::fs::remove_file(&path)?;
                results.push((seq, RecoveredKind::OrphanedPartialDiscarded));
            }
        } else if let Some(seq_str) = file_name.strip_suffix(".opus") {
            if let Ok(seq) = seq_str.parse::<u64>() {
                if db.is_registered(session_id, seq)? {
                    results.push((seq, RecoveredKind::AlreadyRegistered));
                } else {
                    let bytes = std::fs::read(&path)?;
                    let sha256 = sha256_hex(&bytes);
                    let size = bytes.len();
                    db.register_ready(session_id, seq, &path, &sha256, size)?;
                    results.push((seq, RecoveredKind::UnregisteredCompleteRegistered));
                }
            }
        }
    }

    results.sort_by_key(|(seq, _)| *seq);
    Ok(results)
}
