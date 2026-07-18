//! Settings screen (#31/#49 STT provider + credentials, #37 summary LLM provider +
//! API key). Reachable from `ui::App` via a `Signal<Screen>` swap — no router crate
//! needed for two screens. Secrets are never displayed once saved (`credential-store`
//! `load` is only used to decide "設定済み/未設定", not to populate an input).
//!
//! Each provider category (STT, summary) is split into two independent sections:
//! "credential registration" (register a key/credential for any one provider) and
//! "active selection" (pick which already-registered provider is actually used).
//! Before this split, one form did both jobs at once — switching the active
//! provider required re-entering that provider's API key every time, since the
//! save handler rejected an empty key even when the user only meant to switch, not
//! re-register. The two `save_*_credential`/`save_*_active` pairs below fix that:
//! switching which provider is active never touches its stored credential.

use std::sync::Arc;

use credential_store::CredentialStore;
use dioxus::prelude::*;

use crate::app_state::AppState;

/// Which top-level screen `ui::App` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Settings,
}

/// The live-transcription STT providers exposed in the picker (#49). Wraps
/// `app_service::SttProviderKind` (the type `live_transcription::run_live_transcription`
/// actually selects on) rather than replacing it, since this file still needs its
/// own per-variant label/credential-account mapping — the same "UI enum pairs with
/// a domain enum" shape as [`SummaryProvider`] pairing with `summarize`'s provider
/// strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SttProvider {
    Deepgram,
    OpenAi,
    Google,
    AssemblyAi,
}

impl SttProvider {
    const ALL: [SttProvider; 4] = [SttProvider::Deepgram, SttProvider::OpenAi, SttProvider::Google, SttProvider::AssemblyAi];

    fn kind(self) -> app_service::SttProviderKind {
        match self {
            SttProvider::Deepgram => app_service::SttProviderKind::Deepgram,
            SttProvider::OpenAi => app_service::SttProviderKind::OpenAi,
            SttProvider::Google => app_service::SttProviderKind::Google,
            SttProvider::AssemblyAi => app_service::SttProviderKind::AssemblyAi,
        }
    }

    fn from_kind(kind: app_service::SttProviderKind) -> Self {
        match kind {
            app_service::SttProviderKind::Deepgram => SttProvider::Deepgram,
            app_service::SttProviderKind::OpenAi => SttProvider::OpenAi,
            app_service::SttProviderKind::Google => SttProvider::Google,
            app_service::SttProviderKind::AssemblyAi => SttProvider::AssemblyAi,
        }
    }

    fn key(self) -> &'static str {
        self.kind().as_account_value()
    }

    fn from_key(key: &str) -> Self {
        app_service::SttProviderKind::from_account_value(key).map(Self::from_kind).unwrap_or(SttProvider::Deepgram)
    }

    fn label(self) -> &'static str {
        match self {
            SttProvider::Deepgram => "Deepgram",
            SttProvider::OpenAi => "OpenAI",
            SttProvider::Google => "Google Cloud Speech-to-Text",
            SttProvider::AssemblyAi => "AssemblyAI",
        }
    }

    /// `true` for [`SttProvider::Google`], the one provider whose credential is a
    /// JSON bundle (project/location/service-account) rather than a bare API key —
    /// see [`GoogleSttCredentials`](stt_google::GoogleSttCredentials)'s doc comment.
    fn is_google(self) -> bool {
        matches!(self, SttProvider::Google)
    }

    /// `credential-store` (service, account) for a bare-API-key provider. `None`
    /// for Google, which is saved through its own JSON-blob path instead (see
    /// `save_stt_credential` below).
    fn api_key_service_account(self) -> Option<(&'static str, &'static str)> {
        match self {
            SttProvider::Deepgram => Some((stt_deepgram::CREDENTIAL_SERVICE, stt_deepgram::DEEPGRAM_API_KEY_ACCOUNT)),
            SttProvider::OpenAi => Some((stt_openai::CREDENTIAL_SERVICE, stt_openai::OPENAI_STT_API_KEY_ACCOUNT)),
            SttProvider::AssemblyAi => Some((stt_assemblyai::CREDENTIAL_SERVICE, stt_assemblyai::ASSEMBLYAI_API_KEY_ACCOUNT)),
            SttProvider::Google => None,
        }
    }
}

/// Loads which STT provider is currently selected as active (`SELECTED_STT_PROVIDER_ACCOUNT`),
/// falling back to Deepgram when nothing has been saved yet — shared by the "which
/// provider am I editing credentials for" and "which provider is active" signals'
/// initial values, since both start out pointed at today's active provider.
fn load_active_stt_provider(state: &AppState) -> SttProvider {
    state
        .credential_store
        .load(app_service::CREDENTIAL_SERVICE, app_service::SELECTED_STT_PROVIDER_ACCOUNT)
        .ok()
        .map(|key| SttProvider::from_key(&key))
        .unwrap_or(SttProvider::Deepgram)
}

/// "設定済み/未設定" for `provider`: Google's credential is a JSON blob under its
/// own account (see `SttProvider::is_google`/`GoogleSttCredentials`), the other
/// three are a bare API key under `SttProvider::api_key_service_account`.
fn stt_provider_is_configured(state: &AppState, provider: SttProvider) -> bool {
    match provider.api_key_service_account() {
        Some((service, account)) => state.credential_store.load(service, account).is_ok(),
        None => state.credential_store.load(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT).is_ok(),
    }
}

/// The summary LLM providers exposed in the picker. `summarize` itself stays
/// provider-agnostic (it just takes a `genai` model string), so this enum — and
/// its mapping to `summarize`'s credential-store accounts — lives here instead.
/// `pub(crate)` so `ui.rs`'s "要約を生成" flow (#38) can resolve the same
/// provider/API-key-account mapping the settings screen saved, without
/// duplicating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryProvider {
    Claude,
    OpenAi,
    Gemini,
    Groq,
    DeepSeek,
    XAi,
    ClaudeVertex,
    GeminiVertex,
    ClaudeBedrock,
    ClaudeCli,
    Codex,
}

impl SummaryProvider {
    const ALL: [SummaryProvider; 11] = [
        SummaryProvider::Claude,
        SummaryProvider::OpenAi,
        SummaryProvider::Gemini,
        SummaryProvider::Groq,
        SummaryProvider::DeepSeek,
        SummaryProvider::XAi,
        SummaryProvider::ClaudeVertex,
        SummaryProvider::GeminiVertex,
        SummaryProvider::ClaudeBedrock,
        SummaryProvider::ClaudeCli,
        SummaryProvider::Codex,
    ];

    pub(crate) fn key(self) -> &'static str {
        match self {
            SummaryProvider::Claude => "claude",
            SummaryProvider::OpenAi => "openai",
            SummaryProvider::Gemini => "gemini",
            SummaryProvider::Groq => "groq",
            SummaryProvider::DeepSeek => "deepseek",
            SummaryProvider::XAi => "xai",
            SummaryProvider::ClaudeVertex => "claude-vertex",
            SummaryProvider::GeminiVertex => "gemini-vertex",
            SummaryProvider::ClaudeBedrock => "claude-bedrock",
            SummaryProvider::ClaudeCli => "claude-cli",
            SummaryProvider::Codex => "codex",
        }
    }

    fn label(self) -> &'static str {
        match self {
            SummaryProvider::Claude => "Claude (Anthropic)",
            SummaryProvider::OpenAi => "OpenAI",
            SummaryProvider::Gemini => "Gemini (Google)",
            SummaryProvider::Groq => "Groq",
            SummaryProvider::DeepSeek => "DeepSeek",
            SummaryProvider::XAi => "xAI (Grok)",
            SummaryProvider::ClaudeVertex => "Claude (Google Vertex AI)",
            SummaryProvider::GeminiVertex => "Gemini (Google Vertex AI)",
            SummaryProvider::ClaudeBedrock => "Claude (AWS Bedrock)",
            SummaryProvider::ClaudeCli => "Claude (Claude Code CLI)",
            SummaryProvider::Codex => "Codex (Codex CLI)",
        }
    }

    pub(crate) fn default_model(self) -> &'static str {
        match self {
            SummaryProvider::Claude => "claude-sonnet-4-5",
            SummaryProvider::OpenAi => "gpt-4o-mini",
            SummaryProvider::Gemini => "gemini-3-flash-preview",
            // `genai` requires Groq models to be namespaced (`groq::_model_`) since
            // v0.6.0 — unlike Gemini/DeepSeek/xAI, Groq model names have no
            // recognizable prefix genai can dispatch on automatically.
            SummaryProvider::Groq => "groq::openai/gpt-oss-20b",
            // `deepseek-chat`/`deepseek-reasoner` are being deprecated by DeepSeek;
            // `deepseek-v4-flash` is genai's current default DeepSeek model.
            SummaryProvider::DeepSeek => "deepseek-v4-flash",
            SummaryProvider::XAi => "grok-4",
            // `vertex::` namespaces the model the same way `groq::` does above;
            // `summarize::build_vertex_client`'s `ServiceTargetResolver` forces the
            // Vertex adapter regardless, but the namespace keeps these strings
            // self-documenting and matches `genai`'s own Vertex convention.
            SummaryProvider::ClaudeVertex => "vertex::claude-sonnet-4-5",
            SummaryProvider::GeminiVertex => "vertex::gemini-3-flash-preview",
            // `bedrock_api::` namespaces the model the same way `groq::`/`vertex::` do
            // above, selecting `genai`'s Bedrock-via-API-key adapter (bearer-token
            // auth, not the SigV4/default-AWS-credential-chain adapter). Bedrock
            // model IDs are the publisher's own ID, not `genai`'s usual bare model
            // name — verified against `genai` 0.7.0-beta.13's
            // `adapter/adapters/bedrock/shared.rs` curated model list.
            SummaryProvider::ClaudeBedrock => "bedrock_api::anthropic.claude-sonnet-4-5-20250929-v1:0",
            // These two go through `claude --model`/`codex -m`, not `genai` — plain
            // CLI model aliases, not `genai` model spec strings.
            SummaryProvider::ClaudeCli => "sonnet",
            SummaryProvider::Codex => "gpt-5.5",
        }
    }

    /// `credential-store` account this provider's credential is saved under, or
    /// `None` for [`SummaryProvider::ClaudeCli`]/[`SummaryProvider::Codex`] (see
    /// [`Self::uses_cli`]): both authenticate as the `claude`/`codex` CLI's own
    /// OAuth/subscription login, so there is no API key for this app to store.
    /// [`SummaryProvider::ClaudeVertex`]/[`SummaryProvider::GeminiVertex`] store a
    /// [`summarize::VertexCredentials`] JSON blob here instead of a bare API key
    /// (see [`Self::is_vertex`]) — `credential-store` doesn't care about the shape
    /// of what's stored, so the same account-based "設定済み/未設定" check in
    /// `summary_provider_is_configured` works for both credential shapes unchanged.
    pub(crate) fn api_key_account(self) -> Option<&'static str> {
        match self {
            SummaryProvider::Claude => Some(summarize::CLAUDE_API_KEY_ACCOUNT),
            SummaryProvider::OpenAi => Some(summarize::OPENAI_API_KEY_ACCOUNT),
            SummaryProvider::Gemini => Some(summarize::GEMINI_API_KEY_ACCOUNT),
            SummaryProvider::Groq => Some(summarize::GROQ_API_KEY_ACCOUNT),
            SummaryProvider::DeepSeek => Some(summarize::DEEPSEEK_API_KEY_ACCOUNT),
            SummaryProvider::XAi => Some(summarize::XAI_API_KEY_ACCOUNT),
            SummaryProvider::ClaudeVertex => Some(summarize::CLAUDE_VERTEX_CREDENTIALS_ACCOUNT),
            SummaryProvider::GeminiVertex => Some(summarize::GEMINI_VERTEX_CREDENTIALS_ACCOUNT),
            SummaryProvider::ClaudeBedrock => Some(summarize::BEDROCK_API_KEY_ACCOUNT),
            SummaryProvider::ClaudeCli | SummaryProvider::Codex => None,
        }
    }

    pub(crate) fn from_key(key: &str) -> Self {
        Self::ALL.into_iter().find(|p| p.key() == key).unwrap_or(SummaryProvider::Claude)
    }

    /// `true` for [`SummaryProvider::ClaudeVertex`]/[`SummaryProvider::GeminiVertex`],
    /// whose credential is a JSON bundle (project/location/service-account) rather
    /// than a bare API key — same idea as [`SttProvider::is_google`], but backed by
    /// `summarize`'s own [`summarize::VertexCredentials`] type, not
    /// `stt_google::GoogleSttCredentials`.
    pub(crate) fn is_vertex(self) -> bool {
        matches!(self, SummaryProvider::ClaudeVertex | SummaryProvider::GeminiVertex)
    }

    /// `true` for [`SummaryProvider::ClaudeCli`]/[`SummaryProvider::Codex`] (#59),
    /// which shell out to the `claude`/`codex` CLIs (`summarize::cli_backend`)
    /// instead of calling a provider API through `genai` — no API key, so the
    /// "credential registration" section shows CLI-detection status instead of an
    /// input field, and `ui.rs`'s generate-summary handler dispatches to
    /// `summarize::cli_backend::summarize_via_cli` rather than `summarize::summarize`.
    pub(crate) fn uses_cli(self) -> bool {
        matches!(self, SummaryProvider::ClaudeCli | SummaryProvider::Codex)
    }

    /// The [`summarize::cli_backend::CliBackend`] this provider runs on. Only
    /// meaningful when [`Self::uses_cli`] is `true`.
    pub(crate) fn cli_backend(self) -> Option<summarize::cli_backend::CliBackend> {
        match self {
            SummaryProvider::ClaudeCli => Some(summarize::cli_backend::CliBackend::ClaudeCode),
            SummaryProvider::Codex => Some(summarize::cli_backend::CliBackend::Codex),
            _ => None,
        }
    }

    /// Representative current models offered in the settings picker's `<select>`
    /// (`default_model()` is always included). Not exhaustive — the UI falls back
    /// to a freeform "カスタム" entry (see [`CUSTOM_MODEL`]) for anything else, so
    /// this list doesn't need to track every new model release.
    fn known_models(self) -> &'static [&'static str] {
        match self {
            SummaryProvider::Claude => &["claude-sonnet-4-5", "claude-opus-4-5", "claude-haiku-4-5"],
            SummaryProvider::OpenAi => &["gpt-4o-mini", "gpt-4o", "gpt-5-mini"],
            SummaryProvider::Gemini => &["gemini-3-flash-preview", "gemini-3-pro-preview", "gemini-2.5-flash"],
            // Same `groq::` namespace requirement as `default_model` above.
            SummaryProvider::Groq => &["groq::openai/gpt-oss-20b", "groq::openai/gpt-oss-120b", "groq::llama-3.3-70b-versatile"],
            SummaryProvider::DeepSeek => &["deepseek-v4-flash", "deepseek-v4"],
            SummaryProvider::XAi => &["grok-4", "grok-4-fast", "grok-3"],
            SummaryProvider::ClaudeVertex => &["vertex::claude-sonnet-4-5", "vertex::claude-opus-4-5", "vertex::claude-haiku-4-5"],
            SummaryProvider::GeminiVertex => &["vertex::gemini-3-flash-preview", "vertex::gemini-3-pro-preview", "vertex::gemini-2.5-flash"],
            // Bedrock model IDs (`genai`'s curated list) rather than the bare
            // `claude-*` names the other Claude entries use.
            SummaryProvider::ClaudeBedrock => &[
                "bedrock_api::anthropic.claude-sonnet-4-5-20250929-v1:0",
                "bedrock_api::anthropic.claude-opus-4-1-20250805-v1:0",
                "bedrock_api::anthropic.claude-haiku-4-5-20251001-v1:0",
            ],
            // `claude --model` aliases (see `claude --help`), not `genai` model
            // spec strings.
            SummaryProvider::ClaudeCli => &["sonnet", "opus", "haiku"],
            // Model slugs from this sandbox's `~/.codex/models_cache.json`.
            SummaryProvider::Codex => &["gpt-5.5", "gpt-5.5-codex"],
        }
    }
}

/// Sentinel `<select>` value meaning "not one of `SummaryProvider::known_models()`
/// — show the freeform text input instead". Not a valid `genai` model string, so it
/// can't collide with a real model name.
const CUSTOM_MODEL: &str = "__custom__";

/// Loads which summary provider is currently active (`SELECTED_PROVIDER_ACCOUNT`),
/// falling back to Claude — same role as `load_active_stt_provider` above.
fn load_active_summary_provider(state: &AppState) -> SummaryProvider {
    state
        .credential_store
        .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT)
        .ok()
        .map(|key| SummaryProvider::from_key(&key))
        .unwrap_or(SummaryProvider::Claude)
}

/// "設定済み/未設定" for the provider picker. [`SummaryProvider::uses_cli`]
/// providers have no stored credential to check (#59) — they're always shown as
/// usable here; whether the underlying CLI is actually installed is checked
/// separately (async, since it spawns a subprocess) in the credential section.
fn summary_provider_is_configured(state: &AppState, provider: SummaryProvider) -> bool {
    match provider.api_key_account() {
        Some(account) => state.credential_store.load(summarize::CREDENTIAL_SERVICE, account).is_ok(),
        None => true,
    }
}

/// Status text for the "資格情報の登録" section when `provider` is CLI-based
/// (#59) — replaces the API key form, since these providers have nothing to save.
/// `available` is the latest [`summarize::cli_backend::CliBackend::is_available`]
/// result for `provider` (see the `use_effect` that keeps `summary_cli_available`
/// in sync with `summary_edit_provider`).
fn summary_cli_status_text(provider: SummaryProvider, available: Option<bool>) -> String {
    let Some(backend) = provider.cli_backend() else {
        return String::new();
    };
    match available {
        Some(true) => format!("{} コマンドを検出しました(ログイン状態は要約生成時に確認されます)", backend.binary()),
        Some(false) => format!("{} コマンドが見つかりません。インストールしてPATHに追加し、ログインしてください", backend.binary()),
        None => "確認中...".to_string(),
    }
}

const STYLE: &str = r#"
.settings-container {
  margin: 0;
  padding: 5vh 2rem;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 1.5em;
  font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
}
.settings-header {
  width: 100%;
  max-width: 360px;
  display: flex;
  align-items: center;
  gap: 0.8em;
}
.settings-header h1 {
  margin: 0;
  font-size: 1.2em;
}
.settings-section {
  width: 100%;
  max-width: 360px;
  display: flex;
  flex-direction: column;
  align-items: stretch;
  gap: 0.5em;
  text-align: left;
}
.settings-section h2 {
  margin: 0;
  font-size: 1em;
}
.settings-section h3 {
  margin: 0.6em 0 0;
  font-size: 0.85em;
  opacity: 0.75;
  border-top: 1px solid #333;
  padding-top: 0.6em;
}
.settings-section label {
  font-size: 0.85em;
  opacity: 0.8;
  display: flex;
  flex-direction: column;
  gap: 0.3em;
}
.settings-section input,
.settings-section select,
.settings-section textarea {
  padding: 0.5em;
  border-radius: 6px;
  border: 1px solid #555;
  font-size: 1em;
}
.settings-section textarea {
  min-height: 6em;
  font-family: monospace;
  resize: vertical;
}
.status-badge {
  font-size: 0.8em;
  opacity: 0.7;
  margin: 0;
}
"#;

#[component]
pub fn Settings(mut screen: Signal<Screen>) -> Element {
    let state = use_context::<Arc<AppState>>();

    // ---- STT: 資格情報の登録(どのプロバイダを編集中かは表示のためだけの状態で、
    // 「使用するプロバイダ」の選択とは独立。初期値だけ現在アクティブなプロバイダに
    // 合わせておく) ----
    let mut stt_edit_provider = use_signal({
        let state = state.clone();
        move || load_active_stt_provider(&state)
    });
    let mut stt_edit_configured = use_signal({
        let state = state.clone();
        move || stt_provider_is_configured(&state, stt_edit_provider())
    });
    let mut stt_key_input = use_signal(String::new);
    let mut stt_google_project_input = use_signal(String::new);
    let mut stt_google_location_input = use_signal(|| "global".to_string());
    let mut stt_google_json_input = use_signal(String::new);
    let mut stt_credential_message = use_signal(|| None::<String>);

    // ---- STT: 使用するプロバイダの選択 ----
    let mut stt_active_provider = use_signal({
        let state = state.clone();
        move || load_active_stt_provider(&state)
    });
    let mut stt_active_message = use_signal(|| None::<String>);

    // ---- 要約: 資格情報の登録 ----
    let mut summary_edit_provider = use_signal({
        let state = state.clone();
        move || load_active_summary_provider(&state)
    });
    let mut summary_edit_key_configured = use_signal({
        let state = state.clone();
        move || summary_provider_is_configured(&state, summary_edit_provider())
    });
    let mut summary_edit_key_input = use_signal(String::new);
    let mut summary_vertex_project_input = use_signal(String::new);
    let mut summary_vertex_location_input = use_signal(|| "global".to_string());
    let mut summary_vertex_json_input = use_signal(String::new);
    let mut summary_credential_message = use_signal(|| None::<String>);
    // `Some(true/false)` once `<binary> --version` has been checked for the
    // currently-edited CLI-based provider (#59); `None` while checking or when
    // `summary_edit_provider()` isn't CLI-based. Re-checked whenever the edited
    // provider changes (see the `use_effect` below), since detection is async
    // (spawns a subprocess) and can't happen inline in `summary_provider_is_configured`.
    let mut summary_cli_available = use_signal(|| None::<bool>);
    use_effect(move || {
        let provider = summary_edit_provider();
        match provider.cli_backend() {
            Some(backend) => {
                summary_cli_available.set(None);
                spawn(async move {
                    summary_cli_available.set(Some(backend.is_available().await));
                });
            }
            None => summary_cli_available.set(None),
        }
    });

    // ---- 要約: 使用するプロバイダ・モデルの選択 ----
    let mut summary_active_provider = use_signal({
        let state = state.clone();
        move || load_active_summary_provider(&state)
    });
    let mut model_input = use_signal({
        let state = state.clone();
        move || {
            state
                .credential_store
                .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_MODEL_ACCOUNT)
                .unwrap_or_else(|_| summary_active_provider().default_model().to_string())
        }
    });
    // Selects a `known_models()` entry, or `CUSTOM_MODEL` when the saved model
    // isn't one (e.g. a value from before this picker existed, or hand-entered).
    let mut model_select = use_signal(move || {
        let saved = model_input();
        if summary_active_provider().known_models().contains(&saved.as_str()) { saved } else { CUSTOM_MODEL.to_string() }
    });
    let mut summary_active_message = use_signal(|| None::<String>);

    // ==== STTハンドラ ====

    let onchange_stt_edit_provider = {
        let state = state.clone();
        move |evt: FormEvent| {
            let new_provider = SttProvider::from_key(&evt.value());
            stt_edit_provider.set(new_provider);
            stt_key_input.set(String::new());
            stt_google_project_input.set(String::new());
            stt_google_location_input.set("global".to_string());
            stt_google_json_input.set(String::new());
            stt_edit_configured.set(stt_provider_is_configured(&state, new_provider));
            stt_credential_message.set(None);
        }
    };

    // Saves only `stt_edit_provider`'s own credential — never touches which
    // provider is active (see `save_stt_active` below for that).
    let save_stt_credential = {
        let state = state.clone();
        move |_| {
            let current_provider = stt_edit_provider();

            if current_provider.is_google() {
                let project = stt_google_project_input().trim().to_string();
                let location = stt_google_location_input().trim().to_string();
                let json = stt_google_json_input().trim().to_string();
                if project.is_empty() || location.is_empty() || json.is_empty() {
                    stt_credential_message.set(Some("プロジェクトID・ロケーション・サービスアカウントJSONをすべて入力してください".to_string()));
                    return;
                }
                let credentials = stt_google::GoogleSttCredentials::new(project, location).with_service_account_json(json);
                let serialized = match serde_json::to_string(&credentials) {
                    Ok(s) => s,
                    Err(e) => {
                        stt_credential_message.set(Some(format!("認証情報の変換に失敗しました: {e}")));
                        return;
                    }
                };
                match state.credential_store.save(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT, &serialized) {
                    Ok(()) => {
                        stt_edit_configured.set(true);
                        stt_google_project_input.set(String::new());
                        stt_google_location_input.set("global".to_string());
                        stt_google_json_input.set(String::new());
                        stt_credential_message.set(Some("保存しました".to_string()));
                    }
                    Err(e) => stt_credential_message.set(Some(format!("保存に失敗しました: {e}"))),
                }
                return;
            }

            let key = stt_key_input().trim().to_string();
            if key.is_empty() {
                stt_credential_message.set(Some("APIキーを入力してください".to_string()));
                return;
            }
            // `SttProvider::Google` is handled above and always returns before here,
            // so this is always `Some` — see `SttProvider::api_key_service_account`.
            let Some((service, account)) = current_provider.api_key_service_account() else {
                stt_credential_message.set(Some("未対応のプロバイダです".to_string()));
                return;
            };
            match state.credential_store.save(service, account, &key) {
                Ok(()) => {
                    stt_edit_configured.set(true);
                    stt_key_input.set(String::new());
                    stt_credential_message.set(Some("保存しました".to_string()));
                }
                Err(e) => stt_credential_message.set(Some(format!("保存に失敗しました: {e}"))),
            }
        }
    };

    let onchange_stt_active_provider = move |evt: FormEvent| {
        stt_active_provider.set(SttProvider::from_key(&evt.value()));
        stt_active_message.set(None);
    };

    // Saves only which STT provider is active — never touches any provider's
    // stored credential (see `save_stt_credential` above for that). Allowed even
    // if the chosen provider has no credential saved yet; `live_transcription.rs`
    // already treats a missing credential as "skip live transcription for this
    // session", not a hard error.
    let save_stt_active = {
        let state = state.clone();
        move |_| match state.credential_store.save(app_service::CREDENTIAL_SERVICE, app_service::SELECTED_STT_PROVIDER_ACCOUNT, stt_active_provider().key()) {
            Ok(()) => stt_active_message.set(Some("保存しました".to_string())),
            Err(e) => stt_active_message.set(Some(format!("保存に失敗しました: {e}"))),
        }
    };

    // ==== 要約ハンドラ ====

    let onchange_summary_edit_provider = {
        let state = state.clone();
        move |evt: FormEvent| {
            let new_provider = SummaryProvider::from_key(&evt.value());
            summary_edit_provider.set(new_provider);
            summary_edit_key_input.set(String::new());
            summary_vertex_project_input.set(String::new());
            summary_vertex_location_input.set("global".to_string());
            summary_vertex_json_input.set(String::new());
            summary_edit_key_configured.set(summary_provider_is_configured(&state, new_provider));
            summary_credential_message.set(None);
        }
    };

    // Saves only `summary_edit_provider`'s own credential — never touches which
    // provider/model is active (see `save_summary_active` below for that).
    let save_summary_credential = {
        let state = state.clone();
        move |_| {
            let current_provider = summary_edit_provider();

            // CLI-based providers (#59) have no credential to save — the section
            // shows CLI-detection status instead of a form, so this handler is
            // unreachable via the UI for them (no save button is rendered), but
            // guard anyway rather than unwrapping `api_key_account()`.
            let Some(api_key_account) = current_provider.api_key_account() else {
                return;
            };

            if current_provider.is_vertex() {
                let project = summary_vertex_project_input().trim().to_string();
                let location = summary_vertex_location_input().trim().to_string();
                let json = summary_vertex_json_input().trim().to_string();
                if project.is_empty() || location.is_empty() || json.is_empty() {
                    summary_credential_message.set(Some("プロジェクトID・ロケーション・サービスアカウントJSONをすべて入力してください".to_string()));
                    return;
                }
                let credentials = summarize::VertexCredentials::new(project, location).with_service_account_json(json);
                let serialized = match serde_json::to_string(&credentials) {
                    Ok(s) => s,
                    Err(e) => {
                        summary_credential_message.set(Some(format!("認証情報の変換に失敗しました: {e}")));
                        return;
                    }
                };
                match state.credential_store.save(summarize::CREDENTIAL_SERVICE, api_key_account, &serialized) {
                    Ok(()) => {
                        summary_edit_key_configured.set(true);
                        summary_vertex_project_input.set(String::new());
                        summary_vertex_location_input.set("global".to_string());
                        summary_vertex_json_input.set(String::new());
                        summary_credential_message.set(Some("保存しました".to_string()));
                    }
                    Err(e) => summary_credential_message.set(Some(format!("保存に失敗しました: {e}"))),
                }
                return;
            }

            let key = summary_edit_key_input().trim().to_string();
            if key.is_empty() {
                summary_credential_message.set(Some("APIキーを入力してください".to_string()));
                return;
            }
            match state.credential_store.save(summarize::CREDENTIAL_SERVICE, api_key_account, &key) {
                Ok(()) => {
                    summary_edit_key_configured.set(true);
                    summary_edit_key_input.set(String::new());
                    summary_credential_message.set(Some("保存しました".to_string()));
                }
                Err(e) => summary_credential_message.set(Some(format!("保存に失敗しました: {e}"))),
            }
        }
    };

    let onchange_summary_active_provider = move |evt: FormEvent| {
        let new_provider = SummaryProvider::from_key(&evt.value());
        summary_active_provider.set(new_provider);
        model_input.set(new_provider.default_model().to_string());
        model_select.set(new_provider.default_model().to_string());
        summary_active_message.set(None);
    };

    let onchange_model = move |evt: FormEvent| {
        let value = evt.value();
        if value != CUSTOM_MODEL {
            model_input.set(value.clone());
        }
        model_select.set(value);
    };

    // Saves only which summary provider/model is active — never touches any
    // provider's stored API key (see `save_summary_credential` above for that).
    // Allowed even if the chosen provider has no key saved yet; #38's "要約を生成"
    // already surfaces "設定画面でAPIキーを設定してください" at generation time.
    let save_summary_active = {
        let state = state.clone();
        move |_| {
            let model = {
                let m = model_input().trim().to_string();
                if m.is_empty() { summary_active_provider().default_model().to_string() } else { m }
            };
            if let Err(e) = state.credential_store.save(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT, summary_active_provider().key()) {
                summary_active_message.set(Some(format!("保存に失敗しました: {e}")));
                return;
            }
            if let Err(e) = state.credential_store.save(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_MODEL_ACCOUNT, &model) {
                summary_active_message.set(Some(format!("保存に失敗しました: {e}")));
                return;
            }
            model_input.set(model);
            summary_active_message.set(Some("保存しました".to_string()));
        }
    };

    rsx! {
        style { "{STYLE}" }
        main { class: "settings-container",
            div { class: "settings-header",
                button { onclick: move |_| screen.set(Screen::Main), "← 戻る" }
                h1 { "設定" }
            }

            section { class: "settings-section",
                h2 { "音声認識 (STT) の資格情報" }
                p { class: "hint", "プロバイダごとにAPIキー/認証情報を登録します。実際に使うプロバイダの選択は下の「使用する音声認識プロバイダ」で行います。" }
                label {
                    "編集するプロバイダ"
                    select {
                        onchange: onchange_stt_edit_provider,
                        for p in SttProvider::ALL {
                            option { value: "{p.key()}", selected: p == stt_edit_provider(), "{p.label()}" }
                        }
                    }
                }
                p { class: "status-badge", if stt_edit_configured() { "設定済み" } else { "未設定" } }
                if stt_edit_provider().is_google() {
                    label {
                        "プロジェクトID"
                        input {
                            r#type: "text",
                            placeholder: "my-gcp-project",
                            value: "{stt_google_project_input}",
                            oninput: move |e| stt_google_project_input.set(e.value()),
                        }
                    }
                    label {
                        "ロケーション"
                        input {
                            r#type: "text",
                            placeholder: "global",
                            value: "{stt_google_location_input}",
                            oninput: move |e| stt_google_location_input.set(e.value()),
                        }
                    }
                    label {
                        "サービスアカウントJSON"
                        textarea {
                            placeholder: "サービスアカウントキーJSONファイルの中身を貼り付け",
                            value: "{stt_google_json_input}",
                            oninput: move |e| stt_google_json_input.set(e.value()),
                        }
                    }
                } else {
                    label {
                        "APIキー"
                        input {
                            r#type: "password",
                            placeholder: "APIキー",
                            value: "{stt_key_input}",
                            oninput: move |e| stt_key_input.set(e.value()),
                        }
                    }
                }
                button { class: "primary", onclick: save_stt_credential, "保存" }
                if let Some(msg) = stt_credential_message() {
                    p { class: "status-badge", "{msg}" }
                }
            }

            section { class: "settings-section",
                h2 { "使用する音声認識プロバイダ" }
                label {
                    "プロバイダ"
                    select {
                        onchange: onchange_stt_active_provider,
                        for p in SttProvider::ALL {
                            option {
                                value: "{p.key()}",
                                selected: p == stt_active_provider(),
                                if stt_provider_is_configured(&state, p) { "{p.label()} (設定済み)" } else { "{p.label()} (未設定)" }
                            }
                        }
                    }
                }
                button { class: "primary", onclick: save_stt_active, "この設定を保存" }
                if let Some(msg) = stt_active_message() {
                    p { class: "status-badge", "{msg}" }
                }
            }

            section { class: "settings-section",
                h2 { "要約 (LLM) の資格情報" }
                p { class: "hint", "プロバイダごとにAPIキーを登録します。実際に使うプロバイダ・モデルの選択は下の「使用する要約プロバイダ・モデル」で行います。" }
                label {
                    "編集するプロバイダ"
                    select {
                        onchange: onchange_summary_edit_provider,
                        for p in SummaryProvider::ALL {
                            option { value: "{p.key()}", selected: p == summary_edit_provider(), "{p.label()}" }
                        }
                    }
                }
                if summary_edit_provider().uses_cli() {
                    // CLI-based providers (#59) authenticate as the `claude`/
                    // `codex` CLI's own OAuth/subscription login — nothing for
                    // this app to save, so show detection status instead of a
                    // form/save button.
                    p { class: "hint", "APIキーは不要です。このプロバイダは claude/codex CLI 自体のログイン(OAuth/サブスクリプション認証)をそのまま使います。" }
                    p { class: "status-badge", "{summary_cli_status_text(summary_edit_provider(), summary_cli_available())}" }
                } else {
                    p { class: "status-badge", if summary_edit_key_configured() { "設定済み" } else { "未設定" } }
                    if summary_edit_provider().is_vertex() {
                        label {
                            "プロジェクトID"
                            input {
                                r#type: "text",
                                placeholder: "my-gcp-project",
                                value: "{summary_vertex_project_input}",
                                oninput: move |e| summary_vertex_project_input.set(e.value()),
                            }
                        }
                        label {
                            "ロケーション"
                            input {
                                r#type: "text",
                                placeholder: "global",
                                value: "{summary_vertex_location_input}",
                                oninput: move |e| summary_vertex_location_input.set(e.value()),
                            }
                        }
                        label {
                            "サービスアカウントJSON"
                            textarea {
                                placeholder: "サービスアカウントキーJSONファイルの中身を貼り付け",
                                value: "{summary_vertex_json_input}",
                                oninput: move |e| summary_vertex_json_input.set(e.value()),
                            }
                        }
                    } else {
                        if summary_edit_provider() == SummaryProvider::ClaudeBedrock {
                            p { class: "hint", "AWSコンソールのBedrock「API keys」画面で発行した長期(long-term) APIキーを貼り付けてください。短期(short-term)キーは最大12時間で失効するため、常駐アプリの資格情報には不向きです。" }
                        }
                        label {
                            "APIキー"
                            input {
                                r#type: "password",
                                placeholder: if summary_edit_provider() == SummaryProvider::ClaudeBedrock { "Bedrock APIキー(長期)" } else { "APIキー" },
                                value: "{summary_edit_key_input}",
                                oninput: move |e| summary_edit_key_input.set(e.value()),
                            }
                        }
                    }
                    button { class: "primary", onclick: save_summary_credential, "保存" }
                    if let Some(msg) = summary_credential_message() {
                        p { class: "status-badge", "{msg}" }
                    }
                }
            }

            section { class: "settings-section",
                h2 { "使用する要約プロバイダ・モデル" }
                label {
                    "プロバイダ"
                    select {
                        onchange: onchange_summary_active_provider,
                        for p in SummaryProvider::ALL {
                            option {
                                value: "{p.key()}",
                                selected: p == summary_active_provider(),
                                if summary_provider_is_configured(&state, p) { "{p.label()} (設定済み)" } else { "{p.label()} (未設定)" }
                            }
                        }
                    }
                }
                label {
                    "モデル"
                    select {
                        onchange: onchange_model,
                        for m in summary_active_provider().known_models() {
                            option { value: "{m}", selected: model_select() == *m, "{m}" }
                        }
                        option { value: CUSTOM_MODEL, selected: model_select() == CUSTOM_MODEL, "カスタム..." }
                    }
                }
                if model_select() == CUSTOM_MODEL {
                    label {
                        "カスタムモデル名"
                        input {
                            r#type: "text",
                            value: "{model_input}",
                            oninput: move |e| model_input.set(e.value()),
                        }
                    }
                }
                button { class: "primary", onclick: save_summary_active, "この設定を保存" }
                if let Some(msg) = summary_active_message() {
                    p { class: "status-badge", "{msg}" }
                }
            }
        }
    }
}
