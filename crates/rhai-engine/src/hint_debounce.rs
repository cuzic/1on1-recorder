//! Silence-detection debounce timer for `plugins/default/hint.rhai`'s hint
//! generation, built on the `timed-fsm` crate.
//!
//! Rhai itself has no timer primitive (`hint.rhai` used to approximate a
//! debounce by checking elapsed time against `now_ms()` on every incoming
//! event — a *throttle*, not a debounce: it could still fire mid-conversation
//! rather than only after a quiet period). This module drives a real timer in
//! Rust instead: every relevant event resets a countdown, and only once
//! `debounce` has elapsed with no further events does `on_timeout` fire.
//!
//! # Known tradeoff: occasional stray early fire
//!
//! `TokioTimerRuntime`'s own docs note that `kill_timer` cannot retract a
//! timer notification already in flight on its internal channel — so a timer
//! about to fire and a `reset` (new event) can race, occasionally producing
//! one extra `on_timeout` right as activity resumes rather than only after
//! real silence. A generation-counter `TimerId` would close this, but that
//! trades it for exactly the anti-pattern `TokioTimerRuntime`'s docs warn
//! about (an unbounded `TimerId` source grows its internal handle map
//! without bound). Given the consequence here is "one hint generated
//! slightly early," not a correctness or resource issue, this module accepts
//! the rare stray fire rather than adopt that anti-pattern.

use std::time::Duration;

use timed_fsm::{Response, TimedStateMachine};

pub struct HintDebounce {
    debounce: Duration,
}

impl HintDebounce {
    pub fn new(debounce: Duration) -> Self {
        Self { debounce }
    }
}

impl TimedStateMachine for HintDebounce {
    type Event = ();
    type Action = ();
    type TimerId = ();

    fn on_event(&mut self, (): ()) -> Response<(), ()> {
        Response::consume().with_timer((), self.debounce)
    }

    fn on_timeout(&mut self, (): ()) -> Response<(), ()> {
        Response::emit_one(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_event_resets_rather_than_accumulates() {
        let mut fsm = HintDebounce::new(Duration::from_millis(50));
        let r1 = fsm.on_event(());
        assert!(r1.consumed);
        assert_eq!(r1.timers.len(), 1);
        assert!(r1.actions.is_empty(), "on_event must not fire immediately");

        let r2 = fsm.on_event(());
        assert_eq!(r2.timers.len(), 1, "a second event should (re)set the same single timer, not add a second one");
    }

    #[test]
    fn timeout_emits_exactly_one_action() {
        let mut fsm = HintDebounce::new(Duration::from_millis(50));
        let _ = fsm.on_event(());
        let response = fsm.on_timeout(());
        assert_eq!(response.actions.len(), 1);
    }
}
