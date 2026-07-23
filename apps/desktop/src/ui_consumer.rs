//! UI Consumer: subscribes to `transcription.{session_id}.segment.updated` via the
//! Local Broker and updates a shared transcript buffer in real time.
//!
//! See `docs/decouple-summary-transcription.md` §6.1.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use local_broker::LocalBroker;
use recorder_domain::SessionId;
use session_store::{SessionStore, TranscriptSegment};
use transcript_event::{EventEnvelope, Finality, ProtocolValidator, SegmentData, TranscriptEvent};

/// A shared buffer that the UI Consumer writes to and the UI polling loop reads from.
#[derive(Clone, Default)]
pub struct TranscriptBuffer {
    pub segments: Arc<Mutex<Vec<TranscriptSegment>>>,
}

impl TranscriptBuffer {
    pub fn new() -> Self {
        Self { segments: Arc::new(Mutex::new(Vec::new())) }
    }

    pub fn take(&self) -> Vec<TranscriptSegment> {
        self.segments.lock().unwrap().clone()
    }
}

/// Spawns a background task that subscribes to broker events for `session_id` and
/// updates `buffer` with the latest transcript state. Returns immediately.
///
/// The task ends itself as soon as `apps/desktop/src/recording.rs::stop`
/// publishes `session.{id}.stopped` — before that was added, this looped
/// forever on `rx.recv()` waiting for the broker channel to close, which
/// never happens (`LocalBroker` keeps a subject's sender alive as long as
/// any receiver, including this task's own, still exists), leaking one task
/// per recording session for the process's lifetime.
pub fn spawn_ui_consumer(
    broker: LocalBroker,
    session_id: SessionId,
    store: Arc<SessionStore>,
    buffer: TranscriptBuffer,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_ui_consumer(broker, session_id, store, buffer).await;
    })
}

async fn run_ui_consumer(
    broker: LocalBroker,
    session_id: SessionId,
    store: Arc<SessionStore>,
    buffer: TranscriptBuffer,
) {
    let subject = format!("transcription.{session_id}.segment.updated");
    let stop_subject = format!("session.{session_id}.stopped");
    let mut rx = broker.subscribe(&subject);
    let mut stop_rx = broker.subscribe(&stop_subject);

    let mut segment_map: BTreeMap<String, TranscriptSegment> = BTreeMap::new();
    let mut validator = ProtocolValidator::new(session_id);

    loop {
        let payload = tokio::select! {
            result = rx.recv() => match result {
                Ok(p) => p,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, %session_id, "ui_consumer: lagged, reloading from store");
                    if let Ok(segments) = store.list_transcript_segments(session_id) {
                        if let Ok(mut guard) = buffer.segments.lock() {
                            *guard = segments;
                        }
                    }
                    rx = broker.subscribe(&subject);
                    segment_map.clear();
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = stop_rx.recv() => break,
        };

        let envelope: EventEnvelope<TranscriptEvent> = match serde_json::from_slice(&payload) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(%err, "ui_consumer: failed to deserialize event");
                continue;
            }
        };

        let (data, finality) = match envelope.body {
            TranscriptEvent::SegmentUpdated { data, finality, .. } => (data, finality),
            _ => continue,
        };

        // Validate against the canonical protocol
        if let Err(err) = validator.validate(&TranscriptEvent::SegmentUpdated {
            session_id,
            data: data.clone(),
            finality,
        }) {
            tracing::warn!(%session_id, %err, "ui_consumer: protocol validation error");
        }

        let segment = data_to_transcript_segment(&data, finality);
        segment_map.insert(data.segment_id, segment);

        let mut segments: Vec<TranscriptSegment> = segment_map.values().cloned().collect();
        segments.sort_by_key(|s| s.start_ms.unwrap_or(0));

        if let Ok(mut guard) = buffer.segments.lock() {
            *guard = segments;
        }
    }
}

fn data_to_transcript_segment(data: &SegmentData, finality: Finality) -> TranscriptSegment {
    let is_final = matches!(finality, Finality::Final);
    let speaker = parse_speaker_from_label(&data.speaker_label);
    TranscriptSegment {
        session_id: SessionId::new(),
        track: Some(data.track),
        speaker,
        text: data.text.clone(),
        start_ms: data.start_ms,
        end_ms: data.end_ms,
        is_final,
        is_retranscribed: false,
    }
}

fn parse_speaker_from_label(label: &str) -> Option<u32> {
    if let Some(open) = label.find("(話者") {
        let rest = &label[open + "(話者".len()..];
        if let Some(close) = rest.find(')') {
            return rest[..close].parse::<u32>().ok().map(|n| n.saturating_sub(1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_speaker_from_label_extracts_speaker_index() {
        assert_eq!(parse_speaker_from_label("自分"), None);
        assert_eq!(parse_speaker_from_label("相手 (話者1)"), Some(0));
        assert_eq!(parse_speaker_from_label("相手 (話者2)"), Some(1));
        assert_eq!(parse_speaker_from_label("相手 (話者3)"), Some(2));
    }
}