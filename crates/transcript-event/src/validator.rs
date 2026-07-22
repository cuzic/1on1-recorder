//! Protocol validator for the canonical transcript event stream.
//!
//! Ensures events flowing through the broker respect the protocol invariants:
//! - `SegmentUpdated` → `SegmentFinalized` state transition
//! - No duplicate `SegmentFinalized` for the same segment_id
//! - Revision monotonicity per segment_id
//! - SessionId consistency
//!
//! Does NOT validate:
//! - `event_id` (always a ULID via `EventEnvelope::new()`)
//! - Whether `SegmentUpdated` preceded `SegmentFinalized` (the producer guarantees
//!   it by emitting both in the same call — see `live_transcription.rs`)
//!
//! # Memory management
//!
//! Internal HashMaps grow with each finalized segment. An eviction strategy caps
//! the tracked set at [`MAX_TRACKED_SEGMENTS`] by removing the oldest entries.
//! Call [`reset`] at session end to free all memory.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use recorder_domain::SessionId;

use crate::{Finality, SegmentData, TranscriptEvent, UtteranceEndReason};

/// Maximum number of segment_ids tracked before eviction kicks in.
/// A typical 1-hour meeting produces ~200-400 segments, so 10,000 is generous.
const MAX_TRACKED_SEGMENTS: usize = 10_000;

/// Errors detected by [`ProtocolValidator::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// `SegmentFinalized` was emitted twice for the same segment_id.
    DuplicateFinalized { segment_id: String },
    /// `SegmentUpdated` arrived for a segment that was already finalized.
    UpdateAfterFinalized { segment_id: String },
    /// Revision decreased (or stayed the same) for a segment_id.
    RevisionNotMonotonic {
        segment_id: String,
        last_seen: u32,
        got: u32,
    },
    /// The event's `session_id` does not match the validator's.
    SessionIdMismatch {
        expected: SessionId,
        got: SessionId,
    },
    /// `SegmentFinalized` data differs from the last `SegmentUpdated` for the
    /// same segment_id. This indicates a data integrity issue in the producer.
    FinalizedDataMismatch {
        segment_id: String,
        field: String,
        updated_value: String,
        finalized_value: String,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateFinalized { segment_id } => {
                write!(f, "duplicate SegmentFinalized for segment '{segment_id}'")
            }
            Self::UpdateAfterFinalized { segment_id } => {
                write!(f, "SegmentUpdated after SegmentFinalized for segment '{segment_id}'")
            }
            Self::RevisionNotMonotonic { segment_id, last_seen, got } => {
                write!(f, "revision not monotonic for segment '{segment_id}': last={last_seen}, got={got}")
            }
            Self::SessionIdMismatch { expected, got } => {
                write!(f, "session_id mismatch: expected {expected}, got {got}")
            }
            Self::FinalizedDataMismatch { segment_id, field, updated_value, finalized_value } => {
                write!(f, "SegmentFinalized data mismatch for segment '{segment_id}' field '{field}': updated='{updated_value}', finalized='{finalized_value}'")
            }
        }
    }
}

/// A warning-level observation — not an error (the protocol is still valid),
/// but worth noting for debugging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationWarning {
    /// `UtteranceEnded(SessionEnd)` was emitted more than once (idempotent).
    DuplicateSessionEnd,
}

/// Per-session state machine that validates [`TranscriptEvent`]s.
///
/// # Thread safety
///
/// `ProtocolValidator` takes `&mut self` — wrap in `Arc<Mutex<ProtocolValidator>>`
/// if shared across consumers.
#[derive(Debug)]
pub struct ProtocolValidator {
    session_id: SessionId,

    /// Segment IDs that have been finalized (prevents double-finalize).
    finalized: HashSet<String>,

    /// The last `SegmentData` seen via `SegmentUpdated` for each segment_id,
    /// used to validate `FinalizedDataMismatch`.
    last_updated_data: HashMap<String, SegmentData>,

    /// The last revision seen for each segment_id (monotonicity check).
    revisions: HashMap<String, u32>,

    /// Insertion order for eviction — oldest entries are removed first.
    insertion_order: VecDeque<String>,

    /// Whether `UtteranceEnded(SessionEnd)` has been seen.
    session_ended: bool,
}

impl ProtocolValidator {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            finalized: HashSet::new(),
            last_updated_data: HashMap::new(),
            revisions: HashMap::new(),
            insertion_order: VecDeque::new(),
            session_ended: false,
        }
    }

    /// Validates a single event. Returns `Ok(())` if the event is canonical,
    /// along with any non-fatal warnings.
    pub fn validate(&mut self, event: &TranscriptEvent) -> Result<Vec<ValidationWarning>, ValidationError> {
        let mut warnings = Vec::new();

        // --- SessionId check (applies to all events) ---
        let event_session_id = match event {
            TranscriptEvent::SegmentUpdated { session_id, .. }
            | TranscriptEvent::SegmentFinalized { session_id, .. }
            | TranscriptEvent::UtteranceEnded { session_id, .. } => *session_id,
        };
        if event_session_id != self.session_id {
            return Err(ValidationError::SessionIdMismatch {
                expected: self.session_id,
                got: event_session_id,
            });
        }

        match event {
            TranscriptEvent::SegmentUpdated { data, finality, .. } => {
                self.validate_segment_updated(data, *finality, &mut warnings)?;
            }
            TranscriptEvent::SegmentFinalized { data, .. } => {
                self.validate_segment_finalized(data, &mut warnings)?;
            }
            TranscriptEvent::UtteranceEnded { reason, .. } => {
                self.validate_utterance_ended(*reason, &mut warnings);
            }
        }

        Ok(warnings)
    }

    fn validate_segment_updated(
        &mut self,
        data: &SegmentData,
        _finality: Finality,
        _warnings: &mut Vec<ValidationWarning>,
    ) -> Result<(), ValidationError> {
        let sid = &data.segment_id;

        // Update-after-finalized is a protocol violation
        if self.finalized.contains(sid) {
            return Err(ValidationError::UpdateAfterFinalized { segment_id: sid.clone() });
        }

        // Revision monotonicity
        if let Some(&last_rev) = self.revisions.get(sid) {
            if data.revision <= last_rev {
                return Err(ValidationError::RevisionNotMonotonic {
                    segment_id: sid.clone(),
                    last_seen: last_rev,
                    got: data.revision,
                });
            }
        }

        // Track the latest data for FinalizedDataMismatch check
        self.last_updated_data.insert(sid.clone(), data.clone());
        self.revisions.insert(sid.clone(), data.revision);

        // Track insertion order for eviction
        if !self.insertion_order.iter().any(|id| id == sid) {
            self.insertion_order.push_back(sid.clone());
            self.maybe_evict();
        }

        // If this is a Final update, it's not an error but we don't need to
        // do anything special — SegmentFinalized will follow shortly.

        Ok(())
    }

    fn validate_segment_finalized(
        &mut self,
        data: &SegmentData,
        _warnings: &mut Vec<ValidationWarning>,
    ) -> Result<(), ValidationError> {
        let sid = &data.segment_id;

        // Duplicate finalize check
        if !self.finalized.insert(sid.clone()) {
            return Err(ValidationError::DuplicateFinalized { segment_id: sid.clone() });
        }

        // Data mismatch: compare against the last SegmentUpdated data
        if let Some(last) = self.last_updated_data.get(sid) {
            if last.text != data.text {
                return Err(ValidationError::FinalizedDataMismatch {
                    segment_id: sid.clone(),
                    field: "text".to_string(),
                    updated_value: last.text.clone(),
                    finalized_value: data.text.clone(),
                });
            }
            if last.speaker_label != data.speaker_label {
                return Err(ValidationError::FinalizedDataMismatch {
                    segment_id: sid.clone(),
                    field: "speaker_label".to_string(),
                    updated_value: last.speaker_label.clone(),
                    finalized_value: data.speaker_label.clone(),
                });
            }
            if last.start_ms != data.start_ms {
                return Err(ValidationError::FinalizedDataMismatch {
                    segment_id: sid.clone(),
                    field: "start_ms".to_string(),
                    updated_value: format!("{:?}", last.start_ms),
                    finalized_value: format!("{:?}", data.start_ms),
                });
            }
            if last.end_ms != data.end_ms {
                return Err(ValidationError::FinalizedDataMismatch {
                    segment_id: sid.clone(),
                    field: "end_ms".to_string(),
                    updated_value: format!("{:?}", last.end_ms),
                    finalized_value: format!("{:?}", data.end_ms),
                });
            }
        }

        Ok(())
    }

    fn validate_utterance_ended(
        &mut self,
        reason: UtteranceEndReason,
        warnings: &mut Vec<ValidationWarning>,
    ) {
        if reason == UtteranceEndReason::SessionEnd {
            if self.session_ended {
                warnings.push(ValidationWarning::DuplicateSessionEnd);
            }
            self.session_ended = true;
        }
    }

    /// Evicts the oldest entries when the tracked set exceeds the cap.
    fn maybe_evict(&mut self) {
        while self.insertion_order.len() > MAX_TRACKED_SEGMENTS {
            if let Some(oldest) = self.insertion_order.pop_front() {
                self.finalized.remove(&oldest);
                self.last_updated_data.remove(&oldest);
                self.revisions.remove(&oldest);
            }
        }
    }

    /// Resets all state for a new session.
    pub fn reset(&mut self) {
        self.finalized.clear();
        self.last_updated_data.clear();
        self.revisions.clear();
        self.insertion_order.clear();
        self.session_ended = false;
    }

    /// Returns the number of tracked segments (for diagnostics).
    pub fn tracked_count(&self) -> usize {
        self.finalized.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SegmentData;

    fn make_segment(_session_id: SessionId, sid: &str, rev: u32, text: &str) -> SegmentData {
        SegmentData {
            segment_id: sid.to_string(),
            revision: rev,
            text: text.to_string(),
            speaker_label: "自分".to_string(),
            track: recorder_domain::TrackKind::SelfMic,
            start_ms: Some(0),
            end_ms: Some(1000),
        }
    }

    fn session_id() -> SessionId {
        SessionId::new()
    }

    #[test]
    fn valid_sequence_passes() {
        let sid = session_id();
        let mut v = ProtocolValidator::new(sid);
        let data = make_segment(sid, "seg1", 0, "hello");

        assert!(v
            .validate(&TranscriptEvent::SegmentUpdated {
                session_id: sid,
                data: data.clone(),
                finality: Finality::Final,
            })
            .is_ok());
        assert!(v
            .validate(&TranscriptEvent::SegmentFinalized { session_id: sid, data })
            .is_ok());
    }

    #[test]
    fn duplicate_finalized_is_error() {
        let sid = session_id();
        let mut v = ProtocolValidator::new(sid);
        let data = make_segment(sid, "seg1", 0, "hello");

        v.validate(&TranscriptEvent::SegmentUpdated {
            session_id: sid,
            data: data.clone(),
            finality: Finality::Final,
        })
        .unwrap();
        v.validate(&TranscriptEvent::SegmentFinalized { session_id: sid, data: data.clone() })
            .unwrap();

        let err = v
            .validate(&TranscriptEvent::SegmentFinalized { session_id: sid, data })
            .unwrap_err();
        assert_eq!(
            err,
            ValidationError::DuplicateFinalized { segment_id: "seg1".to_string() }
        );
    }

    #[test]
    fn update_after_finalized_is_error() {
        let sid = session_id();
        let mut v = ProtocolValidator::new(sid);
        let data = make_segment(sid, "seg1", 0, "hello");

        v.validate(&TranscriptEvent::SegmentUpdated {
            session_id: sid,
            data: data.clone(),
            finality: Finality::Final,
        })
        .unwrap();
        v.validate(&TranscriptEvent::SegmentFinalized { session_id: sid, data })
            .unwrap();

        let data2 = make_segment(sid, "seg1", 1, "hello updated");
        let err = v
            .validate(&TranscriptEvent::SegmentUpdated {
                session_id: sid,
                data: data2,
                finality: Finality::Interim,
            })
            .unwrap_err();
        assert_eq!(
            err,
            ValidationError::UpdateAfterFinalized { segment_id: "seg1".to_string() }
        );
    }

    #[test]
    fn revision_not_monotonic_is_error() {
        let sid = session_id();
        let mut v = ProtocolValidator::new(sid);
        let d1 = make_segment(sid, "seg1", 2, "hello");

        v.validate(&TranscriptEvent::SegmentUpdated {
            session_id: sid,
            data: d1,
            finality: Finality::Interim,
        })
        .unwrap();

        let d2 = make_segment(sid, "seg1", 1, "hello");
        let err = v
            .validate(&TranscriptEvent::SegmentUpdated {
                session_id: sid,
                data: d2,
                finality: Finality::Interim,
            })
            .unwrap_err();
        assert!(matches!(err, ValidationError::RevisionNotMonotonic { .. }));
    }

    #[test]
    fn session_id_mismatch_is_error() {
        let sid = session_id();
        let other = session_id();
        let mut v = ProtocolValidator::new(sid);
        let data = make_segment(other, "seg1", 0, "hello");

        let err = v
            .validate(&TranscriptEvent::SegmentUpdated {
                session_id: other,
                data,
                finality: Finality::Interim,
            })
            .unwrap_err();
        assert!(matches!(err, ValidationError::SessionIdMismatch { .. }));
    }

    #[test]
    fn finalized_data_mismatch_is_error() {
        let sid = session_id();
        let mut v = ProtocolValidator::new(sid);
        let d1 = make_segment(sid, "seg1", 0, "hello");

        v.validate(&TranscriptEvent::SegmentUpdated {
            session_id: sid,
            data: d1,
            finality: Finality::Final,
        })
        .unwrap();

        let d2 = make_segment(sid, "seg1", 0, "different text");
        let err = v
            .validate(&TranscriptEvent::SegmentFinalized { session_id: sid, data: d2 })
            .unwrap_err();
        assert!(matches!(err, ValidationError::FinalizedDataMismatch { .. }));
    }

    #[test]
    fn duplicate_session_end_is_warning() {
        let sid = session_id();
        let mut v = ProtocolValidator::new(sid);

        let w = v
            .validate(&TranscriptEvent::UtteranceEnded {
                session_id: sid,
                segment_id: None,
                reason: UtteranceEndReason::SessionEnd,
            })
            .unwrap();
        assert!(w.is_empty());

        let w = v
            .validate(&TranscriptEvent::UtteranceEnded {
                session_id: sid,
                segment_id: None,
                reason: UtteranceEndReason::SessionEnd,
            })
            .unwrap();
        assert_eq!(w, vec![ValidationWarning::DuplicateSessionEnd]);
    }

    #[test]
    fn eviction_removes_oldest_entries() {
        let sid = session_id();
        let mut v = ProtocolValidator::new(sid);

        // Fill beyond MAX_TRACKED_SEGMENTS
        for i in 0..(MAX_TRACKED_SEGMENTS + 10) {
            let sid_str = format!("seg{i}");
            let data = make_segment(sid, &sid_str, 0, "text");
            v.validate(&TranscriptEvent::SegmentUpdated {
                session_id: sid,
                data,
                finality: Finality::Final,
            })
            .unwrap();
        }

        assert!(v.tracked_count() <= MAX_TRACKED_SEGMENTS);
        // The oldest entries should have been evicted
        assert!(!v.finalized.contains("seg0"));
    }

    #[test]
    fn reset_clears_all_state() {
        let sid = session_id();
        let mut v = ProtocolValidator::new(sid);
        let data = make_segment(sid, "seg1", 0, "hello");

        v.validate(&TranscriptEvent::SegmentUpdated {
            session_id: sid,
            data,
            finality: Finality::Final,
        })
        .unwrap();

        v.reset();
        assert_eq!(v.tracked_count(), 0);
        assert!(!v.session_ended);
    }
}