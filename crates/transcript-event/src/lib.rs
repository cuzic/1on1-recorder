//! Canonical transcript events for the event-driven decoupling between
//! transcription and summary generation (see `docs/decouple-summary-transcription.md`).
//!
//! This crate defines the single message contract between the Local Broker and all
//! consumers. It has no dependency on any STT provider, summarizer, or UI crate —
//! "data in, data out" is the entire contract.
//!
//! Two event families:
//! - [`TranscriptEvent`]: published by `live_transcription` after converting `SttEvent`
//!   into a provider-agnostic form.
//! - [`SummaryEvent`]: published by the Summary Consumer to report generation progress
//!   and results.
//!
//! Protocol validation is provided by [`ProtocolValidator`] — see the [`validator`]
//! module for the canonicality rules.

pub mod aggregator;
pub mod validator;

pub use aggregator::{FinalizedTurns, SegmentSnapshot};
pub use validator::{ProtocolValidator, ValidationError, ValidationWarning};

use chrono::{DateTime, Utc};
use recorder_domain::{SessionId, TrackKind};
use serde::{Deserialize, Serialize};

/// Common data shared by `SegmentUpdated` and `SegmentFinalized`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentData {
    pub segment_id: String,
    pub revision: u32,
    pub text: String,
    pub speaker_label: String,
    pub track: TrackKind,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Finality {
    Interim,
    Final,
}

/// A provider-agnostic transcript event, published by `live_transcription` to the
/// Local Broker. Every consumer subscribes to one or more of the subjects derived
/// from these variants (see [`subject_for`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranscriptEvent {
    /// The segment's text was updated (latest snapshot, not a diff).
    SegmentUpdated {
        session_id: SessionId,
        data: SegmentData,
        finality: Finality,
    },
    /// This segment will not be updated again (emitted for `is_final: true`).
    SegmentFinalized {
        session_id: SessionId,
        data: SegmentData,
    },
    /// An utterance boundary was reached.
    UtteranceEnded {
        session_id: SessionId,
        segment_id: Option<String>,
        reason: UtteranceEndReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UtteranceEndReason {
    /// Deepgram `speech_final`, AssemblyAI `end_of_turn`.
    EndOfTurn,
    /// Google `SPEECH_ACTIVITY_END` etc.
    SpeechPause,
    /// STT session `finalize()` completed — the recording has ended.
    SessionEnd,
}

/// Published by the Summary Consumer to report generation progress and results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SummaryEvent {
    /// Summary generation has started.
    Started {
        session_id: SessionId,
    },
    /// Summary generation completed successfully.
    Completed {
        session_id: SessionId,
        text: String,
        provider_model: String,
    },
    /// Summary generation failed.
    Failed {
        session_id: SessionId,
        error: String,
    },
}

/// Common envelope for every message flowing through the Local Broker.
/// `T` is the event body type — typically [`TranscriptEvent`] or [`SummaryEvent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope<T> {
    /// ULID, used by consumers for idempotency.
    pub event_id: String,
    pub schema_version: u32,
    pub producer: String,
    pub session_id: SessionId,
    pub occurred_at: DateTime<Utc>,
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub body: T,
}

impl<T> EventEnvelope<T> {
    pub fn new(session_id: SessionId, body: T) -> Self {
        Self {
            event_id: ulid::Ulid::new().to_string(),
            schema_version: 1,
            producer: "capture-app".to_string(),
            session_id,
            occurred_at: Utc::now(),
            correlation_id: None,
            causation_id: None,
            body,
        }
    }
}

/// Returns the Broker subject string for a [`TranscriptEvent`] variant.
pub fn subject_for(event: &TranscriptEvent, session_id: SessionId) -> String {
    match event {
        TranscriptEvent::SegmentUpdated { .. } => {
            format!("transcription.{session_id}.segment.updated")
        }
        TranscriptEvent::SegmentFinalized { .. } => {
            format!("transcription.{session_id}.segment.finalized")
        }
        TranscriptEvent::UtteranceEnded { .. } => {
            format!("transcription.{session_id}.utterance.ended")
        }
    }
}

/// Returns the Broker subject string for a [`SummaryEvent`] variant.
pub fn summary_subject_for(event: &SummaryEvent, session_id: SessionId) -> String {
    match event {
        SummaryEvent::Started { .. } => format!("summary.{session_id}.started"),
        SummaryEvent::Completed { .. } => format!("summary.{session_id}.completed"),
        SummaryEvent::Failed { .. } => format!("summary.{session_id}.failed"),
    }
}

/// Generates a stable `segment_id` from a session, track, and monotonic counter.
/// Format: `{session_id}:{track_key}:{counter}`.
pub fn segment_id_for(session_id: SessionId, track: TrackKind, counter: u64) -> String {
    let track_key = match track {
        TrackKind::SelfMic => "self",
        TrackKind::RemoteAudio => "remote",
    };
    format!("{session_id}:{track_key}:{counter}")
}

/// Derives a `segment_id` from an already-committed `TranscriptSegment` row.
/// Uses `start_ms` as a stable key, falling back to the text hash when absent.
pub fn segment_id_for_segment(
    session_id: SessionId,
    track: Option<TrackKind>,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
) -> String {
    let track_key = match track {
        Some(TrackKind::SelfMic) => "self",
        Some(TrackKind::RemoteAudio) => "remote",
        None => "unknown",
    };
    let start = start_ms.unwrap_or(0);
    let end = end_ms.unwrap_or(start);
    format!("{session_id}:{track_key}:{start}-{end}")
}