//! Rhai scripting engine for 1on1 Recorder plugins.

mod dispatcher;
mod engine;
mod hooks;
mod rag;
mod scope;

use std::path::Path;
use std::sync::Arc;

use credential_store::FallbackCredentialStore;
use local_broker::LocalBroker;
use recorder_domain::SessionId;
use rhai::Engine;
use session_store::SessionStore;

use crate::dispatcher::{AsyncCommand, async_worker};
use crate::scope::ScopeStore;

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

        Self {
            inner: Arc::new(RhaiEngineInner {
                engine,
                scripts: Vec::new(),
                _worker: worker,
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
    /// events to all loaded scripts. Also calls `on_session_start` and handles
    /// `on_session_end` cleanup.
    pub fn spawn_session(&self, broker: &LocalBroker, session_id: SessionId) {
        let this = self.clone();
        let broker = broker.clone();
        let scopes = Arc::new(ScopeStore::new());
        scopes.start_session_asts(&this.inner.scripts, session_id);

        tokio::spawn(async move {
            this.run_session(broker, session_id, scopes).await;
        });
    }

    async fn run_session(&self, broker: LocalBroker, session_id: SessionId, scopes: Arc<ScopeStore>) {
        let seg_subject = format!("transcription.{session_id}.segment.updated");
        let fin_subject = format!("transcription.{session_id}.segment.finalized");
        let utt_subject = format!("transcription.{session_id}.utterance.ended");

        let mut seg_rx = broker.subscribe(&seg_subject);
        let mut fin_rx = broker.subscribe(&fin_subject);
        let mut utt_rx = broker.subscribe(&utt_subject);

        loop {
            tokio::select! {
                result = seg_rx.recv() => {
                    if let Ok(payload) = result {
                        if let Ok(env) = serde_json::from_slice::<transcript_event::EventEnvelope<transcript_event::TranscriptEvent>>(&payload) {
                            hooks::dispatch(&self.inner.engine, &self.inner.scripts, &scopes, &env.body);
                        }
                    } else { seg_rx = broker.subscribe(&seg_subject); }
                }
                result = fin_rx.recv() => {
                    if let Ok(payload) = result {
                        if let Ok(env) = serde_json::from_slice::<transcript_event::EventEnvelope<transcript_event::TranscriptEvent>>(&payload) {
                            hooks::dispatch(&self.inner.engine, &self.inner.scripts, &scopes, &env.body);
                        }
                    } else { fin_rx = broker.subscribe(&fin_subject); }
                }
                result = utt_rx.recv() => {
                    match result {
                        Ok(payload) => {
                            if let Ok(env) = serde_json::from_slice::<transcript_event::EventEnvelope<transcript_event::TranscriptEvent>>(&payload) {
                                hooks::dispatch(&self.inner.engine, &self.inner.scripts, &scopes, &env.body);
                                if matches!(&env.body, transcript_event::TranscriptEvent::UtteranceEnded {
                                    reason: transcript_event::UtteranceEndReason::SessionEnd, ..
                                }) { break; }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }

    /// Triggers manual summary for `session_id` using the broker events.
    /// The script must define `on_manual_summary(data)`.
    pub fn trigger_manual_summary(&self, _broker: &LocalBroker, session_id: SessionId) {
        let this = self.clone();
        let scopes = Arc::new(ScopeStore::new());
        scopes.start_session_asts(&this.inner.scripts, session_id);

        tokio::spawn(async move {
            // Give the scope a moment, then trigger
            hooks::trigger_manual_summary(&this.inner.engine, &this.inner.scripts, &scopes, session_id);
        });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RhaiError {
    #[error("failed to read plugin directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to compile plugin '{path}': {error}")]
    Compile { path: String, error: String },
}