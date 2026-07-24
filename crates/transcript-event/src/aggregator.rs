//! Reusable segment-merging primitives shared by every consumer of
//! [`TranscriptEvent`](crate::TranscriptEvent). These implement two distinct
//! semantics consumers otherwise tend to hand-roll separately:
//!
//! - [`SegmentSnapshot`]: "what does the transcript look like right now" — a live,
//!   overwrite-on-update view including interim text (`apps/desktop/src/ui_consumer.rs`'s
//!   use case).
//! - [`FinalizedTurns`]: "what has been said so far, once each" — an append-only,
//!   deduplicated log of only the segments that reached `Finality::Final`
//!   (`apps/desktop/src/summary_consumer.rs`'s use case).
//!
//! Both exist because `tokio::broadcast`'s `lagged` semantics (see
//! `local-broker`'s crate doc comment) mean every consumer needs a way to safely
//! replay/rebuild its state from `SessionStore` without double-counting — that's
//! `SegmentSnapshot::apply`'s idempotent overwrite and `FinalizedTurns::insert_if_new`'s
//! dedup-by-segment_id, respectively.

use std::collections::{BTreeMap, HashSet};

use crate::{Finality, SegmentData};

/// Tracks the latest known state of every segment in a session, keyed by
/// `segment_id`. Applying a `SegmentUpdated` event always overwrites whatever was
/// previously stored for that `segment_id` — interim text is replaced by later
/// interim or final text, never merged. This is a live snapshot, not an append log
/// (see [`FinalizedTurns`] for that).
#[derive(Debug, Default)]
pub struct SegmentSnapshot {
    segments: BTreeMap<String, (SegmentData, Finality)>,
}

impl SegmentSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Upserts `data` under `data.segment_id`, replacing whatever was there before.
    pub fn apply(&mut self, data: SegmentData, finality: Finality) {
        self.segments.insert(data.segment_id.clone(), (data, finality));
    }

    /// The current segments, ordered by `start_ms` ascending (`None` sorts as `0`,
    /// matching the pre-extraction behavior every caller of this already relied on).
    pub fn ordered_by_start(&self) -> Vec<(&SegmentData, Finality)> {
        let mut ordered: Vec<(&SegmentData, Finality)> =
            self.segments.values().map(|(data, finality)| (data, *finality)).collect();
        ordered.sort_by_key(|(data, _)| data.start_ms.unwrap_or(0));
        ordered
    }

    /// Discards all tracked segments — used when a consumer has just reloaded a
    /// full, authoritative snapshot from durable storage (e.g. after a `lagged`
    /// broadcast gap) and wants subsequent live events to rebuild from scratch
    /// rather than merge with now-possibly-stale in-memory state.
    pub fn clear(&mut self) {
        self.segments.clear();
    }
}

/// Collects segments into an ordered, deduplicated-by-`segment_id` list. Once a
/// `segment_id` has been recorded, later attempts to insert the same id (e.g. a
/// duplicate `SegmentFinalized` replayed during lagged-recovery) are ignored —
/// unlike [`SegmentSnapshot`], entries are never overwritten once inserted.
#[derive(Debug, Default)]
pub struct FinalizedTurns<T> {
    seen: HashSet<String>,
    turns: Vec<T>,
}

impl<T> FinalizedTurns<T> {
    pub fn new() -> Self {
        Self { seen: HashSet::new(), turns: Vec::new() }
    }

    /// Appends `value` and returns `true` if `segment_id` is new; otherwise leaves
    /// `self` unchanged and returns `false`.
    pub fn insert_if_new(&mut self, segment_id: String, value: T) -> bool {
        if self.seen.insert(segment_id) {
            self.turns.push(value);
            true
        } else {
            false
        }
    }

    pub fn turns(&self) -> &[T] {
        &self.turns
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use recorder_domain::TrackKind;

    fn segment(id: &str, start_ms: Option<u64>, text: &str) -> SegmentData {
        SegmentData {
            segment_id: id.to_string(),
            revision: 0,
            text: text.to_string(),
            speaker_label: "自分".to_string(),
            track: TrackKind::SelfMic,
            start_ms,
            end_ms: None,
        }
    }

    #[test]
    fn segment_snapshot_apply_overwrites_same_segment_id() {
        let mut snapshot = SegmentSnapshot::new();
        snapshot.apply(segment("a", Some(100), "interim"), Finality::Interim);
        snapshot.apply(segment("a", Some(100), "final text"), Finality::Final);

        let ordered = snapshot.ordered_by_start();
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].0.text, "final text");
        assert_eq!(ordered[0].1, Finality::Final);
    }

    #[test]
    fn segment_snapshot_orders_by_start_ms_treating_none_as_zero() {
        let mut snapshot = SegmentSnapshot::new();
        snapshot.apply(segment("late", Some(500), "b"), Finality::Final);
        snapshot.apply(segment("unset", None, "c"), Finality::Final);
        snapshot.apply(segment("early", Some(100), "a"), Finality::Final);

        let ids: Vec<&str> =
            snapshot.ordered_by_start().into_iter().map(|(d, _)| d.segment_id.as_str()).collect();
        assert_eq!(ids, vec!["unset", "early", "late"]);
    }

    #[test]
    fn segment_snapshot_clear_removes_all_tracked_segments() {
        let mut snapshot = SegmentSnapshot::new();
        snapshot.apply(segment("a", Some(0), "x"), Finality::Final);
        snapshot.clear();
        assert!(snapshot.ordered_by_start().is_empty());
    }

    #[test]
    fn finalized_turns_ignores_duplicate_segment_ids() {
        let mut turns: FinalizedTurns<&str> = FinalizedTurns::new();
        assert!(turns.insert_if_new("a".to_string(), "first"));
        assert!(!turns.insert_if_new("a".to_string(), "duplicate"));
        assert_eq!(turns.turns(), &["first"]);
    }

    #[test]
    fn finalized_turns_preserves_insertion_order() {
        let mut turns: FinalizedTurns<&str> = FinalizedTurns::new();
        turns.insert_if_new("a".to_string(), "one");
        turns.insert_if_new("b".to_string(), "two");
        turns.insert_if_new("c".to_string(), "three");
        assert_eq!(turns.turns(), &["one", "two", "three"]);
    }

    #[test]
    fn finalized_turns_is_empty_reflects_turns_length() {
        let mut turns: FinalizedTurns<&str> = FinalizedTurns::new();
        assert!(turns.is_empty());
        turns.insert_if_new("a".to_string(), "one");
        assert!(!turns.is_empty());
    }
}
