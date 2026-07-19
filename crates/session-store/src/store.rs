use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use recorder_domain::{AudioCodec, AudioSegment, CaptureState, SessionId, SessionManifest, TrackKind, UploadState};
use rusqlite::{named_params, params, Connection, OptionalExtension};

use crate::error::StoreError;
use crate::schema;
use crate::state_codec;

/// Non-terminal `CaptureState` tags — a session found in one of these at `open()` time
/// was left mid-flight by a process that never reached `Finalized`/`Failed` (crash,
/// force-quit, OS shutdown). See `reconcile_on_startup`.
const NON_TERMINAL_CAPTURE_STATE_TAGS: [&str; 4] = ["preparing", "recording", "stopping", "finalizing"];

/// One row of diarized transcript output — corresponds to a single `stt-api`
/// `SttEvent::PartialTranscript`/`FinalTranscript`. `track` is `None` when the
/// transcript isn't scoped to a single captured track; `speaker` is `None` when the
/// provider/session wasn't configured for diarization (`SttSessionConfig::diarization`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSegment {
    pub session_id: SessionId,
    pub track: Option<TrackKind>,
    pub speaker: Option<u32>,
    pub text: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub is_final: bool,
    /// `true` for a row produced by `app-service`'s manual gap re-transcription
    /// pass (task #91: `BatchSttProvider::transcribe_batch` over audio already
    /// saved to `segments`), `false` for a row produced by
    /// `live_transcription`'s streaming `SttEvent`s — the marking task #91's doc
    /// comment asked for, so a UI (or `to_turns`) can tell the two apart, e.g. to
    /// label a re-transcribed stretch differently from what was heard live. Not
    /// `is_final`'s opposite or a replacement for it: a re-transcribed row is
    /// always `is_final: true` too (batch transcription has no interim state).
    pub is_retranscribed: bool,
}

/// One live-transcription outage on one track (task #90) — corresponds to a
/// row in `transcription_gaps`. Distinct from a missing `TranscriptSegment`:
/// the audio for `[start_ms, end_ms)` was still captured and saved to
/// `segments` as normal, it just never reached the STT provider (a mid-session
/// disconnect — see `app-service`'s `live_transcription` module), so a manual
/// re-transcription pass (task #91) needs to know which stretches of audio to
/// re-run rather than the whole recording. `end_ms` is `None` while the
/// outage is still open (see `record_gap_start`/`record_gap_end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptionGap {
    pub id: i64,
    pub session_id: SessionId,
    pub track: TrackKind,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
}

/// One generated summary of a session. Append-only like `transcript_segments` — a
/// session may be re-summarized (e.g. with a different provider/model) without
/// losing earlier results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub session_id: SessionId,
    pub text: String,
    /// The provider/model used to generate this summary, e.g. `"openai/gpt-4o"`.
    pub provider_model: String,
    pub generated_at: DateTime<Utc>,
}

/// One row of `list_sessions`' past-sessions summary — enough to render the
/// desktop app's history screen (task #69) without pulling every session's full
/// transcript/segments. Unlike `AppState::last_summary` (in-memory, cleared on
/// restart), this comes straight from `sessions`, so it survives an app restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListItem {
    pub session_id: SessionId,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub capture_state: CaptureState,
}

pub struct SessionStore {
    conn: Mutex<Connection>,
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = schema::open_with_pragmas(path)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// An in-memory store — useful for tests, and for any future dry-run mode that
    /// shouldn't touch disk.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = schema::open_with_pragmas_on(Connection::open_in_memory()?)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Registers a new session and its declared tracks. The session starts in
    /// `CaptureState::Preparing` — design.md §10's diagram has no state before it once
    /// a manifest exists, since `Idle` describes the app with no session at all.
    pub fn create_session(&self, manifest: &SessionManifest) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let session_id = manifest.session_id.to_string();
        let initial_state = CaptureState::Preparing;

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions (
                session_id, schema_version, started_at, ended_at, platform, app_version,
                microphone_device_id, remote_source_id, remote_source_kind,
                sample_rate, segment_duration_ms,
                consent_confirmed_by_user, consent_confirmed_at,
                remote_session_id,
                capture_state_tag, capture_state_recoverable, capture_state_reason,
                created_at, updated_at
            ) VALUES (
                :session_id, :schema_version, :started_at, :ended_at, :platform, :app_version,
                :microphone_device_id, :remote_source_id, :remote_source_kind,
                :sample_rate, :segment_duration_ms,
                :consent_confirmed_by_user, :consent_confirmed_at,
                NULL,
                :capture_state_tag, NULL, NULL,
                :now, :now
            )",
            named_params! {
                ":session_id": session_id,
                ":schema_version": manifest.schema_version,
                ":started_at": manifest.started_at.to_rfc3339(),
                ":ended_at": manifest.ended_at.map(|t| t.to_rfc3339()),
                ":platform": manifest.platform,
                ":app_version": manifest.app_version,
                ":microphone_device_id": manifest.capture.microphone_device_id,
                ":remote_source_id": manifest.capture.remote_source_id,
                ":remote_source_kind": manifest.capture.remote_source_kind.as_str(),
                ":sample_rate": manifest.audio.sample_rate,
                ":segment_duration_ms": manifest.audio.segment_duration_ms,
                ":consent_confirmed_by_user": manifest.consent.confirmed_by_user,
                ":consent_confirmed_at": manifest.consent.confirmed_at.to_rfc3339(),
                ":capture_state_tag": state_codec::capture_state_tag(&initial_state),
                ":now": now,
            },
        )?;

        for track in &manifest.audio.tracks {
            tx.execute(
                "INSERT INTO tracks (session_id, track) VALUES (?1, ?2)",
                params![session_id, track.as_manifest_str()],
            )?;
        }

        insert_event(&tx, &session_id, "session_created", None)?;
        tx.commit()?;
        Ok(())
    }

    /// Records the API's own session identifier, once `UploadAdapter::create_session`
    /// (design.md §13) succeeds.
    pub fn set_remote_session_id(&self, session_id: SessionId, remote_session_id: &str) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE sessions SET remote_session_id = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![remote_session_id, now, session_id.to_string()],
        )?;
        if rows == 0 {
            return Err(StoreError::SessionNotFound(session_id.to_string()));
        }
        Ok(())
    }

    /// The API's own session identifier, once `set_remote_session_id` has recorded
    /// it — `None` if `UploadAdapter::create_session` never got a response before a
    /// crash (see `app-service`'s startup recovery, which needs this to know
    /// whether it can resume uploading a recovered session or must first retry
    /// `create_session` itself).
    pub fn remote_session_id(&self, session_id: SessionId) -> Result<Option<String>, StoreError> {
        let session_id_str = session_id.to_string();
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT remote_session_id FROM sessions WHERE session_id = ?1",
            params![session_id_str],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::SessionNotFound(session_id_str),
            other => StoreError::Sqlite(other),
        })
    }

    pub fn capture_state(&self, session_id: SessionId) -> Result<CaptureState, StoreError> {
        let session_id_str = session_id.to_string();
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT capture_state_tag, capture_state_recoverable, capture_state_reason
             FROM sessions WHERE session_id = ?1",
            params![session_id_str],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<bool>>(1)?, row.get::<_, Option<String>>(2)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::SessionNotFound(session_id_str.clone()),
            other => StoreError::Sqlite(other),
        })
        .and_then(|(tag, recoverable, reason)| state_codec::decode_capture_state(&tag, recoverable, reason))
    }

    pub fn upload_state(&self, session_id: SessionId, track: TrackKind, sequence: u64) -> Result<UploadState, StoreError> {
        let session_id_str = session_id.to_string();
        let track_str = track.as_manifest_str();
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT state_tag, retryable, reason FROM upload_status
             WHERE session_id = ?1 AND track = ?2 AND sequence = ?3",
            params![session_id_str, track_str, sequence],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<bool>>(1)?, row.get::<_, Option<String>>(2)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::SegmentNotFound {
                session_id: session_id_str.clone(),
                track: track_str.to_string(),
                sequence,
            },
            other => StoreError::Sqlite(other),
        })
        .and_then(|(tag, retryable, reason)| state_codec::decode_upload_state(&tag, retryable, reason))
    }

    /// How many times a segment has entered `UploadState::Uploading` — see
    /// `update_upload_state`'s doc comment for exactly what counts as an attempt.
    /// `upload-client`'s backoff pacing is expected to read this.
    pub fn upload_attempt_count(&self, session_id: SessionId, track: TrackKind, sequence: u64) -> Result<u32, StoreError> {
        let session_id_str = session_id.to_string();
        let track_str = track.as_manifest_str();
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT attempt_count FROM upload_status WHERE session_id = ?1 AND track = ?2 AND sequence = ?3",
            params![session_id_str, track_str, sequence],
            |row| row.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::SegmentNotFound { session_id: session_id_str, track: track_str.to_string(), sequence },
            other => StoreError::Sqlite(other),
        })
    }

    pub fn update_capture_state(&self, session_id: SessionId, state: &CaptureState) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let tag = state_codec::capture_state_tag(state);
        let (recoverable, reason) = state_codec::capture_state_detail(state);
        let session_id_str = session_id.to_string();

        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE sessions SET
                capture_state_tag = :tag,
                capture_state_recoverable = :recoverable,
                capture_state_reason = :reason,
                updated_at = :now,
                ended_at = CASE WHEN :tag = 'finalized' AND ended_at IS NULL THEN :now ELSE ended_at END
             WHERE session_id = :session_id",
            named_params! {
                ":tag": tag,
                ":recoverable": recoverable,
                ":reason": reason,
                ":now": now,
                ":session_id": session_id_str,
            },
        )?;
        if rows == 0 {
            return Err(StoreError::SessionNotFound(session_id_str));
        }
        insert_event(&conn, &session_id_str, "capture_state_changed", Some(tag))?;
        Ok(())
    }

    /// Whether a segment is already registered — `segment-store`'s restart-time
    /// directory scan uses this to tell "rename completed but DB registration didn't"
    /// (needs registering) apart from "already fully committed" (leave alone).
    pub fn segment_exists(&self, session_id: SessionId, track: TrackKind, sequence: u64) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE session_id = ?1 AND track = ?2 AND sequence = ?3",
            params![session_id.to_string(), track.as_manifest_str(), sequence],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Registers one committed segment (already fsynced/hashed by `segment-store`) and
    /// its initial `NotStarted` upload status, in a single transaction — a segment
    /// isn't visible to `upload-client` until both rows exist together.
    pub fn register_segment(&self, segment: &AudioSegment) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let session_id = segment.session_id.to_string();
        let track = segment.track.as_manifest_str();

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO segments (
                session_id, track, sequence, timeline_start_ms, duration_ms, codec,
                sample_rate, channels, sha256, local_path, byte_len, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                session_id,
                track,
                segment.sequence,
                segment.timeline_start_ms,
                segment.duration_ms,
                segment.codec.as_str(),
                segment.sample_rate,
                segment.channels,
                segment.sha256,
                segment.local_path.to_string_lossy(),
                segment.byte_len,
                now,
            ],
        )?;
        tx.execute(
            "INSERT INTO upload_status (
                session_id, track, sequence, state_tag, retryable, reason,
                attempt_count, last_attempt_at, updated_at
            ) VALUES (?1, ?2, ?3, 'not_started', NULL, NULL, 0, NULL, ?4)",
            params![session_id, track, segment.sequence, now],
        )?;
        insert_event(&tx, &session_id, "segment_registered", Some(track))?;
        tx.commit()?;
        Ok(())
    }

    /// Starting a new attempt (transitioning to `Uploading`) bumps `attempt_count`
    /// and `last_attempt_at`; every other transition — including that same
    /// attempt's own `Completed`/`Failed` outcome — does not. Counting only the
    /// start of an attempt (not its outcome too) means a caller can always go
    /// `Uploading -> {Completed, Failed}` for one real upload call without
    /// double-counting it; a caller that skips `Uploading` and jumps straight to
    /// `Failed` won't have that attempt counted at all, so callers that want an
    /// accurate count should always transition through `Uploading` first (see
    /// `upload_worker::upload_pending_once`).
    pub fn update_upload_state(
        &self,
        session_id: SessionId,
        track: TrackKind,
        sequence: u64,
        state: &UploadState,
    ) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let tag = state_codec::upload_state_tag(state);
        let (retryable, reason) = state_codec::upload_state_detail(state);
        let session_id_str = session_id.to_string();
        let track_str = track.as_manifest_str();
        let is_attempt = matches!(state, UploadState::Uploading);

        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE upload_status SET
                state_tag = :tag,
                retryable = :retryable,
                reason = :reason,
                attempt_count = attempt_count + :is_attempt,
                last_attempt_at = CASE WHEN :is_attempt = 1 THEN :now ELSE last_attempt_at END,
                updated_at = :now
             WHERE session_id = :session_id AND track = :track AND sequence = :sequence",
            named_params! {
                ":tag": tag,
                ":retryable": retryable,
                ":reason": reason,
                ":is_attempt": is_attempt as i64,
                ":now": now,
                ":session_id": session_id_str,
                ":track": track_str,
                ":sequence": sequence,
            },
        )?;
        if rows == 0 {
            return Err(StoreError::SegmentNotFound {
                session_id: session_id_str,
                track: track_str.to_string(),
                sequence,
            });
        }
        insert_event(&conn, &session_id_str, "upload_state_changed", Some(tag))?;
        Ok(())
    }

    /// Segments an upload worker should (re)send after a restart: anything not yet
    /// `Completed` and not permanently `Failed` (`retryable = false`).
    pub fn pending_uploads(&self, session_id: SessionId) -> Result<Vec<AudioSegment>, StoreError> {
        let session_id_str = session_id.to_string();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.track, s.sequence, s.timeline_start_ms, s.duration_ms, s.codec,
                    s.sample_rate, s.channels, s.sha256, s.local_path, s.byte_len
             FROM segments s
             JOIN upload_status u
               ON u.session_id = s.session_id AND u.track = s.track AND u.sequence = s.sequence
             WHERE s.session_id = ?1
               AND u.state_tag != 'completed'
               AND NOT (u.state_tag = 'failed' AND u.retryable = 0)
             ORDER BY s.track, s.sequence",
        )?;
        let rows = stmt.query_map(params![session_id_str], |row| {
            let track_str: String = row.get(0)?;
            let codec_str: String = row.get(4)?;
            let local_path: String = row.get(8)?;
            Ok((track_str, codec_str, local_path, row.get::<_, u64>(1)?, row.get::<_, u64>(2)?, row.get::<_, u32>(3)?, row.get::<_, u32>(5)?, row.get::<_, u16>(6)?, row.get::<_, String>(7)?, row.get::<_, u64>(9)?))
        })?;

        let mut segments = Vec::new();
        for row in rows {
            let (track_str, codec_str, local_path, sequence, timeline_start_ms, duration_ms, sample_rate, channels, sha256, byte_len) = row?;
            segments.push(AudioSegment {
                session_id,
                track: track_str.parse::<TrackKind>()?,
                sequence,
                timeline_start_ms,
                duration_ms,
                codec: codec_str.parse::<AudioCodec>()?,
                sample_rate,
                channels,
                sha256,
                local_path: PathBuf::from(local_path),
                byte_len,
            });
        }
        Ok(segments)
    }

    /// Every committed segment for one track, in sequence order, regardless of upload
    /// status — for verifying/inspecting what's on disk (e.g. a future desktop UI's
    /// "recorded so far" view), unlike `pending_uploads` which only returns segments
    /// still needing an upload attempt.
    pub fn segments_for_track(&self, session_id: SessionId, track: TrackKind) -> Result<Vec<AudioSegment>, StoreError> {
        let session_id_str = session_id.to_string();
        let track_str = track.as_manifest_str();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT track, sequence, timeline_start_ms, duration_ms, codec,
                    sample_rate, channels, sha256, local_path, byte_len
             FROM segments
             WHERE session_id = ?1 AND track = ?2
             ORDER BY sequence",
        )?;
        let rows = stmt.query_map(params![session_id_str, track_str], |row| {
            let track_str: String = row.get(0)?;
            let codec_str: String = row.get(4)?;
            let local_path: String = row.get(8)?;
            Ok((track_str, codec_str, local_path, row.get::<_, u64>(1)?, row.get::<_, u64>(2)?, row.get::<_, u32>(3)?, row.get::<_, u32>(5)?, row.get::<_, u16>(6)?, row.get::<_, String>(7)?, row.get::<_, u64>(9)?))
        })?;

        let mut segments = Vec::new();
        for row in rows {
            let (track_str, codec_str, local_path, sequence, timeline_start_ms, duration_ms, sample_rate, channels, sha256, byte_len) = row?;
            segments.push(AudioSegment {
                session_id,
                track: track_str.parse::<TrackKind>()?,
                sequence,
                timeline_start_ms,
                duration_ms,
                codec: codec_str.parse::<AudioCodec>()?,
                sample_rate,
                channels,
                sha256,
                local_path: PathBuf::from(local_path),
                byte_len,
            });
        }
        Ok(segments)
    }

    /// For `SessionSummary::segment_counts_by_track`, sent to `finalize_session`.
    pub fn segment_counts_by_track(&self, session_id: SessionId) -> Result<BTreeMap<TrackKind, u64>, StoreError> {
        let session_id_str = session_id.to_string();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT track, COUNT(*) FROM segments WHERE session_id = ?1 GROUP BY track",
        )?;
        let rows = stmt.query_map(params![session_id_str], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;

        let mut counts = BTreeMap::new();
        for row in rows {
            let (track_str, count) = row?;
            counts.insert(track_str.parse::<TrackKind>()?, count);
        }
        Ok(counts)
    }

    /// Registers one transcript event (interim or final) from an STT session.
    pub fn insert_transcript_segment(&self, segment: &TranscriptSegment) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO transcript_segments (
                session_id, track, speaker, text, start_ms, end_ms, is_final, is_retranscribed, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                segment.session_id.to_string(),
                segment.track.map(|t| t.as_manifest_str()),
                segment.speaker,
                segment.text,
                segment.start_ms,
                segment.end_ms,
                segment.is_final,
                segment.is_retranscribed,
                now,
            ],
        )?;
        Ok(())
    }

    /// Every transcript segment recorded for a session, oldest first.
    pub fn list_transcript_segments(&self, session_id: SessionId) -> Result<Vec<TranscriptSegment>, StoreError> {
        let session_id_str = session_id.to_string();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT track, speaker, text, start_ms, end_ms, is_final, is_retranscribed
             FROM transcript_segments
             WHERE session_id = ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id_str], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<u32>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<u64>>(3)?,
                row.get::<_, Option<u64>>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, bool>(6)?,
            ))
        })?;

        let mut segments = Vec::new();
        for row in rows {
            let (track_str, speaker, text, start_ms, end_ms, is_final, is_retranscribed) = row?;
            segments.push(TranscriptSegment {
                session_id,
                track: track_str.map(|t| t.parse::<TrackKind>()).transpose()?,
                speaker,
                text,
                start_ms,
                end_ms,
                is_final,
                is_retranscribed,
            });
        }
        Ok(segments)
    }

    /// Opens a new live-transcription gap on `track` (task #90), recorded the
    /// moment `app-service`'s `live_transcription` reconnect flow detects a
    /// disconnect — *before* it's known whether (or how long until) a
    /// reconnect succeeds — and returns the new row's id, to be passed to
    /// [`Self::record_gap_end`] once the outage is over. Writing the row
    /// eagerly, rather than only once the outage's full extent is known,
    /// means a long gap still shows up (with `end_ms = NULL`) even if the
    /// process crashes mid-outage instead of only existing in memory.
    pub fn record_gap_start(&self, session_id: SessionId, track: TrackKind, start_ms: u64) -> Result<i64, StoreError> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO transcription_gaps (session_id, track, start_ms, end_ms, created_at)
             VALUES (?1, ?2, ?3, NULL, ?4)",
            params![session_id.to_string(), track.as_manifest_str(), start_ms, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Closes a gap opened by [`Self::record_gap_start`] once its extent is
    /// known — either a reconnect succeeded, or the recording ended with the
    /// track still down (in which case `end_ms` is the recording's end time).
    /// A still-open gap (`end_ms` still `NULL`) is expected to be rare in
    /// practice — `live_transcription::run_live_transcription` always closes
    /// every open gap by the time it returns, on every path (reconnect
    /// success, non-retryable give-up, or `audio_rx` closing) — but nothing
    /// here enforces that, so `gaps_for_session` may still surface one if a
    /// future caller doesn't.
    ///
    /// `gap_id` is never accepted from outside this crate (only
    /// `record_gap_start`'s return value flows into it), so unlike
    /// `update_upload_state`/`update_capture_state` there is no
    /// not-found error to report here: an id matching zero rows updates zero
    /// rows and returns `Ok(())`, since that would already mean the caller
    /// has a bug rather than something this API's caller needs to react to.
    pub fn record_gap_end(&self, gap_id: i64, end_ms: u64) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE transcription_gaps SET end_ms = ?1 WHERE id = ?2", params![end_ms, gap_id])?;
        Ok(())
    }

    /// Discards a gap opened by [`Self::record_gap_start`] without ever
    /// recording an `end_ms` for it — used instead of `record_gap_end` when
    /// the outage turned out shorter than
    /// `live_transcription::MIN_RECORDED_GAP_MS` (see that constant's doc
    /// comment): the row already exists (it was written eagerly, before the
    /// outage's length was known), so once the length turns out too short to
    /// be worth surfacing, this removes it rather than leaving a sub-second
    /// row behind for `gaps_for_session` callers to filter out themselves.
    pub fn discard_gap(&self, gap_id: i64) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM transcription_gaps WHERE id = ?1", params![gap_id])?;
        Ok(())
    }

    /// Every recorded live-transcription gap for a session, oldest first —
    /// task #91's manual re-transcription pass reads this to know which
    /// `[start_ms, end_ms)` stretches (per track) to re-run rather than the
    /// whole recording.
    pub fn gaps_for_session(&self, session_id: SessionId) -> Result<Vec<TranscriptionGap>, StoreError> {
        let session_id_str = session_id.to_string();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, track, start_ms, end_ms FROM transcription_gaps
             WHERE session_id = ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id_str], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, u64>(2)?, row.get::<_, Option<u64>>(3)?))
        })?;

        let mut gaps = Vec::new();
        for row in rows {
            let (id, track_str, start_ms, end_ms) = row?;
            gaps.push(TranscriptionGap { id, session_id, track: track_str.parse::<TrackKind>()?, start_ms, end_ms });
        }
        Ok(gaps)
    }

    /// Records one summarization result.
    pub fn insert_summary(&self, summary: &Summary) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO summaries (session_id, text, provider_model, generated_at) VALUES (?1, ?2, ?3, ?4)",
            params![summary.session_id.to_string(), summary.text, summary.provider_model, summary.generated_at.to_rfc3339()],
        )?;
        Ok(())
    }

    /// The most recently generated summary for a session — `None` if it has never
    /// been summarized. A session may be re-summarized multiple times (see
    /// `Summary`'s doc comment); this is "the current one" for a UI to show.
    pub fn get_latest_summary(&self, session_id: SessionId) -> Result<Option<Summary>, StoreError> {
        let session_id_str = session_id.to_string();
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT text, provider_model, generated_at FROM summaries
                 WHERE session_id = ?1
                 ORDER BY generated_at DESC, id DESC
                 LIMIT 1",
                params![session_id_str],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
            )
            .optional()?;

        row.map(|(text, provider_model, generated_at)| {
            Ok(Summary {
                session_id,
                text,
                provider_model,
                generated_at: DateTime::parse_from_rfc3339(&generated_at)?.with_timezone(&Utc),
            })
        })
        .transpose()
    }

    /// Every session ever recorded, newest first (`started_at` descending) —
    /// backs the desktop app's past-sessions history screen (task #69), the only
    /// way to reach a session recorded in an earlier app run once
    /// `AppState::last_summary` (in-memory only) has been cleared by a restart.
    pub fn list_sessions(&self) -> Result<Vec<SessionListItem>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, started_at, ended_at,
                    capture_state_tag, capture_state_recoverable, capture_state_reason
             FROM sessions
             ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<bool>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;

        let mut items = Vec::new();
        for row in rows {
            let (session_id_str, started_at, ended_at, tag, recoverable, reason) = row?;
            let session_id = session_id_str
                .parse::<SessionId>()
                .map_err(|_| StoreError::SessionNotFound(session_id_str))?;
            items.push(SessionListItem {
                session_id,
                started_at: DateTime::parse_from_rfc3339(&started_at)?.with_timezone(&Utc),
                ended_at: ended_at.map(|s| DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc))).transpose()?,
                capture_state: state_codec::decode_capture_state(&tag, recoverable, reason)?,
            });
        }
        Ok(items)
    }

    /// Finds sessions a previous process instance left in a non-terminal
    /// `CaptureState` (i.e. it never reached `Finalized`/`Failed`), marks each
    /// `Failed { recoverable: true }`, and returns their IDs so `app-service` can drive
    /// them through finalization/upload without starting a fresh recording.
    pub fn reconcile_on_startup(&self) -> Result<Vec<SessionId>, StoreError> {
        let now = Utc::now().to_rfc3339();
        let placeholders = NON_TERMINAL_CAPTURE_STATE_TAGS
            .iter()
            .map(|tag| format!("'{tag}'"))
            .collect::<Vec<_>>()
            .join(", ");

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let session_ids: Vec<String> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT session_id FROM sessions WHERE capture_state_tag IN ({placeholders})"
            ))?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for session_id_str in &session_ids {
            tx.execute(
                "UPDATE sessions SET
                    capture_state_tag = 'failed',
                    capture_state_recoverable = 1,
                    capture_state_reason = 'process restarted while session was in progress',
                    updated_at = ?1,
                    ended_at = COALESCE(ended_at, ?1)
                 WHERE session_id = ?2",
                params![now, session_id_str],
            )?;
            insert_event(&tx, session_id_str, "reconciled_after_restart", None)?;
        }
        tx.commit()?;

        session_ids
            .into_iter()
            .map(|s| s.parse::<SessionId>().map_err(|_| StoreError::SessionNotFound(s)))
            .collect()
    }
}

fn insert_event(conn: &Connection, session_id: &str, kind: &str, detail: Option<&str>) -> Result<(), StoreError> {
    let now = Utc::now().to_rfc3339();
    let detail_json = serde_json::to_string(&detail.unwrap_or_default())?;
    conn.execute(
        "INSERT INTO events (session_id, occurred_at, kind, detail_json) VALUES (?1, ?2, ?3, ?4)",
        params![session_id, now, kind, detail_json],
    )?;
    Ok(())
}
