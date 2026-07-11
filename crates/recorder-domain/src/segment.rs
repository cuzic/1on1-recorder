use std::path::PathBuf;

use crate::session::SessionId;
use crate::track::TrackKind;

/// design.md doesn't enumerate codecs explicitly, but §21's Phase 1A scope commits to
/// 30-second Opus segments. A single-variant enum (instead of a bare string) keeps
/// `AudioSegment::codec` from silently accepting a typo'd codec name, and gives a
/// place to add codecs later without changing the field's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    Opus,
}

/// design.md §9.3. One committed, immutable segment file on disk, already fsynced and
/// hashed by `segment-store`'s atomic-commit path. `sha256` and `byte_len` describe the
/// committed file itself, so `upload-client` can verify integrity without re-reading
/// `local_path` through `segment-store`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AudioSegment {
    pub session_id: SessionId,
    pub track: TrackKind,
    pub sequence: u64,
    pub timeline_start_ms: u64,
    pub duration_ms: u32,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    pub sha256: String,
    pub local_path: PathBuf,
    pub byte_len: u64,
}
