use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A session identifier. design.md §9.4's example (`"01J..."`) is a ULID: sortable by
/// creation time, no coordination needed to generate one, and URL-safe as a string —
/// a good fit for a session ID that's also used verbatim inside the upload API's
/// `Idempotency-Key` (§13.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub ulid::Ulid);

impl SessionId {
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for SessionId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(ulid::Ulid::from_string(s)?))
    }
}

/// How the remote (system-audio) track was captured. design.md §9.4's example manifest
/// shows `"application_process"`, implying `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`
/// (Phase 1B, not yet implemented — see `capture-windows`'s README); Phase 1A's
/// `capture-windows` only implements endpoint loopback (the whole system's playback),
/// which is a materially different guarantee (other applications' audio can leak in).
/// Recorded here explicitly so the manifest — and any later analysis of a session —
/// always states which one actually happened, rather than assuming process-level
/// isolation that Phase 1A doesn't provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSourceKind {
    EndpointLoopback,
    ApplicationProcess,
}

impl RemoteSourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteSourceKind::EndpointLoopback => "endpoint_loopback",
            RemoteSourceKind::ApplicationProcess => "application_process",
        }
    }
}

impl std::fmt::Display for RemoteSourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when parsing a `RemoteSourceKind` from a string other than a
/// known kind — e.g. a corrupted DB row.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid remote source kind: {0:?}")]
pub struct ParseRemoteSourceKindError(pub String);

impl std::str::FromStr for RemoteSourceKind {
    type Err = ParseRemoteSourceKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "endpoint_loopback" => Ok(RemoteSourceKind::EndpointLoopback),
            "application_process" => Ok(RemoteSourceKind::ApplicationProcess),
            other => Err(ParseRemoteSourceKindError(other.to_string())),
        }
    }
}

/// design.md §9.4, the `capture` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureManifest {
    pub microphone_device_id: String,
    pub remote_source_id: String,
    pub remote_source_kind: RemoteSourceKind,
}

/// design.md §9.4, the `audio` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioManifest {
    pub sample_rate: u32,
    pub segment_duration_ms: u32,
    pub tracks: Vec<crate::track::TrackKind>,
}

/// design.md §9.4, the `consent` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentManifest {
    pub confirmed_by_user: bool,
    pub confirmed_at: DateTime<Utc>,
}

/// design.md §9.4. Sent to `UploadAdapter::create_session` (§13) to register a new
/// recording session with the API before any segments are uploaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub platform: String,
    pub app_version: String,
    pub capture: CaptureManifest,
    pub audio: AudioManifest,
    pub consent: ConsentManifest,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_roundtrips_through_display_and_from_str() {
        let id = SessionId::new();
        let text = id.to_string();
        let parsed: SessionId = text.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn manifest_serializes_to_the_shape_design_md_shows() {
        let manifest = SessionManifest {
            schema_version: 1,
            session_id: SessionId::new(),
            started_at: DateTime::from_timestamp(1_751_846_400, 0).unwrap(),
            ended_at: None,
            platform: "windows".to_string(),
            app_version: "0.1.0".to_string(),
            capture: CaptureManifest {
                microphone_device_id: "mic-1".to_string(),
                remote_source_id: "speaker-1".to_string(),
                remote_source_kind: RemoteSourceKind::EndpointLoopback,
            },
            audio: AudioManifest {
                sample_rate: 48_000,
                segment_duration_ms: 30_000,
                tracks: vec![crate::track::TrackKind::SelfMic, crate::track::TrackKind::RemoteAudio],
            },
            consent: ConsentManifest {
                confirmed_by_user: true,
                confirmed_at: DateTime::from_timestamp(1_751_846_400, 0).unwrap(),
            },
        };

        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["platform"], "windows");
        assert_eq!(json["audio"]["tracks"][0], "self");
        assert_eq!(json["audio"]["tracks"][1], "remote");
        assert_eq!(json["ended_at"], serde_json::Value::Null);
    }
}
