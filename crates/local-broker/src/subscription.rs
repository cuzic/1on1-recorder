//! Shared plumbing for per-session consumers (`ui_consumer.rs`, `summary_consumer.rs`,
//! `hint_consumer.rs` in `apps/desktop`): a control-lane "stop" subject every such
//! consumer should watch alongside its data subject(s), and a `Receiver` wrapper that
//! removes the boilerplate of resubscribing after `RecvError::Lagged`.

use tokio::sync::broadcast;

use crate::LocalBroker;

/// The subject a per-session consumer should always additionally subscribe to
/// alongside its data subject(s), so it learns a session ended even if the
/// data-side "end" signal (e.g. a `TranscriptEvent::UtteranceEnded { reason:
/// SessionEnd }`) is delayed, lost to broadcast lag, or never published on this
/// platform at all. Published once, with an empty payload, by
/// `apps/desktop/src/recording.rs::stop` — presence of any message on this subject
/// means "stop", the payload itself carries no information.
pub fn session_stopped_subject(session_id: impl std::fmt::Display) -> String {
    format!("session.{session_id}.stopped")
}

/// What a [`Subscription::recv`] call produced.
#[derive(Debug)]
pub enum RecvOutcome {
    /// A message was received on the subject.
    Message(Vec<u8>),
    /// The receiver fell behind and `n` messages were dropped. The subscription has
    /// already resubscribed internally by the time this is returned — the caller
    /// only needs to react to the gap itself (e.g. reload from a durable store), not
    /// re-subscribe.
    Lagged(u64),
    /// The subject was closed (no publisher will ever send to it again).
    Closed,
}

/// One subject's subscription, with automatic resubscription on `Lagged` so callers
/// never have to remember to call `LocalBroker::subscribe` again themselves.
pub struct Subscription {
    broker: LocalBroker,
    subject: String,
    rx: broadcast::Receiver<Vec<u8>>,
}

impl Subscription {
    pub fn new(broker: LocalBroker, subject: impl Into<String>) -> Self {
        let subject = subject.into();
        let rx = broker.subscribe(&subject);
        Self { broker, subject, rx }
    }

    /// Receives the next message. On `Lagged`, resubscribes to `subject` before
    /// returning — the next call to `recv` reads from the fresh receiver.
    pub async fn recv(&mut self) -> RecvOutcome {
        match self.rx.recv().await {
            Ok(payload) => RecvOutcome::Message(payload),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                self.rx = self.broker.subscribe(&self.subject);
                RecvOutcome::Lagged(n)
            }
            Err(broadcast::error::RecvError::Closed) => RecvOutcome::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_stopped_subject_matches_the_convention_recording_rs_publishes_to() {
        assert_eq!(
            session_stopped_subject("abc123"),
            "session.abc123.stopped"
        );
    }

    #[tokio::test]
    async fn recv_yields_lagged_then_resubscribes_and_keeps_receiving() {
        let broker = LocalBroker::new();
        let mut sub = Subscription::new(broker.clone(), "test.subject");

        // A second, throwaway receiver keeps the subject's sender alive across the
        // `Lagged`-induced resubscribe below (the broker would otherwise drop the
        // sender once `sub`'s own receiver is replaced and briefly has no other
        // subscriber, per `LocalBroker`'s "lifecycle of a subject entry" doc comment).
        let _keep_alive = broker.subscribe("test.subject");

        // `DEFAULT_CAPACITY` is 256, so send far past it while `sub` never calls
        // `recv` to force a `Lagged` on the first read.
        for i in 0..300u32 {
            broker
                .publish("test.subject", &i)
                .expect("at least one subscriber (sub + keep_alive) is listening");
        }

        match sub.recv().await {
            RecvOutcome::Lagged(n) => assert!(n > 0, "expected a positive lag count"),
            other => panic!("expected Lagged, got {other:?}"),
        }

        // The resubscribe happened internally; publishing again and receiving should
        // now work normally.
        broker.publish("test.subject", &42u32).unwrap();
        match sub.recv().await {
            RecvOutcome::Message(payload) => {
                let value: u32 = serde_json::from_slice(&payload).unwrap();
                assert_eq!(value, 42);
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recv_yields_closed_once_the_subject_has_no_publisher_left() {
        let broker = LocalBroker::new();
        let mut sub = Subscription::new(broker.clone(), "test.subject");
        broker.unsubscribe("test.subject");
        match sub.recv().await {
            RecvOutcome::Closed => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }
}
