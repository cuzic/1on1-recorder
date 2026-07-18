//! Thin wrapper over the `genai` crate (jeremychone/rust-genai) for summarizing
//! diarized meeting transcripts. `genai`'s `Client` already covers provider
//! switching (Anthropic/OpenAI/Gemini/etc.) via a model string, so this crate adds
//! three things: the "diarized turns in, summary text out" shape, an `AuthResolver`
//! that pulls provider API keys from `credential-store` instead of environment
//! variables, and (#57) [`build_vertex_client`] for the two providers whose
//! credential is a GCP project/location/service-account bundle rather than a bare
//! API key (Claude and Gemini via Google Vertex AI).
//!
//! Like `stt-api`, this crate has no dependency on this project's session/track
//! types — [`TranscriptTurn`] is a minimal, crate-local type. Mapping a project
//! transcript type into it is the caller's job.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use credential_store::CredentialStore;
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatRequest};
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use serde::{Deserialize, Serialize};

pub mod cli_backend;

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

/// Convenience account name for the AWS Bedrock API key (#58). Unlike SigV4-based
/// Bedrock auth (`genai`'s `adapter_sigv4.rs`, hardcoded to the AWS SDK's default
/// credential chain and not used by this crate), Bedrock's newer "API keys" feature
/// (`adapter_api.rs`) is a bare bearer token sent as `Authorization: Bearer
/// {api_key}` — the same shape as [`CLAUDE_API_KEY_ACCOUNT`] and friends, so it goes
/// through the plain [`credential_store_auth_resolver`] path, not a dedicated
/// client-builder function like [`build_vertex_client`].
pub const BEDROCK_API_KEY_ACCOUNT: &str = "bedrock-api-key";

/// Account name under which [`VertexCredentials`] (serialized as JSON, same "one
/// JSON blob per account" shape as `stt_google::GoogleSttCredentials`) is stored for
/// the Claude-via-Vertex provider.
pub const CLAUDE_VERTEX_CREDENTIALS_ACCOUNT: &str = "claude-vertex-credentials";

/// Same as [`CLAUDE_VERTEX_CREDENTIALS_ACCOUNT`], for the Gemini-via-Vertex provider.
pub const GEMINI_VERTEX_CREDENTIALS_ACCOUNT: &str = "gemini-vertex-credentials";

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

/// Builds the single prompt string [`cli_backend::summarize_via_cli`] hands to the
/// `claude`/`codex` CLIs, which — unlike `genai`'s `ChatRequest` — take one prompt
/// argument rather than separate system/user messages. Reuses the same system
/// prompt resolution and transcript rendering as [`build_chat_request`] so the two
/// execution paths (genai vs. CLI subprocess) produce equivalent input.
pub(crate) fn build_cli_prompt(turns: &[TranscriptTurn], options: &SummarizeOptions) -> String {
    let system = options
        .system_prompt
        .as_deref()
        .unwrap_or(DEFAULT_SYSTEM_PROMPT);
    format!("{system}\n\n{}", render_transcript(turns))
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

/// Where to find the service-account key used to mint OAuth tokens for Vertex AI, if
/// not relying on Application Default Credentials. Same shape as
/// `stt_google::ServiceAccountSource`, but this crate's own type — this crate has no
/// dependency on `stt-google`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VertexServiceAccountSource {
    /// Resolve credentials the standard ADC way: `GOOGLE_APPLICATION_CREDENTIALS`,
    /// `gcloud`'s user credentials, or the GCE/Cloud Run metadata server.
    ApplicationDefault,
    /// Inline service-account key JSON (the file content, not a path).
    Json(String),
    /// Path to a service-account key JSON file on disk.
    Path(String),
}

/// Everything one `credential-store` entry needs for Google Vertex AI: a bare API
/// key doesn't fit this provider since every request is also scoped to a GCP
/// project and location (see [`build_vertex_client`]'s endpoint construction). Same
/// shape as `stt_google::GoogleSttCredentials`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VertexCredentials {
    pub project_id: String,
    /// GCP location, e.g. `"global"` or a region like `"us-central1"` — selects the
    /// regional `{location}-aiplatform.googleapis.com` endpoint (see
    /// [`vertex_base_url`]).
    pub location: String,
    pub service_account: VertexServiceAccountSource,
}

impl VertexCredentials {
    pub fn new(project_id: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            location: location.into(),
            service_account: VertexServiceAccountSource::ApplicationDefault,
        }
    }

    pub fn with_service_account_json(mut self, json: impl Into<String>) -> Self {
        self.service_account = VertexServiceAccountSource::Json(json.into());
        self
    }

    pub fn with_service_account_path(mut self, path: impl Into<String>) -> Self {
        self.service_account = VertexServiceAccountSource::Path(path.into());
        self
    }
}

const VERTEX_OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Vertex AI's regional REST base (verified 2026-07-17 against
/// <https://docs.cloud.google.com/vertex-ai/docs/reference/rest> — every
/// `publishers/{publisher}/models/{model}:{method}` call hangs off this prefix).
/// `genai`'s built-in Vertex adapter would otherwise build this same URL from the
/// `VERTEX_PROJECT_ID`/`VERTEX_LOCATION` env vars; building it here instead lets
/// [`build_vertex_client`] avoid touching process-global env state.
fn vertex_base_url(project_id: &str, location: &str) -> String {
    format!("https://{location}-aiplatform.googleapis.com/v1/projects/{project_id}/locations/{location}/")
}

async fn resolve_vertex_token(credentials: &VertexCredentials) -> Result<String, genai::resolver::Error> {
    use gcp_auth::TokenProvider;

    let scopes = &[VERTEX_OAUTH_SCOPE];
    let token = match &credentials.service_account {
        VertexServiceAccountSource::ApplicationDefault => {
            let provider = gcp_auth::provider()
                .await
                .map_err(|e| genai::resolver::Error::Custom(format!("gcp_auth: {e}")))?;
            provider
                .token(scopes)
                .await
                .map_err(|e| genai::resolver::Error::Custom(format!("gcp_auth: {e}")))?
        }
        VertexServiceAccountSource::Json(json) => {
            let account = gcp_auth::CustomServiceAccount::from_json(json)
                .map_err(|e| genai::resolver::Error::Custom(format!("gcp_auth: {e}")))?;
            account
                .token(scopes)
                .await
                .map_err(|e| genai::resolver::Error::Custom(format!("gcp_auth: {e}")))?
        }
        VertexServiceAccountSource::Path(path) => {
            let account = gcp_auth::CustomServiceAccount::from_file(path)
                .map_err(|e| genai::resolver::Error::Custom(format!("gcp_auth: {e}")))?;
            account
                .token(scopes)
                .await
                .map_err(|e| genai::resolver::Error::Custom(format!("gcp_auth: {e}")))?
        }
    };
    Ok(token.as_str().to_string())
}

/// Builds a `genai` `Client` that routes chat calls to Claude or Gemini through
/// Google Vertex AI, using `credentials` for both endpoint routing (project/
/// location) and auth (an OAuth2 access token minted via `gcp_auth`) — no
/// `VERTEX_PROJECT_ID`/`VERTEX_LOCATION` env vars needed.
///
/// Two resolvers do the work, combined the way `genai` expects (`Client` runs the
/// `AuthResolver` first per call to build a default `ServiceTarget`, then hands that
/// target to the `ServiceTargetResolver` to adjust further — see `examples/
/// c06-target-resolver.rs` upstream): the `AuthResolver` is async because minting a
/// token via `gcp_auth` is an async operation (and needs to happen per-call, not
/// once at client-build time, since tokens expire and `gcp_auth` handles refreshing
/// them); the `ServiceTargetResolver` is plain sync string formatting, and forces
/// `AdapterKind::Vertex` on the model regardless of what prefix `options.model` used
/// (so callers can pass either a bare `"claude-sonnet-4-5"` or a
/// `"vertex::claude-sonnet-4-5"`-style namespaced string — either way this resolver
/// is what actually selects the Vertex adapter, not `genai`'s own namespace-based
/// dispatch).
pub fn build_vertex_client(credentials: VertexCredentials) -> Client {
    let credentials = Arc::new(credentials);

    let auth_credentials = credentials.clone();
    let auth_resolver = AuthResolver::from_resolver_async_fn(move |_model_iden: ModelIden| {
        let credentials = auth_credentials.clone();
        Box::pin(async move {
            let token = resolve_vertex_token(&credentials).await?;
            Ok(Some(AuthData::from_single(token)))
        }) as Pin<Box<dyn Future<Output = Result<Option<AuthData>, genai::resolver::Error>> + Send>>
    });

    let target_credentials = credentials.clone();
    let target_resolver = ServiceTargetResolver::from_resolver_fn(
        move |service_target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            let ServiceTarget { auth, model, .. } = service_target;
            let endpoint = Endpoint::from_owned(vertex_base_url(
                &target_credentials.project_id,
                &target_credentials.location,
            ));
            let model = ModelIden::new(AdapterKind::Vertex, model.model_name);
            Ok(ServiceTarget { endpoint, auth, model })
        },
    );

    Client::builder()
        .with_auth_resolver(auth_resolver)
        .with_service_target_resolver(target_resolver)
        .build()
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

    #[test]
    fn vertex_base_url_uses_regional_aiplatform_host() {
        assert_eq!(
            vertex_base_url("my-project", "us-central1"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/"
        );
    }

    #[test]
    fn vertex_credentials_defaults_to_application_default_service_account() {
        let credentials = VertexCredentials::new("my-project", "us-central1");
        assert_eq!(credentials.project_id, "my-project");
        assert_eq!(credentials.location, "us-central1");
        assert!(matches!(
            credentials.service_account,
            VertexServiceAccountSource::ApplicationDefault
        ));
    }

    #[test]
    fn vertex_credentials_with_service_account_json_round_trips_through_serde() {
        let credentials =
            VertexCredentials::new("my-project", "global").with_service_account_json("{\"type\":\"service_account\"}");
        let serialized = serde_json::to_string(&credentials).expect("serialize");
        let deserialized: VertexCredentials = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(deserialized.project_id, "my-project");
        assert!(matches!(
            deserialized.service_account,
            VertexServiceAccountSource::Json(json) if json == "{\"type\":\"service_account\"}"
        ));
    }

    #[test]
    fn build_vertex_client_constructs_without_touching_network() {
        // `build_vertex_client` only wires up resolvers; nothing in it should call
        // `gcp_auth` or make a network request until an actual `exec_chat` runs.
        let credentials = VertexCredentials::new("my-project", "us-central1");
        let _client = build_vertex_client(credentials);
    }
}
