//! Restart-time scan of one track's segment directory:
//! - `.partial` files are orphaned (commit never reached rename) and discarded.
//! - `.opus` files not yet registered had their rename complete but crashed before
//!   `session-store` registration — re-hash, re-derive duration, and register them.
//! - Already-registered `.opus` files are left alone.

use std::path::Path;

use recorder_domain::{AudioCodec, AudioSegment, SessionId, TrackKind};
use session_store::SessionStore;

use crate::error::SegmentStoreError;
use crate::granule;
use crate::hash::sha256_hex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredKind {
    OrphanedPartialDiscarded,
    UnregisteredCompleteRegistered,
    AlreadyRegistered,
}

/// `nominal_segment_duration_ms` reconstructs `timeline_start_ms` as
/// `sequence * nominal_segment_duration_ms` for any segment recovered here. This is
/// exact for Phase 1A's fixed-cadence, gap-free segmenter, but would be wrong if a
/// future segmenter ever produced variable-length segments or left gaps — the actual
/// audio's own duration is still derived exactly (see `granule::read_total_samples`),
/// only `timeline_start_ms` is an assumption.
pub fn scan_and_recover(
    session_dir: &Path,
    session_id: SessionId,
    track: TrackKind,
    nominal_segment_duration_ms: u64,
    sample_rate: u32,
    channels: u16,
    store: &SessionStore,
) -> Result<Vec<(u64, RecoveredKind)>, SegmentStoreError> {
    let dir = session_dir.join(track.as_manifest_str());
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();

    for entry in std::fs::read_dir(&dir)? {
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
                if store.segment_exists(session_id, track, seq)? {
                    results.push((seq, RecoveredKind::AlreadyRegistered));
                } else {
                    let bytes = std::fs::read(&path)?;
                    let sha256 = sha256_hex(&bytes);
                    let total_samples = granule::read_total_samples(&path)?;
                    let segment = AudioSegment {
                        session_id,
                        track,
                        sequence: seq,
                        timeline_start_ms: seq * nominal_segment_duration_ms,
                        duration_ms: granule::samples_to_ms(total_samples),
                        codec: AudioCodec::Opus,
                        sample_rate,
                        channels,
                        sha256,
                        local_path: path.clone(),
                        byte_len: bytes.len() as u64,
                    };
                    store.register_segment(&segment)?;
                    results.push((seq, RecoveredKind::UnregisteredCompleteRegistered));
                }
            }
        }
    }

    results.sort_by_key(|(seq, _)| *seq);
    Ok(results)
}
