//! Summary Consumer: subscribes to `transcription.{session_id}.segment.finalized`
//! and `transcription.{session_id}.utterance.ended` via the Local Broker, collects
//! finalized transcript turns, and generates a summary when the session ends.
//!
//! Also exposes `generate_summary_now()` for manual user-triggered summary generation
//! (the "要約を生成" button), which reads from `SessionStore` directly as a fallback.
//!
//! See `docs/decouple-summary-transcription.md` §6.2.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use credential_store::CredentialStore;
use local_broker::LocalBroker;
use recorder_domain::SessionId;
use session_store::{SessionStore, Summary};
use summarize::{Summarizer, TranscriptTurn};
use transcript_event::{
    EventEnvelope, ProtocolValidator, SummaryEvent, TranscriptEvent, UtteranceEndReason,
};

use crate::app_settings::AppSettings;
use crate::settings::SummaryProvider;
use crate::transcript;

/// Shared state for the Summary Consumer. Clone is cheap (Arc inside).
#[derive(Clone)]
pub struct SummaryConsumer {
    broker: LocalBroker,
    store: Arc<SessionStore>,
    credential_store: Arc<credential_store::FallbackCredentialStore>,
    app_settings: Arc<Mutex<AppSettings>>,
}

impl SummaryConsumer {
    pub fn new(
        broker: LocalBroker,
        store: Arc<SessionStore>,
        credential_store: Arc<credential_store::FallbackCredentialStore>,
        app_settings: Arc<Mutex<AppSettings>>,
    ) -> Self {
        Self { broker, store, credential_store, app_settings }
    }

    /// Spawns an auto-summary task for `session_id`. Returns immediately; the task
    /// runs until `UtteranceEnded(SessionEnd)` arrives, then generates the summary.
    /// Does nothing if the broker is not configured (broker is always configured
    /// in this app, but the parameter is kept for future IPC mode).
    pub fn spawn_auto_summary(&self, session_id: SessionId) {
        let this = self.clone();
        tokio::spawn(async move {
            this.run_auto_summary(session_id).await;
        });
    }

    async fn run_auto_summary(&self, session_id: SessionId) {
        let segment_subject = format!("transcription.{session_id}.segment.finalized");
        let utterance_subject = format!("transcription.{session_id}.utterance.ended");

        let mut segment_rx = self.broker.subscribe(&segment_subject);
        let mut utterance_rx = self.broker.subscribe(&utterance_subject);

        // 1. Load existing finalized segments from SessionStore (late-join support)
        let existing = self.store.list_transcript_segments(session_id).unwrap_or_default();
        let mut seen: HashSet<String> = HashSet::new();
        let mut turns: Vec<TranscriptTurn> = Vec::new();

        for seg in &existing {
            if seg.is_final {
                let sid = transcript_event::segment_id_for_segment(
                    seg.session_id, seg.track, seg.start_ms, seg.end_ms,
                );
                if seen.insert(sid) {
                    turns.push(TranscriptTurn {
                        speaker: Some(transcript::speaker_label(seg.track, seg.speaker)),
                        text: seg.text.clone(),
                    });
                }
            }
        }

        // 2. Initialize protocol validator
        let mut validator = ProtocolValidator::new(session_id);

        // 3. Subscribe to broker events
        loop {
            tokio::select! {
                result = segment_rx.recv() => {
                    match result {
                        Ok(payload) => {
                            let envelope: EventEnvelope<TranscriptEvent> = match serde_json::from_slice(&payload) {
                                Ok(e) => e,
                                Err(err) => {
                                    tracing::warn!(%err, "summary_consumer: failed to deserialize SegmentFinalized");
                                    continue;
                                }
                            };
                            if let TranscriptEvent::SegmentFinalized { data, .. } = &envelope.body {
                                // Validate the event against the protocol
                                if let Err(err) = validator.validate(&envelope.body) {
                                    tracing::warn!(%session_id, %err, "summary_consumer: protocol validation error");
                                }
                                if seen.insert(data.segment_id.clone()) {
                                    turns.push(TranscriptTurn {
                                        speaker: Some(data.speaker_label.clone()),
                                        text: data.text.clone(),
                                    });
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(n, %session_id, "summary_consumer: lagged, reloading from store");
                            recover_turns(&self.store, session_id, &mut seen, &mut turns);
                            segment_rx = self.broker.subscribe(&segment_subject);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                result = utterance_rx.recv() => {
                    match result {
                        Ok(payload) => {
                            let envelope: EventEnvelope<TranscriptEvent> = match serde_json::from_slice(&payload) {
                                Ok(e) => e,
                                Err(err) => {
                                    tracing::warn!(%err, "summary_consumer: failed to deserialize UtteranceEnded");
                                    continue;
                                }
                            };
                            if let TranscriptEvent::UtteranceEnded { reason: UtteranceEndReason::SessionEnd, .. } = &envelope.body {
                                // Validate the event against the protocol
                                if let Err(err) = validator.validate(&envelope.body) {
                                    tracing::warn!(%session_id, %err, "summary_consumer: protocol validation error");
                                }
                                break; // Session ended → generate summary
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            utterance_rx = self.broker.subscribe(&utterance_subject);
                        }
                    }
                }
            }
        }

        // 3. Generate summary
        if turns.is_empty() {
            return;
        }

        self.generate_and_publish(session_id, &turns).await;
    }

    /// Manual summary generation (user-triggered "要約を生成" button).
    /// Reads segments from `SessionStore` directly and generates the summary
    /// synchronously (within the async context). Publishes SummaryEvent results.
    pub async fn generate_summary_now(&self, session_id: SessionId) -> Result<String, String> {
        let segments = self.store.list_transcript_segments(session_id)
            .map_err(|e| format!("文字起こしの取得に失敗しました: {e}"))?;
        let turns = transcript::to_turns(&segments);
        if turns.is_empty() {
            return Err("要約対象の文字起こしがありません".to_string());
        }

        self.broker.publish(
            &transcript_event::summary_subject_for(
                &SummaryEvent::Started { session_id }, session_id,
            ),
            &EventEnvelope::new(session_id, SummaryEvent::Started { session_id }),
        ).ok();

        match self.summarize(&turns).await {
            Ok(text) => {
                Ok(text)
            }
            Err(e) => {
                self.broker.publish(
                    &transcript_event::summary_subject_for(
                        &SummaryEvent::Failed { session_id, error: e.clone() }, session_id,
                    ),
                    &EventEnvelope::new(session_id, SummaryEvent::Failed {
                        session_id,
                        error: e.clone(),
                    }),
                ).ok();
                Err(e)
            }
        }
    }

    async fn generate_and_publish(&self, session_id: SessionId, turns: &[TranscriptTurn]) {
        self.broker.publish(
            &transcript_event::summary_subject_for(
                &SummaryEvent::Started { session_id }, session_id,
            ),
            &EventEnvelope::new(session_id, SummaryEvent::Started { session_id }),
        ).ok();

        match self.summarize(turns).await {
            Ok(text) => {
                let provider_model = self.provider_model_string();
                self.store.insert_summary(&Summary {
                    session_id,
                    text: text.clone(),
                    provider_model: provider_model.clone(),
                    generated_at: chrono::Utc::now(),
                }).ok();

                self.broker.publish(
                    &transcript_event::summary_subject_for(
                        &SummaryEvent::Completed { session_id, text: text.clone(), provider_model: provider_model.clone() },
                        session_id,
                    ),
                    &EventEnvelope::new(session_id, SummaryEvent::Completed {
                        session_id,
                        text,
                        provider_model,
                    }),
                ).ok();
            }
            Err(e) => {
                self.broker.publish(
                    &transcript_event::summary_subject_for(
                        &SummaryEvent::Failed { session_id, error: e.to_string() }, session_id,
                    ),
                    &EventEnvelope::new(session_id, SummaryEvent::Failed {
                        session_id,
                        error: e.to_string(),
                    }),
                ).ok();
            }
        }
    }

    async fn summarize(&self, turns: &[TranscriptTurn]) -> Result<String, String> {
        let provider = self
            .credential_store
            .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT)
            .ok()
            .map(|key| SummaryProvider::from_key(&key))
            .unwrap_or(SummaryProvider::Claude);
        let model = self
            .credential_store
            .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_MODEL_ACCOUNT)
            .unwrap_or_else(|_| provider.default_model().to_string());

        let stored_credential = provider.api_key_account()
            .map(|account| self.credential_store.load(summarize::CREDENTIAL_SERVICE, account));
        if matches!(stored_credential, Some(Err(_))) {
            let message = if provider.is_vertex() {
                "設定画面でGoogle Vertex AIの認証情報を設定してください"
            } else {
                "設定画面でAPIキーを設定してください"
            };
            return Err(message.to_string());
        }

        let summary_template = self.app_settings.lock().unwrap().summary_template.clone();
        let options = crate::summary_template::summarize_options_for(model.clone(), summary_template);

        let summarizer: Result<Box<dyn Summarizer>, String> = if let Some(backend) = provider.cli_backend() {
            Ok(Box::new(summarize::CliSummarizer(backend)))
        } else if provider == SummaryProvider::ClaudeOAuth {
            Ok(Box::new(summarize::GenaiSummarizer(summarize::build_claude_oauth_client())))
        } else if provider.is_vertex() {
            let raw = stored_credential.and_then(Result::ok).unwrap_or_default();
            match serde_json::from_str::<summarize::VertexCredentials>(&raw) {
                Ok(credentials) => Ok(Box::new(summarize::GenaiSummarizer(summarize::build_vertex_client(credentials)))),
                Err(e) => Err(format!("認証情報の読み込みに失敗しました: {e}")),
            }
        } else if provider == SummaryProvider::Ollama {
            let base_url = self.app_settings.lock().unwrap().ollama_base_url.clone();
            Ok(Box::new(summarize::GenaiSummarizer(summarize::build_ollama_client(base_url))))
        } else if let Some(account) = provider.api_key_account() {
            let resolver = summarize::credential_store_auth_resolver(self.credential_store.clone(), account);
            let client = genai::Client::builder().with_auth_resolver(resolver).build();
            Ok(Box::new(summarize::GenaiSummarizer(client)))
        } else {
            let client = genai::Client::builder().build();
            Ok(Box::new(summarize::GenaiSummarizer(client)))
        };

        match summarizer {
            Ok(summarizer) => summarizer.summarize(turns, &options).await.map_err(|e| e.to_string()),
            Err(e) => Err(e),
        }
    }

    fn provider_model_string(&self) -> String {
        let provider = self
            .credential_store
            .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT)
            .ok()
            .map(|key| SummaryProvider::from_key(&key))
            .unwrap_or(SummaryProvider::Claude);
        let model = self
            .credential_store
            .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_MODEL_ACCOUNT)
            .unwrap_or_else(|_| provider.default_model().to_string());
        format!("{}/{}", provider.key(), model)
    }
}

fn recover_turns(
    store: &SessionStore,
    session_id: SessionId,
    seen: &mut HashSet<String>,
    turns: &mut Vec<TranscriptTurn>,
) {
    let segments = store.list_transcript_segments(session_id).unwrap_or_default();
    for seg in &segments {
        if seg.is_final {
            let sid = transcript_event::segment_id_for_segment(
                seg.session_id, seg.track, seg.start_ms, seg.end_ms,
            );
            if seen.insert(sid) {
                turns.push(TranscriptTurn {
                    speaker: Some(transcript::speaker_label(seg.track, seg.speaker)),
                    text: seg.text.clone(),
                });
            }
        }
    }
}