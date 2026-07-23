//! Rhai scripting engine for 1on1 Recorder plugins.

mod dispatcher;
mod engine;
mod hint_debounce;
mod hooks;
mod rag;
mod scope;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use credential_store::FallbackCredentialStore;
use dashmap::DashMap;
use local_broker::LocalBroker;
use recorder_domain::SessionId;
use rhai::Engine;
use session_store::SessionStore;
use timed_fsm::{TimedStateMachine, TimerCommand, TimerRuntime};

use crate::dispatcher::{AsyncCommand, async_worker};
use crate::scope::ScopeStore;

// See `rag.rs`'s own re-export comment — `apps/desktop/src/settings.rs` needs
// these two constants to save Cloudflare AI Search credentials under the exact
// `(service, account)` pair `rag::cloudflare::search` reads.
pub use rag::{CloudflareCredentials, CLOUDFLARE_AI_SEARCH_ACCOUNT, CLOUDFLARE_CREDENTIAL_SERVICE};

pub trait SettingsProvider: Send + Sync + 'static {
    fn get(&self, key: &str) -> Option<String>;
    fn selected_model(&self) -> String;
    fn session_metadata(&self, session_id: SessionId) -> rhai::Map;
}

#[derive(Clone)]
pub struct RhaiEngine {
    inner: Arc<RhaiEngineInner>,
}

struct RhaiEngineInner {
    engine: Engine,
    scripts: Vec<rhai::AST>,
    _worker: tokio::task::JoinHandle<()>,
    /// Live per-session Rhai `Scope`s, keyed by session — lets a trigger
    /// *outside* `run_session`'s own event loop (currently only
    /// `spawn_hint_debounce_driver`'s timer) share the exact same Scope
    /// `on_segment_finalized` has been accumulating into (e.g. `hint.rhai`'s
    /// `turns`), rather than getting a fresh, empty one the way
    /// `trigger_manual_summary` deliberately does. Entries are inserted by
    /// `spawn_session` and removed by `SessionScopeGuard` when that
    /// session's `run_session` task ends (whichever of a real
    /// `SessionEnd` broadcast or the `session.{id}.stopped` signal arrives
    /// first — see `run_session`).
    session_scopes: DashMap<SessionId, Arc<ScopeStore>>,
}

/// Removes `session_id`'s entry from `session_scopes` when dropped — held as
/// a local in `run_session`'s async body so the entry is cleaned up whether
/// that task ends via its own `break` or (in principle; nothing in this
/// workspace aborts it today) external cancellation.
struct SessionScopeGuard {
    inner: Arc<RhaiEngineInner>,
    session_id: SessionId,
}

impl Drop for SessionScopeGuard {
    fn drop(&mut self) {
        self.inner.session_scopes.remove(&self.session_id);
    }
}

impl RhaiEngine {
    pub fn new(
        broker: LocalBroker,
        store: Arc<SessionStore>,
        credential_store: Arc<FallbackCredentialStore>,
        settings: Arc<dyn SettingsProvider>,
    ) -> Self {
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();

        let worker = tokio::spawn(async_worker(
            command_rx,
            broker,
            store,
            credential_store,
            settings,
        ));

        let mut engine = Engine::new();
        engine.set_max_operations(100_000);
        // Rhai's default max expression-nesting depth is lower in debug builds
        // (32) than release (64) — a plain nested `fn { if { for { if { ... } } } }`
        // shape like `summary.rhai`/`hint.rhai`'s `on_session_end`/debounce logic
        // already exceeds 32, so a script that compiles fine in a release build
        // could fail to load in a debug build (confirmed: `summary.rhai` as
        // shipped failed to compile under `cargo test`'s debug profile with
        // "Expression exceeds maximum complexity" before this line was added).
        // Pinning both limits to a fixed, generous value makes plugin loading
        // behave identically regardless of build profile.
        engine.set_max_expr_depths(128, 128);

        let tx = command_tx.clone();
        engine.register_fn("call_async", move |name: &str, args: rhai::Map| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
            let (tx_reply, rx_reply) = std::sync::mpsc::channel();
            tx.send(AsyncCommand { name: name.to_string(), args, reply: tx_reply })
                .map_err(|e| e.to_string())?;
            rx_reply.recv().map_err(|e| e.to_string())?.map_err(|e| -> Box<rhai::EvalAltResult> { e.into() })
        });
        engine.register_fn("log_info", |msg: &str| tracing::info!("[rhai] {msg}"));
        engine.register_fn("log_warn", |msg: &str| tracing::warn!("[rhai] {msg}"));
        engine.register_fn("log_error", |msg: &str| tracing::error!("[rhai] {msg}"));
        // Debounce/throttle timing for plugins like `hint.rhai` — synchronous and
        // non-I/O, so (unlike `call_async`'s commands) this never blocks on the
        // async worker's mpsc round trip.
        engine.register_fn("now_ms", || -> i64 {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
        });

        Self {
            inner: Arc::new(RhaiEngineInner {
                engine,
                scripts: Vec::new(),
                _worker: worker,
                session_scopes: DashMap::new(),
            }),
        }
    }

    pub fn load_plugins(&mut self, plugin_dir: &Path) -> Result<usize, RhaiError> {
        let inner = Arc::get_mut(&mut self.inner)
            .ok_or(RhaiError::Io(std::io::Error::other("engine is shared")))?;
        let compiled = engine::load_plugins(&mut inner.engine, plugin_dir)?;
        let count = compiled.len();
        inner.scripts = compiled.into_iter().map(|c| c.ast).collect();
        Ok(count)
    }

    /// Spawns a background task that subscribes to the broker and dispatches
    /// events to all loaded scripts, and handles `on_session_end` cleanup.
    ///
    /// Does *not* call an `on_session_start` hook — despite both
    /// `plugins/default/summary.rhai` and `hint.rhai` defining one, nothing
    /// in this workspace ever invokes it (`hooks::dispatch` has no branch
    /// for it). This is harmless today only because
    /// `ScopeStore::start_session_asts` already re-runs each script's
    /// top-level `let turns = []; let seen = #{};` fresh into every new
    /// session's `Scope`, which is all either plugin's `on_session_start`
    /// would have done anyway. A plugin relying on `on_session_start` for
    /// anything *beyond* that would silently never see it fire.
    ///
    /// Returns the task's `JoinHandle` — the caller doesn't need to hold onto
    /// it for cleanup purposes (this task ends itself; see the
    /// `session.{id}.stopped` handling in `run_session`), but callers that
    /// want to know when the session's Rhai-side work has fully wound down
    /// can await it.
    pub fn spawn_session(&self, broker: &LocalBroker, session_id: SessionId) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        let broker = broker.clone();
        let scopes = Arc::new(ScopeStore::new());
        scopes.start_session_asts(&this.inner.engine, &this.inner.scripts, session_id);
        this.inner.session_scopes.insert(session_id, scopes.clone());

        tokio::spawn(async move {
            let _guard = SessionScopeGuard { inner: this.inner.clone(), session_id };
            this.run_session(broker, session_id, scopes).await;
        })
    }

    async fn run_session(&self, broker: LocalBroker, session_id: SessionId, scopes: Arc<ScopeStore>) {
        let seg_subject = format!("transcription.{session_id}.segment.updated");
        let fin_subject = format!("transcription.{session_id}.segment.finalized");
        let utt_subject = format!("transcription.{session_id}.utterance.ended");
        // Published by `apps/desktop/src/recording.rs::stop` (all platforms)
        // once the recording has actually stopped — unlike
        // `UtteranceEnded(SessionEnd)` above, which only
        // `app_service::live_transcription` (Windows only) ever publishes.
        // Without this branch, this task never ends on macOS/dev-fallback
        // builds (no leak *and* no auto-summary: `SessionEnd` never arrives
        // there either way). Whichever of the two arrives first finalizes
        // the session and breaks; the other, if it still arrives afterward,
        // is simply never observed once this task has already exited.
        let stop_subject = format!("session.{session_id}.stopped");

        let mut seg_rx = broker.subscribe(&seg_subject);
        let mut fin_rx = broker.subscribe(&fin_subject);
        let mut utt_rx = broker.subscribe(&utt_subject);
        let mut stop_rx = broker.subscribe(&stop_subject);

        loop {
            tokio::select! {
                result = seg_rx.recv() => {
                    if let Ok(payload) = result {
                        if let Ok(env) = serde_json::from_slice::<transcript_event::EventEnvelope<transcript_event::TranscriptEvent>>(&payload) {
                            self.dispatch_blocking(&scopes, env.body).await;
                        }
                    } else { seg_rx = broker.subscribe(&seg_subject); }
                }
                result = fin_rx.recv() => {
                    if let Ok(payload) = result {
                        if let Ok(env) = serde_json::from_slice::<transcript_event::EventEnvelope<transcript_event::TranscriptEvent>>(&payload) {
                            self.dispatch_blocking(&scopes, env.body).await;
                        }
                    } else { fin_rx = broker.subscribe(&fin_subject); }
                }
                result = utt_rx.recv() => {
                    match result {
                        Ok(payload) => {
                            if let Ok(env) = serde_json::from_slice::<transcript_event::EventEnvelope<transcript_event::TranscriptEvent>>(&payload) {
                                let is_session_end = matches!(&env.body, transcript_event::TranscriptEvent::UtteranceEnded {
                                    reason: transcript_event::UtteranceEndReason::SessionEnd, ..
                                });
                                self.dispatch_blocking(&scopes, env.body).await;
                                if is_session_end { break; }
                            }
                        }
                        Err(_) => break,
                    }
                }
                result = stop_rx.recv() => {
                    if result.is_ok() {
                        self.session_end_blocking(&scopes, session_id).await;
                    }
                    break;
                }
            }
        }
    }

    /// Runs `hooks::dispatch` on the blocking thread pool rather than inline
    /// on this (async) task — `call_fn`'s bridge to `call_async` blocks
    /// synchronously on a `std::sync::mpsc::Receiver::recv()` for however
    /// long the hook's own RAG/LLM call takes, which would otherwise pin a
    /// tokio async worker thread for that whole duration.
    async fn dispatch_blocking(&self, scopes: &Arc<ScopeStore>, event: transcript_event::TranscriptEvent) {
        let this = self.clone();
        let scopes = scopes.clone();
        let _ = tokio::task::spawn_blocking(move || {
            hooks::dispatch(&this.inner.engine, &this.inner.scripts, &scopes, &event);
        })
        .await;
    }

    /// Same `spawn_blocking` rationale as `dispatch_blocking`, for the
    /// `session.{id}.stopped`-triggered `on_session_end` call.
    async fn session_end_blocking(&self, scopes: &Arc<ScopeStore>, session_id: SessionId) {
        let this = self.clone();
        let scopes = scopes.clone();
        let _ = tokio::task::spawn_blocking(move || {
            hooks::trigger_session_end(&this.inner.engine, &this.inner.scripts, &scopes, session_id);
        })
        .await;
    }

    /// Triggers manual summary for `session_id` using the broker events.
    /// The script must define `on_manual_summary(data)`.
    pub fn trigger_manual_summary(&self, _broker: &LocalBroker, session_id: SessionId) {
        let this = self.clone();
        let scopes = Arc::new(ScopeStore::new());
        scopes.start_session_asts(&this.inner.engine, &this.inner.scripts, session_id);

        tokio::spawn(async move {
            // Give the scope a moment, then trigger
            hooks::trigger_manual_summary(&this.inner.engine, &this.inner.scripts, &scopes, session_id);
        });
    }

    /// Drives a real, non-blocking silence-detection debounce
    /// (`hint_debounce::HintDebounce`, via the `timed-fsm` crate) for
    /// `plugins/default/hint.rhai`'s `on_hint_timeout()` hook — see that
    /// module's doc comment for why Rhai itself can't implement this
    /// (no timer primitive). Subscribes to
    /// `transcription.{id}.segment.finalized` (any finalized segment resets
    /// the debounce window) and `session.{id}.stopped` (ends the driver,
    /// same subject `run_session` reacts to).
    ///
    /// This method doesn't check `AppSettings::hint_enabled` itself — the
    /// caller (`apps/desktop`'s `actions::start_recording`) only calls it
    /// when hints are enabled, so an unconfigured/disabled install never
    /// even subscribes to these subjects.
    pub fn spawn_hint_debounce_driver(&self, broker: &LocalBroker, session_id: SessionId, debounce: Duration) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        let broker = broker.clone();

        tokio::spawn(async move {
            let fin_subject = format!("transcription.{session_id}.segment.finalized");
            let stop_subject = format!("session.{session_id}.stopped");
            let mut fin_rx = broker.subscribe(&fin_subject);
            let mut stop_rx = broker.subscribe(&stop_subject);

            let mut fsm = hint_debounce::HintDebounce::new(debounce);
            let mut timers = timed_fsm::tokio_support::TokioTimerRuntime::new();

            loop {
                let response = tokio::select! {
                    result = fin_rx.recv() => {
                        match result {
                            Ok(_) => fsm.on_event(()),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                fin_rx = broker.subscribe(&fin_subject);
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    Some(timer_id) = timers.recv() => fsm.on_timeout(timer_id),
                    _result = stop_rx.recv() => break,
                };

                for cmd in &response.timers {
                    match *cmd {
                        TimerCommand::Set { id, duration } => timers.set_timer(id, duration),
                        TimerCommand::Kill { id } => timers.kill_timer(id),
                    }
                }

                if !response.actions.is_empty() {
                    // Looked up fresh each time rather than cached: the
                    // session may have ended (and `SessionScopeGuard`
                    // removed the entry) in the brief window between this
                    // driver's own `stop_rx` branch not having fired yet and
                    // the debounce timer firing — `None` here just means
                    // "nothing to do," not an error, since the Arc clone
                    // this driver would have held is otherwise no different
                    // from one fetched right before use.
                    if let Some(scopes) = this.inner.session_scopes.get(&session_id).map(|entry| entry.clone()) {
                        let this = this.clone();
                        tokio::task::spawn_blocking(move || {
                            hooks::trigger_hint_timeout(&this.inner.engine, &this.inner.scripts, &scopes, session_id);
                        });
                    }
                }
            }
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RhaiError {
    #[error("failed to read plugin directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to compile plugin '{path}': {error}")]
    Compile { path: String, error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_is_registered_and_advances() {
        let mut engine = rhai::Engine::new();
        engine.register_fn("now_ms", || -> i64 {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
        });
        let first: i64 = engine.eval("now_ms()").expect("call now_ms");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second: i64 = engine.eval("now_ms()").expect("call now_ms again");
        assert!(second >= first, "now_ms() should be non-decreasing across calls");
        assert!(first > 0, "now_ms() should return a real epoch-millis timestamp, not 0");
    }

    struct TestSettings;
    impl SettingsProvider for TestSettings {
        fn get(&self, _key: &str) -> Option<String> {
            None
        }
        fn selected_model(&self) -> String {
            "test-model".to_string()
        }
        fn session_metadata(&self, _session_id: SessionId) -> rhai::Map {
            rhai::Map::new()
        }
    }

    /// Real, non-network end-to-end test of the entire redesign this session
    /// added: `spawn_session`'s `session_scopes` sharing, `spawn_hint_debounce_driver`'s
    /// `timed-fsm`-based silence debounce (not a throttle — rapid events must
    /// NOT trigger a timeout), and both tasks ending themselves on the
    /// `session.{id}.stopped` signal (`apps/desktop/src/recording.rs::stop`'s
    /// counterpart) rather than only on the Windows-only real `SessionEnd`
    /// broadcast. Uses a minimal custom plugin pair (not the real
    /// `plugins/default/`) that only ever calls `publish_event` — the real
    /// `hint.rhai`/`summary.rhai` call `rag_search`/`ai_summarize`, which
    /// would attempt real network requests this sandbox can't make; this test
    /// is about the Rust-side wiring, not those hooks' own content.
    #[tokio::test(flavor = "multi_thread")]
    async fn hint_and_session_lifecycle_end_to_end() {
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join("default")).unwrap();
        std::fs::write(
            plugin_dir.path().join("default/std.rhai"),
            r#"fn publish_event(subject, data) { call_async("publish_event", #{ subject: subject, data: data }) }"#,
        )
        .unwrap();
        std::fs::write(
            plugin_dir.path().join("default/probe.rhai"),
            r#"
            import "std" as std;
            let turns = [];
            let hint_count = 0;

            fn on_segment_finalized(data) {
                turns.push(data.segment_id);
                std::publish_event(`probe.${session_id}.segment_seen`, #{});
            }
            fn on_hint_timeout() {
                // Publishing `turns` here — accumulated by `on_segment_finalized`,
                // which `run_session`'s own dispatch calls — is what actually
                // proves `spawn_hint_debounce_driver`'s *separate* task looked up
                // the *same* `Scope` via `RhaiEngineInner::session_scopes` rather
                // than some fresh/empty one: if the lookup were wrong, `turns`
                // would be empty here regardless of what `on_segment_finalized`
                // saw.
                hint_count += 1;
                std::publish_event(`probe.${session_id}.hint_timeout`, #{ count: hint_count, turns: turns });
            }
            fn on_session_end() {
                std::publish_event(`probe.${session_id}.session_ended`, #{});
            }
            fn on_load() {}
            "#,
        )
        .unwrap();

        // Built inline (rather than via `test_engine`) so this test keeps a
        // handle to the exact `LocalBroker` the engine's `call_async`
        // dispatcher (and therefore `publish_event`) was constructed with —
        // needed to subscribe to the `probe.*` subjects below.
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(SessionStore::open(&tmp.path().join("sessions.db")).unwrap());
        let credential_store = Arc::new(FallbackCredentialStore::new(tmp.path().join("credentials")).unwrap());
        let broker = LocalBroker::new();
        let mut engine = RhaiEngine::new(broker.clone(), store, credential_store, Arc::new(TestSettings));
        engine.load_plugins(plugin_dir.path()).expect("plugins should load");

        let session_id = SessionId::new();
        let debounce = Duration::from_millis(60);

        let mut segment_seen_rx = broker.subscribe(&format!("probe.{session_id}.segment_seen"));
        let mut hint_timeout_rx = broker.subscribe(&format!("probe.{session_id}.hint_timeout"));
        let mut session_ended_rx = broker.subscribe(&format!("probe.{session_id}.session_ended"));

        let session_handle = engine.spawn_session(&broker, session_id);
        let debounce_handle = engine.spawn_hint_debounce_driver(&broker, session_id, debounce);
        // Give both freshly spawned tasks a moment to actually reach their
        // `broker.subscribe(...)` calls before this test starts publishing —
        // otherwise the first `publish` below can race ahead of them and see
        // `BrokerError::NoSubscribers`.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let finalized_event = |seg: &str| transcript_event::EventEnvelope::new(
            session_id,
            transcript_event::TranscriptEvent::SegmentFinalized {
                session_id,
                data: transcript_event::SegmentData {
                    segment_id: seg.to_string(),
                    revision: 1,
                    text: "hello".to_string(),
                    speaker_label: "self".to_string(),
                    track: recorder_domain::TrackKind::SelfMic,
                    start_ms: Some(0),
                    end_ms: Some(1000),
                },
            },
        );
        let fin_subject = format!("transcription.{session_id}.segment.finalized");

        // Rapid-fire three events, each well inside the debounce window —
        // this is the behavior that distinguishes a real silence-debounce
        // from the old Rhai-side throttle: none of these should trigger
        // `on_hint_timeout` yet.
        for seg in ["seg1", "seg2", "seg3"] {
            broker.publish(&fin_subject, &finalized_event(seg)).unwrap();
            tokio::time::sleep(debounce / 3).await;
        }
        for seg in ["seg1", "seg2", "seg3"] {
            segment_seen_rx.recv().await.expect("on_segment_finalized should have fired for each");
            let _ = seg;
        }
        assert!(hint_timeout_rx.try_recv().is_err(), "rapid events must not trigger a hint timeout — that would be a throttle, not a debounce");

        // Now go quiet — only after the full debounce window elapses with no
        // further activity should the timeout fire, exactly once.
        let payload = tokio::time::timeout(debounce * 3, hint_timeout_rx.recv()).await.expect("hint_timeout should fire after the quiet period").unwrap();
        let data: serde_json::Value = serde_json::from_slice(&payload).expect("hint_timeout payload should be valid JSON");
        assert_eq!(data["count"].as_i64(), Some(1));
        // The real assertion for `session_scopes` sharing: `on_hint_timeout`
        // runs in `spawn_hint_debounce_driver`'s own task, entirely separate
        // from `run_session`'s task that dispatched the three
        // `on_segment_finalized` calls above — if the driver had looked up a
        // fresh/wrong `Scope` instead of the shared one via
        // `RhaiEngineInner::session_scopes`, `turns` would be `[]` here.
        assert_eq!(data["turns"], serde_json::json!(["seg1", "seg2", "seg3"]), "on_hint_timeout must see the same `turns` on_segment_finalized accumulated — proves session_scopes sharing, not just that a hint fired");
        assert!(hint_timeout_rx.try_recv().is_err(), "should have fired exactly once, not repeatedly");

        // Now signal the session actually stopped — proves `run_session`
        // finalizes (`on_session_end`) and exits on this signal alone,
        // without ever needing the Windows-only real `SessionEnd` broadcast.
        broker.publish_bytes(&format!("session.{session_id}.stopped"), Vec::new()).unwrap();
        session_ended_rx.recv().await.expect("on_session_end should fire on the explicit stop signal");

        tokio::time::timeout(Duration::from_secs(2), session_handle).await.expect("run_session should exit promptly after the stop signal").unwrap();
        tokio::time::timeout(Duration::from_secs(2), debounce_handle).await.expect("the debounce driver should exit promptly after the stop signal").unwrap();
    }
}