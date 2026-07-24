//! In-process pub/sub broker for decoupling transcription events from downstream
//! consumers (see `docs/decouple-summary-transcription.md`).
//!
//! Phase 1 uses `tokio::broadcast` channels keyed by subject string. Events are
//! serialized to JSON bytes at publish time; consumers deserialize on their side.
//! Phase 2 will add IPC transport via the `interprocess` crate without changing
//! the public API.
//!
//! # Lifecycle of a subject entry
//!
//! - `subscribe()` creates a `broadcast::Sender` for the subject if one doesn't
//!   exist yet, then returns a new `Receiver`.
//! - `publish()` serializes and sends. If the sender has no receivers, the entry
//!   is cleaned up lazily.
//! - When all receivers for a subject are dropped, the sender is removed from the
//!   map on the next `publish()` or `subscribe()` call to that subject.

mod subscription;

pub use subscription::{session_stopped_subject, RecvOutcome, Subscription};

use std::sync::Arc;

use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::broadcast;

const DEFAULT_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("failed to serialize event: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("no subscribers for subject '{0}'")]
    NoSubscribers(String),
}

/// An in-process pub/sub broker. Clone is cheap (Arc inside).
#[derive(Clone)]
pub struct LocalBroker {
    subjects: Arc<DashMap<String, broadcast::Sender<Vec<u8>>>>,
}

impl LocalBroker {
    pub fn new() -> Self {
        Self {
            subjects: Arc::new(DashMap::new()),
        }
    }

    /// Subscribes to a subject. If no sender exists for this subject yet, one is
    /// created with [`DEFAULT_CAPACITY`]. The returned receiver yields raw JSON
    /// bytes — the caller is responsible for deserializing into the expected
    /// event type.
    pub fn subscribe(&self, subject: &str) -> broadcast::Receiver<Vec<u8>> {
        let sender = self
            .subjects
            .entry(subject.to_string())
            .or_insert_with(|| broadcast::channel(DEFAULT_CAPACITY).0);
        sender.subscribe()
    }

    /// Publishes a serialized event to all subscribers of `subject`.
    ///
    /// Returns `Ok(())` if at least one subscriber received the event, or
    /// `BrokerError::NoSubscribers` if no one is listening.
    pub fn publish<T: Serialize>(&self, subject: &str, event: &T) -> Result<(), BrokerError> {
        let payload = serde_json::to_vec(event)?;

        // Use `get` + `send` instead of `entry` to avoid holding a write lock
        // while serializing — the serialization is the expensive part.
        let sender = match self.subjects.get(subject) {
            Some(s) => s.clone(),
            None => return Err(BrokerError::NoSubscribers(subject.to_string())),
        };

        match sender.send(payload) {
            Ok(_) => Ok(()),
            Err(broadcast::error::SendError(_)) => {
                // No receivers left — clean up the stale entry.
                self.subjects.remove(subject);
                Err(BrokerError::NoSubscribers(subject.to_string()))
            }
        }
    }

    /// Removes a subject's sender, if it exists. Active receivers will get
    /// `RecvError::Closed` on their next `recv()` call.
    pub fn unsubscribe(&self, subject: &str) {
        self.subjects.remove(subject);
    }

    /// Publishes raw bytes to all subscribers of `subject`. Used when the
    /// payload is already serialized (e.g., from Rhai scripts).
    pub fn publish_bytes(&self, subject: &str, payload: Vec<u8>) -> Result<(), BrokerError> {
        let sender = match self.subjects.get(subject) {
            Some(s) => s.clone(),
            None => return Err(BrokerError::NoSubscribers(subject.to_string())),
        };
        match sender.send(payload) {
            Ok(_) => Ok(()),
            Err(broadcast::error::SendError(_)) => {
                self.subjects.remove(subject);
                Err(BrokerError::NoSubscribers(subject.to_string()))
            }
        }
    }
}

impl Default for LocalBroker {
    fn default() -> Self {
        Self::new()
    }
}