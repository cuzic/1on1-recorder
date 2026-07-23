//! Hint Consumer: subscribes to `hints.{session_id}.updated` (published by
//! `plugins/default/hint.rhai` via `rhai-engine`'s `publish_event`/
//! `handle_publish_event`) and updates a shared buffer the UI polls, the same
//! shape as `ui_consumer.rs`'s `TranscriptBuffer` for the transcript panel.
//!
//! Unlike `transcription.*` events (published by the Rust live-transcription
//! pipeline, wrapped in `transcript_event::EventEnvelope`), `hints.*.updated`
//! is published directly by a Rhai script's `publish_event(subject, data)` —
//! `handle_publish_event` (`crates/rhai-engine/src/dispatcher.rs`) serializes
//! the raw data map to JSON bytes with no envelope, so this subscribes to
//! plain `HintPayload` JSON instead of `EventEnvelope<T>`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use local_broker::LocalBroker;
use recorder_domain::SessionId;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct HintPayload {
    text: String,
    provider: String,
}

#[derive(Debug, Clone)]
pub struct HintState {
    pub text: String,
    pub provider: String,
    pub updated_at: Instant,
}

/// A shared slot the Hint Consumer writes to and the UI polling loop reads
/// from — parallel to `ui_consumer::TranscriptBuffer`, but a single latest
/// value rather than an accumulated list, since a hint supersedes the
/// previous one rather than appending to it.
#[derive(Clone, Default)]
pub struct HintBuffer {
    pub state: Arc<Mutex<Option<HintState>>>,
}

impl HintBuffer {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(None)) }
    }

    pub fn take(&self) -> Option<HintState> {
        self.state.lock().unwrap().clone()
    }
}

/// Spawns a background task that subscribes to `hints.{session_id}.updated`
/// and updates `buffer` with the latest hint. Returns immediately.
///
/// Ends itself on `session.{id}.stopped` (published by
/// `apps/desktop/src/recording.rs::stop`) — see `ui_consumer::spawn_ui_consumer`'s
/// doc comment for why waiting on the broker channel to close on its own
/// (the previous design here) never actually happens and leaks the task.
pub fn spawn_hint_consumer(broker: LocalBroker, session_id: SessionId, buffer: HintBuffer) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_hint_consumer(broker, session_id, buffer).await;
    })
}

async fn run_hint_consumer(broker: LocalBroker, session_id: SessionId, buffer: HintBuffer) {
    let subject = format!("hints.{session_id}.updated");
    let stop_subject = format!("session.{session_id}.stopped");
    let mut rx = broker.subscribe(&subject);
    let mut stop_rx = broker.subscribe(&stop_subject);

    loop {
        let payload = tokio::select! {
            result = rx.recv() => match result {
                Ok(p) => p,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, %session_id, "hint_consumer: lagged, skipping to latest");
                    rx = broker.subscribe(&subject);
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = stop_rx.recv() => break,
        };

        let parsed: HintPayload = match serde_json::from_slice(&payload) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%err, "hint_consumer: failed to deserialize hint payload");
                continue;
            }
        };

        *buffer.state.lock().unwrap() = Some(HintState { text: parsed.text, provider: parsed.provider, updated_at: Instant::now() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_payload_deserializes_from_the_shape_hint_rhai_publishes() {
        // Matches `publish_event(subject, #{ session_id: session_id, text: ...,
        // provider: ... })` in plugins/default/hint.rhai — `session_id` isn't
        // read here (the subject already encodes it), but must not break
        // deserialization by being present.
        let json = r#"{"session_id":"abc","text":"キャリア目標の進捗を聞いてみましょう。","provider":"cloudflare"}"#;
        let parsed: HintPayload = serde_json::from_str(json).expect("deserialize");
        assert_eq!(parsed.text, "キャリア目標の進捗を聞いてみましょう。");
        assert_eq!(parsed.provider, "cloudflare");
    }
}
