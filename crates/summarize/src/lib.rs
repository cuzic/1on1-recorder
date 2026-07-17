//! Thin wrapper over the `genai` crate (jeremychone/rust-genai) for summarizing
//! diarized meeting transcripts. `genai`'s `Client` already covers provider
//! switching (Anthropic/OpenAI/Gemini/etc.) via a model string, so this crate adds
//! only two things: the "diarized turns in, summary text out" shape, and an
//! `AuthResolver` that pulls provider API keys from `credential-store` instead of
//! environment variables.
//!
//! Like `stt-api`, this crate has no dependency on this project's session/track
//! types — [`TranscriptTurn`] is a minimal, crate-local type. Mapping a project
//! transcript type into it is the caller's job.

use std::sync::Arc;

use credential_store::CredentialStore;
use genai::chat::{ChatMessage, ChatRequest};
use genai::resolver::{AuthData, AuthResolver};
use genai::{Client, ModelIden};

/// OS keyring service name under which all provider API keys used by this crate are
/// stored (design.md §12.4). Account names are per-provider, e.g. [`CLAUDE_API_KEY_ACCOUNT`].
pub const CREDENTIAL_SERVICE: &str = "1on1-recorder";

/// Convenience account name for the Anthropic API key.
pub const CLAUDE_API_KEY_ACCOUNT: &str = "claude-api-key";

/// Convenience account name for the OpenAI API key.
pub const OPENAI_API_KEY_ACCOUNT: &str = "openai-api-key";

/// Convenience account name for the Gemini API key.
pub const GEMINI_API_KEY_ACCOUNT: &str = "gemini-api-key";

/// Convenience account name for the Groq API key.
pub const GROQ_API_KEY_ACCOUNT: &str = "groq-api-key";

/// Convenience account name for the DeepSeek API key.
pub const DEEPSEEK_API_KEY_ACCOUNT: &str = "deepseek-api-key";

/// Convenience account name for the xAI (Grok) API key.
pub const XAI_API_KEY_ACCOUNT: &str = "xai-api-key";

/// Account name under which the user's currently selected summary provider (e.g.
/// `"claude"` or `"openai"`) is stored — written by the settings UI, read by later
/// summary-triggering code (#38), so both agree on where "which provider is active"
/// lives without either depending on the other's crate.
pub const SELECTED_PROVIDER_ACCOUNT: &str = "summary-selected-provider";

/// Account name under which the user's currently selected [`SummarizeOptions::model`]
/// string (e.g. `"claude-sonnet-4-5"`) is stored, paired with
/// [`SELECTED_PROVIDER_ACCOUNT`].
pub const SELECTED_MODEL_ACCOUNT: &str = "summary-selected-model";

/// One speaker turn in a diarized transcript.
#[derive(Debug, Clone)]
pub struct TranscriptTurn {
    pub speaker: Option<String>,
    pub text: String,
}

/// Per-call configuration. `#[non_exhaustive]` + builder, so new fields (e.g.
/// temperature, max tokens) can be added later without breaking callers — same
/// pattern as `stt-api`'s `SttSessionConfig`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SummarizeOptions {
    /// A `genai` model spec string, e.g. `"claude-sonnet-4-5"` or `"gpt-4o-mini"`.
    /// Selects both the provider and the model; chosen by the caller (eventually
    /// from a UI picker), never inferred by this crate.
    pub model: String,
    /// Overrides [`DEFAULT_SYSTEM_PROMPT`] when set.
    pub system_prompt: Option<String>,
}

impl SummarizeOptions {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system_prompt: None,
        }
    }

    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(system_prompt.into());
        self
    }
}

pub const DEFAULT_SYSTEM_PROMPT: &str =
    "You summarize 1-on-1 meeting transcripts. Produce a concise summary covering key discussion points, decisions, and action items.";

#[derive(Debug, thiserror::Error)]
pub enum SummarizeError {
    #[error("genai request failed: {0}")]
    Provider(#[from] genai::Error),
    #[error("provider returned an empty summary")]
    EmptyResponse,
}

/// Summarizes `turns` using the model named in `options.model`. `client` is the
/// caller's `genai::Client` (see [`credential_store_auth_resolver`] for wiring one
/// up against the OS keyring) so one client can be reused across calls.
pub async fn summarize(
    client: &Client,
    turns: &[TranscriptTurn],
    options: &SummarizeOptions,
) -> Result<String, SummarizeError> {
    let chat_req = build_chat_request(turns, options);
    let chat_res = client
        .exec_chat(options.model.as_str(), chat_req, None)
        .await?;
    chat_res
        .first_text()
        .map(str::to_string)
        .ok_or(SummarizeError::EmptyResponse)
}

fn build_chat_request(turns: &[TranscriptTurn], options: &SummarizeOptions) -> ChatRequest {
    let system = options
        .system_prompt
        .as_deref()
        .unwrap_or(DEFAULT_SYSTEM_PROMPT);
    ChatRequest::new(vec![
        ChatMessage::system(system),
        ChatMessage::user(render_transcript(turns)),
    ])
}

fn render_transcript(turns: &[TranscriptTurn]) -> String {
    turns
        .iter()
        .map(|turn| match &turn.speaker {
            Some(speaker) => format!("{speaker}: {}", turn.text),
            None => turn.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Builds a `genai` `AuthResolver` that loads the API key from `store` at
/// `(CREDENTIAL_SERVICE, account)` (e.g. `account` = [`CLAUDE_API_KEY_ACCOUNT`]).
/// Pass the result to `genai::Client::builder().with_auth_resolver(..)`.
///
/// The resolver ignores `ModelIden` and always returns the same account's key —
/// callers targeting multiple providers build one `AuthResolver` (and `Client`) per
/// provider rather than relying on `ModelIden`-based dispatch.
pub fn credential_store_auth_resolver<S>(store: Arc<S>, account: impl Into<String>) -> AuthResolver
where
    S: CredentialStore + Send + Sync + 'static,
{
    let account = account.into();
    AuthResolver::from_resolver_fn(move |_model_iden: ModelIden| {
        let secret = store
            .load(CREDENTIAL_SERVICE, &account)
            .map_err(|e| genai::resolver::Error::Custom(format!("credential store: {e}")))?;
        Ok(Some(AuthData::from_single(secret)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_transcript_labels_known_speakers() {
        let turns = vec![
            TranscriptTurn {
                speaker: Some("Alice".to_string()),
                text: "Hi there".to_string(),
            },
            TranscriptTurn {
                speaker: Some("Bob".to_string()),
                text: "Hello".to_string(),
            },
        ];
        assert_eq!(render_transcript(&turns), "Alice: Hi there\nBob: Hello");
    }

    #[test]
    fn render_transcript_omits_label_for_unknown_speaker() {
        let turns = vec![TranscriptTurn {
            speaker: None,
            text: "unattributed text".to_string(),
        }];
        assert_eq!(render_transcript(&turns), "unattributed text");
    }

    #[test]
    fn summarize_options_defaults_to_no_system_prompt_override() {
        let options = SummarizeOptions::new("claude-sonnet-4-5");
        assert_eq!(options.model, "claude-sonnet-4-5");
        assert!(options.system_prompt.is_none());
    }

    #[test]
    fn summarize_options_with_system_prompt_overrides_default() {
        let options = SummarizeOptions::new("gpt-4o-mini").with_system_prompt("Custom prompt");
        assert_eq!(options.system_prompt.as_deref(), Some("Custom prompt"));
    }
}
