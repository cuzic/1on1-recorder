//! Atomic segment commit: (1) write `.partial` -> (2) flush -> (3) fsync ->
//! (4) SHA-256 -> (5) atomic rename -> (6) register into `session-store`.
//!
//! `CrashPoint` is a test hook that simulates a process crash immediately after a
//! given step, by returning `Ok(None)` instead of continuing — the caller is left
//! with whatever `.partial`/unregistered `.opus` file that step leaves behind, for
//! `recovery::scan_and_recover` to clean up or finish registering.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use recorder_domain::{AudioCodec, AudioSegment, SessionId, TrackKind};
use session_store::SessionStore;

use crate::error::SegmentStoreError;
use crate::granule;
use crate::hash::sha256_hex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// No crash — commit runs to completion.
    None,
    /// Right after the `.partial` write + flush (before fsync).
    AfterPartialWrite,
    /// Right after fsync (before rename).
    AfterFsync,
    /// Right after rename (before registering with `session-store`).
    AfterRename,
}

/// Everything about a segment that isn't derivable from the encoded audio bytes
/// themselves (`duration_ms` is: see `granule::read_total_samples`).
#[derive(Debug, Clone, Copy)]
pub struct SegmentRequest {
    pub session_id: SessionId,
    pub track: TrackKind,
    pub sequence: u64,
    pub timeline_start_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
}

fn track_dir(session_dir: &Path, track: TrackKind) -> PathBuf {
    session_dir.join(track.as_manifest_str())
}

fn partial_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("{sequence:06}.partial"))
}

fn final_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("{sequence:06}.opus"))
}

/// Commits an already-encoded Ogg Opus byte string under `session_dir/{track}/`, then
/// registers it with `store`. Returns `Ok(None)` if `crash_point` cut the commit
/// short (see `CrashPoint`).
pub fn commit_segment(
    encoded: &[u8],
    session_dir: &Path,
    request: &SegmentRequest,
    store: &SessionStore,
    crash_point: CrashPoint,
) -> Result<Option<AudioSegment>, SegmentStoreError> {
    let dir = track_dir(session_dir, request.track);
    std::fs::create_dir_all(&dir)?;
    let partial = partial_path(&dir, request.sequence);
    let final_p = final_path(&dir, request.sequence);

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

    let total_samples = granule::read_total_samples(&final_p)?;
    let segment = AudioSegment {
        session_id: request.session_id,
        track: request.track,
        sequence: request.sequence,
        timeline_start_ms: request.timeline_start_ms,
        duration_ms: granule::samples_to_ms(total_samples),
        codec: AudioCodec::Opus,
        sample_rate: request.sample_rate,
        channels: request.channels,
        sha256,
        local_path: final_p,
        byte_len: encoded.len() as u64,
    };
    store.register_segment(&segment)?;

    Ok(Some(segment))
}
