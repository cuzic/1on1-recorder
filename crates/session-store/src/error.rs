#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("(de)serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("session {0} not found")]
    SessionNotFound(String),
    #[error("segment ({session_id}, {track}, {sequence}) not found")]
    SegmentNotFound {
        session_id: String,
        track: String,
        sequence: u64,
    },
    #[error("unknown state tag stored in database: {0:?}")]
    UnknownStateTag(String),
    #[error("invalid track kind stored in database: {0}")]
    InvalidTrackKind(#[from] recorder_domain::ParseTrackKindError),
    #[error("invalid audio codec stored in database: {0}")]
    InvalidAudioCodec(#[from] recorder_domain::ParseAudioCodecError),
    #[error("invalid remote source kind stored in database: {0}")]
    InvalidRemoteSourceKind(#[from] recorder_domain::ParseRemoteSourceKindError),
    #[error("invalid timestamp stored in database: {0}")]
    InvalidTimestamp(#[from] chrono::ParseError),
}
