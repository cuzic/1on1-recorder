use serde::{Deserialize, Serialize};

/// design.md §9.1. The recorder always keeps these as two independent logical tracks
/// aligned on a shared timeline (see `audio-timeline`), never mixed down to stereo
/// channels internally — mixing to Left=Self/Right=Remote, if ever needed, happens
/// only at final export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TrackKind {
    #[serde(rename = "self")]
    SelfMic,
    #[serde(rename = "remote")]
    RemoteAudio,
}

impl TrackKind {
    /// The string form used in the session manifest's `audio.tracks` array and in
    /// the upload API's `Idempotency-Key: {session_id}:{track}:{sequence}` (design.md
    /// §9.4, §13.2) — `"self"` / `"remote"`, not the Rust variant names.
    pub fn as_manifest_str(&self) -> &'static str {
        match self {
            TrackKind::SelfMic => "self",
            TrackKind::RemoteAudio => "remote",
        }
    }
}

impl std::fmt::Display for TrackKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_manifest_str())
    }
}

/// The error returned when parsing a `TrackKind` from a string other than `"self"` or
/// `"remote"` — e.g. a corrupted DB row or a malformed `Idempotency-Key`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid track kind: {0:?}")]
pub struct ParseTrackKindError(pub String);

impl std::str::FromStr for TrackKind {
    type Err = ParseTrackKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "self" => Ok(TrackKind::SelfMic),
            "remote" => Ok(TrackKind::RemoteAudio),
            other => Err(ParseTrackKindError(other.to_string())),
        }
    }
}
