use rusqlite::Connection;

use crate::error::StoreError;

/// One statement batch, run inside a single transaction on `open`. Every `CREATE TABLE`
/// is `IF NOT EXISTS` so re-opening an existing store file is a no-op — there is no
/// migration machinery yet since the schema has never shipped.
///
/// Table layout follows Codex's review of the original task list (sessions / tracks /
/// segments / upload_status / events), consolidating what spike-04's `SegmentDb`
/// (committed-segment ledger) and spike-08's `SpoolDb` (upload/spool state) each did on
/// their own into one schema so the two never diverge:
/// - `segments` takes `SegmentDb`'s shape (a reference to an already-committed,
///   fsynced, hashed file — no raw bytes stored here) but adopts `SpoolDb`'s
///   `(session_id, track, sequence)` key, since Self/Remote must be distinguishable.
/// - `upload_status` takes over `SpoolDb`'s `uploaded` flag, generalized into the full
///   `UploadState` enum plus retry bookkeeping (`attempt_count`, `last_attempt_at`)
///   that `upload-client`'s backoff logic will need.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    platform TEXT NOT NULL,
    app_version TEXT NOT NULL,
    microphone_device_id TEXT NOT NULL,
    remote_source_id TEXT NOT NULL,
    remote_source_kind TEXT NOT NULL,
    sample_rate INTEGER NOT NULL,
    segment_duration_ms INTEGER NOT NULL,
    consent_confirmed_by_user INTEGER NOT NULL,
    consent_confirmed_at TEXT NOT NULL,
    remote_session_id TEXT,
    capture_state_tag TEXT NOT NULL,
    capture_state_recoverable INTEGER,
    capture_state_reason TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tracks (
    session_id TEXT NOT NULL REFERENCES sessions(session_id),
    track TEXT NOT NULL,
    PRIMARY KEY (session_id, track)
);

CREATE TABLE IF NOT EXISTS segments (
    session_id TEXT NOT NULL,
    track TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    timeline_start_ms INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    codec TEXT NOT NULL,
    sample_rate INTEGER NOT NULL,
    channels INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    local_path TEXT NOT NULL,
    byte_len INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (session_id, track, sequence),
    FOREIGN KEY (session_id, track) REFERENCES tracks(session_id, track)
);

CREATE TABLE IF NOT EXISTS upload_status (
    session_id TEXT NOT NULL,
    track TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    state_tag TEXT NOT NULL,
    retryable INTEGER,
    reason TEXT,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (session_id, track, sequence),
    FOREIGN KEY (session_id, track, sequence) REFERENCES segments(session_id, track, sequence)
);

CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    detail_json TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

-- One row per `stt-api` `SttEvent::PartialTranscript`/`FinalTranscript` (see
-- `TranscriptSegment`). `track`/`speaker` are nullable: a provider without
-- diarization support leaves `speaker` unset, and a transcript not scoped to a
-- single captured track leaves `track` unset.
CREATE TABLE IF NOT EXISTS transcript_segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    track TEXT,
    speaker INTEGER,
    text TEXT NOT NULL,
    start_ms INTEGER,
    end_ms INTEGER,
    is_final INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

-- Append-only like `transcript_segments`: a session may be re-summarized (e.g.
-- with a different provider/model) without losing earlier results (see `Summary`).
CREATE TABLE IF NOT EXISTS summaries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    text TEXT NOT NULL,
    provider_model TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
CREATE INDEX IF NOT EXISTS idx_upload_status_state ON upload_status(state_tag);
CREATE INDEX IF NOT EXISTS idx_transcript_segments_session ON transcript_segments(session_id);
CREATE INDEX IF NOT EXISTS idx_summaries_session ON summaries(session_id);
"#;

pub fn open_with_pragmas(path: &std::path::Path) -> Result<Connection, StoreError> {
    open_with_pragmas_on(Connection::open(path)?)
}

pub fn open_with_pragmas_on(conn: Connection) -> Result<Connection, StoreError> {
    // WAL + busy_timeout so segment-store's writer and upload-client's worker can hold
    // connections to the same file concurrently without "database is locked" errors;
    // foreign_keys is off by default in SQLite and must be turned on per-connection.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}
