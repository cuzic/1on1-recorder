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
use crate::summary_template::{self, SummaryTemplatePreset};

/// Which top-level screen `ui::App` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Settings,
    /// Task #69's past-sessions list (`history::History`) — reachable so a
    /// session recorded in an earlier app run can still be selected for
    /// summary generation/export once `AppState::last_summary` (in-memory
    /// only) has been cleared by a restart.
    History,
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
    /// Claude via the official Anthropic CLI's OAuth login (`ant auth login` /
    /// `ant auth print-credentials --access-token`) — bills against the user's own
    /// Claude subscription instead of a pay-per-token API key, same motivation as
    /// [`ClaudeCli`] but going through `genai`'s normal `exec_chat` call (see
    /// `summarize::build_claude_oauth_client`) rather than a `claude` CLI
    /// subprocess per summary.
    ClaudeOAuth,
    Codex,
    /// A local/self-hosted Ollama server (`genai`'s built-in `AdapterKind::Ollama`,
    /// default endpoint `http://localhost:11434/`) — no API key, base URL is
    /// configured separately in `AppSettings::ollama_base_url` (see the "Ollama設定"
    /// section below), and the model is whatever the user has locally pulled (no
    /// fixed [`Self::known_models`] list, unlike the hosted providers above).
    Ollama,
}

impl SummaryProvider {
    const ALL: [SummaryProvider; 13] = [
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
        SummaryProvider::ClaudeOAuth,
        SummaryProvider::Codex,
        SummaryProvider::Ollama,
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
            SummaryProvider::ClaudeOAuth => "claude-oauth",
            SummaryProvider::Codex => "codex",
            SummaryProvider::Ollama => "ollama",
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
            SummaryProvider::ClaudeOAuth => "Claude (ant CLIでOAuthログイン)",
            SummaryProvider::Codex => "Codex (Codex CLI)",
            SummaryProvider::Ollama => "Ollama (ローカル)",
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
            // Unlike `ClaudeCli` above, this goes through `genai`'s normal
            // `exec_chat` (see `summarize::build_claude_oauth_client`), so it takes
            // the same bare `genai` model spec string as `SummaryProvider::Claude`.
            SummaryProvider::ClaudeOAuth => "claude-sonnet-4-5",
            SummaryProvider::Codex => "gpt-5.5",
            // Not a `genai` model spec string in the usual sense — just a common
            // Ollama library model name, prefilled as a starting point in the
            // freeform model field (`known_models()` is empty for this provider, so
            // the settings UI always shows the custom-text input, never a `<select>`
            // of fixed choices). Kept non-empty so `save_summary_active`'s "fall back
            // to `default_model()` when the field is left blank" guard never silently
            // saves an empty model name.
            SummaryProvider::Ollama => "llama3.2",
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
            // `ClaudeCli`/`Codex` authenticate as the CLI's own login (see the doc
            // comment above); `ClaudeOAuth` likewise authenticates as the `ant`
            // CLI's own OAuth login (`ant auth login`), no API key of this app's
            // own to store either; `Ollama` needs no credential at all — a local
            // server with no API key, configured instead via
            // `AppSettings::ollama_base_url` (see the "Ollama設定" section in
            // `Settings`'s `rsx!`).
            SummaryProvider::ClaudeCli | SummaryProvider::ClaudeOAuth | SummaryProvider::Codex | SummaryProvider::Ollama => None,
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
            // Same bare `genai` model spec strings as `SummaryProvider::Claude` —
            // see `default_model()`'s comment on why this differs from `ClaudeCli`.
            SummaryProvider::ClaudeOAuth => &["claude-sonnet-4-5", "claude-opus-4-5", "claude-haiku-4-5"],
            // Model slugs from this sandbox's `~/.codex/models_cache.json`.
            SummaryProvider::Codex => &["gpt-5.5", "gpt-5.5-codex"],
            // Deliberately empty: unlike the hosted providers above, there's no
            // fixed catalog to offer — the available models are whatever the user
            // has locally `ollama pull`ed, which this app has no way to enumerate
            // without an extra round trip to the Ollama server. The settings UI
            // always falls back to the freeform "カスタム" input for this provider
            // as a result (see `CUSTOM_MODEL`).
            SummaryProvider::Ollama => &[],
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

/// Same "設定済み"/"未設定" badge pattern as `summary_provider_is_configured`,
/// for the Cloudflare AI Search credential `plugins/default/hint.rhai`'s
/// `rag_search("cloudflare", ...)` reads via `crates/rhai-engine/src/rag/cloudflare.rs`.
fn hint_cloudflare_is_configured(state: &AppState) -> bool {
    state.credential_store.load(rhai_engine::CLOUDFLARE_CREDENTIAL_SERVICE, rhai_engine::CLOUDFLARE_AI_SEARCH_ACCOUNT).is_ok()
}

/// Status text for the "資格情報の登録" section when `provider` is CLI-based
/// (#59) — replaces the API key form, since these providers have nothing to save.
/// `available` is the latest [`summarize::cli_backend::CliBackend::is_available`]
/// result for `provider` (see the `use_effect` that keeps `summary_cli_available`
/// in sync with `summary_edit_provider`).
fn summary_cli_status_text(provider: SummaryProvider, available: Option<bool>) -> String {
    if let Some(backend) = provider.cli_backend() {
        return match available {
            Some(true) => format!("{} コマンドを検出しました(ログイン状態は要約生成時に確認されます)", backend.binary()),
            Some(false) => format!("{} コマンドが見つかりません。インストールしてPATHに追加し、ログインしてください", backend.binary()),
            None => "確認中...".to_string(),
        };
    }
    if provider == SummaryProvider::ClaudeOAuth {
        return match available {
            Some(true) => "ant コマンドを検出しました(ログイン状態は要約生成時に確認されます)".to_string(),
            Some(false) => {
                "ant コマンドが見つかりません。https://github.com/anthropics/anthropic-cli からインストールし、`ant auth login` でログインしてください".to_string()
            }
            None => "確認中...".to_string(),
        };
    }
    String::new()
}

/// `<select>` sentinel meaning "follow whatever the OS reports as its current
/// default device" — `AppSettings::microphone_device_id`/`render_device_id`'s
/// `None`. Not a real `DeviceInfo::id` (those are opaque WASAPI/CoreAudio
/// endpoint IDs, never this literal string), so it can't collide with one.
const FOLLOW_SYSTEM_DEFAULT_DEVICE: &str = "__system_default__";

/// Platform-agnostic view of `app_service::DeviceInfo` for the `<select>` options
/// below — built once per list load so the `rsx!` markup doesn't need its own
/// `#[cfg(windows)]`/`#[cfg(target_os = "macos")]` branches (device enumeration
/// itself is real capture backend territory; this file only renders the result).
/// `is_default` is kept as its own field (rather than baked into a `label`
/// string) so it can also drive the "現在のシステム既定" hint line below each
/// `<select>` — that hint has to stay visible even while "システム既定" itself
/// is the selected option, i.e. even when no single `<option>`'s own label is
/// showing.
#[derive(Debug, Clone)]
struct DeviceOption {
    id: String,
    friendly_name: String,
    is_default: bool,
}

impl DeviceOption {
    fn option_label(&self) -> String {
        if self.is_default { format!("{} (既定)", self.friendly_name) } else { self.friendly_name.clone() }
    }
}

/// `true` only where a real capture backend (and therefore real device
/// enumeration) exists at all — see `recording.rs`'s three-way `#[cfg]` split.
/// Everywhere else, `load_capture_devices`/`load_render_devices` return an empty
/// list and the settings UI shows a hint instead of a picker with nothing but
/// "システム既定" in it.
fn device_selection_supported() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

#[cfg(any(windows, target_os = "macos"))]
fn to_device_option(d: app_service::DeviceInfo) -> DeviceOption {
    DeviceOption { id: d.id, friendly_name: d.friendly_name, is_default: d.is_default_for_role.is_some() }
}

#[cfg(any(windows, target_os = "macos"))]
fn load_capture_devices() -> (Vec<DeviceOption>, Option<String>) {
    match app_service::enumerate_capture_devices() {
        Ok(devices) => (devices.into_iter().map(to_device_option).collect(), None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn load_render_devices() -> (Vec<DeviceOption>, Option<String>) {
    match app_service::enumerate_render_devices() {
        Ok(devices) => (devices.into_iter().map(to_device_option).collect(), None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn load_capture_devices() -> (Vec<DeviceOption>, Option<String>) {
    (Vec::new(), None)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn load_render_devices() -> (Vec<DeviceOption>, Option<String>) {
    (Vec::new(), None)
}

/// Starts `app_service::DeviceChangeWatcher` on a blocking thread and, for as
/// long as this component stays mounted, calls `on_device_change` whenever the
/// OS reports a device add/remove/default-change (e.g. a Bluetooth headset
/// connecting or disconnecting) — so the device list picked up by
/// `load_capture_devices`/`load_render_devices` at mount time doesn't otherwise
/// go stale until the user notices and presses "デバイス一覧を更新" themselves.
/// If the watcher fails to start (feature unsupported on this build, or the OS
/// call itself fails), this silently does nothing — the manual refresh button
/// still works either way, this is purely a convenience on top of it.
#[cfg(any(windows, target_os = "macos"))]
fn spawn_device_change_watcher(on_device_change: impl FnMut() + Clone + 'static) {
    // `use_future`'s factory closure is itself `FnMut` (it may run again on
    // restart), so it needs to hand its inner `async move` block a fresh clone of
    // `on_device_change` each time rather than moving the one shared copy in —
    // `do_refresh_devices` only captures `Copy` `Signal`s, so cloning it is cheap.
    use_future(move || {
        let mut on_device_change = on_device_change.clone();
        async move {
            let watcher = match tokio::task::spawn_blocking(app_service::DeviceChangeWatcher::start).await {
                Ok(Ok(watcher)) => watcher,
                _ => return,
            };
            let mut last_generation = watcher.generation();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let generation = watcher.generation();
                if generation != last_generation {
                    last_generation = generation;
                    on_device_change();
                }
            }
        }
    });
}

#[cfg(not(any(windows, target_os = "macos")))]
fn spawn_device_change_watcher(_on_device_change: impl FnMut() + Clone + 'static) {}

/// `mic_device_select`/`render_device_select`'s one-shot initial value: `saved`
/// unchanged if it's still in `devices`, otherwise "システム既定" plus `true` in
/// the second element. Returns the stale flag instead of setting a message signal
/// directly (Codex review finding): called once each for mic and render from two
/// independent `use_signal` initializers, a version that set `device_message`
/// itself would have the second call silently clobber the first's message
/// whenever both are stale at once, dropping the mic notice entirely. The caller
/// folds both flags into one combined `device_message` instead — see the call
/// site's doc comment. Silently keeping a stale id (with the `<select>` just
/// happening to *display* "システム既定" because no `<option>` matches) is a trap
/// for the next save.
fn resolve_initial_device_selection(saved: Option<String>, devices: &[DeviceOption]) -> (String, bool) {
    match saved {
        Some(id) if devices.iter().any(|d| d.id == id) => (id, false),
        Some(_) => (FOLLOW_SYSTEM_DEFAULT_DEVICE.to_string(), true),
        None => (FOLLOW_SYSTEM_DEFAULT_DEVICE.to_string(), false),
    }
}

/// `refresh_devices`'s live-signal counterpart of `resolve_initial_device_selection`
/// — resets `select` back to "システム既定" if its current value has fallen out of
/// `devices` (e.g. the device was unplugged since Settings was opened), and
/// reports whether it did so (so the caller can fold that into one combined
/// `device_message`, rather than this function's own message overwriting/being
/// overwritten by the enumeration-error message `refresh_devices` also sets).
fn reset_if_stale(mut select: Signal<String>, devices: &[DeviceOption]) -> bool {
    let current = select();
    if current != FOLLOW_SYSTEM_DEFAULT_DEVICE && !devices.iter().any(|d| d.id == current) {
        select.set(FOLLOW_SYSTEM_DEFAULT_DEVICE.to_string());
        true
    } else {
        false
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
.summary-template-preview {
  max-height: 10em;
  overflow-y: auto;
  white-space: pre-wrap;
  font-size: 0.8em;
  opacity: 0.75;
  padding: 0.5em;
  border: 1px solid #555;
  border-radius: 6px;
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
    // currently-edited CLI-based provider (#59) or `ant --version` for
    // `SummaryProvider::ClaudeOAuth`; `None` while checking or when
    // `summary_edit_provider()` needs neither check. Re-checked whenever the
    // edited provider changes (see the `use_effect` below), since detection is
    // async (spawns a subprocess) and can't happen inline in
    // `summary_provider_is_configured`.
    let mut summary_cli_available = use_signal(|| None::<bool>);
    use_effect(move || {
        let provider = summary_edit_provider();
        if let Some(backend) = provider.cli_backend() {
            summary_cli_available.set(None);
            spawn(async move {
                summary_cli_available.set(Some(backend.is_available().await));
            });
        } else if provider == SummaryProvider::ClaudeOAuth {
            summary_cli_available.set(None);
            spawn(async move {
                summary_cli_available.set(Some(summarize::ant_cli_is_available().await));
            });
        } else {
            summary_cli_available.set(None);
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

    // ---- Ollama: サーバーbase URLの設定(要約プロバイダの選択とは独立 — Ollamaを
    // 選んでいなくても登録しておける、テンプレート設定と同じ考え方) ----
    let mut ollama_base_url_input = use_signal({
        let state = state.clone();
        move || state.app_settings.lock().unwrap().ollama_base_url.clone().unwrap_or_default()
    });
    let mut ollama_base_url_message = use_signal(|| None::<String>);

    // ---- 録音デバイスの選択(マイク/スピーカーが複数あるとき用、他の設定とは独立) ----
    // Resolved up front — rather than inside `mic_device_select`/`render_device_select`'s
    // own `use_signal` initializers — so a stale mic pick and a stale render pick
    // found at the same time fold into one combined `device_message` below (the
    // same four-way match `refresh_devices` uses for its own re-check), instead of
    // the second initializer's message silently overwriting the first's.
    let initial_mic_devices = load_capture_devices();
    let initial_render_devices = load_render_devices();
    let saved_mic_device = state.app_settings.lock().unwrap().microphone_device_id.clone();
    let saved_render_device = state.app_settings.lock().unwrap().render_device_id.clone();
    // Falls back to "システム既定" (rather than the saved-but-now-missing id) when
    // the saved device isn't in the freshly enumerated list — e.g. a USB headset
    // unplugged since it was chosen. Without this, the `<select>` below displays
    // "システム既定" (no `<option>` matches the stale id) while this signal still
    // holds it, so an unrelated save silently re-persists a device id that no
    // longer exists. See `resolve_initial_device_selection`'s doc comment.
    let (initial_mic_select, mic_was_stale) = resolve_initial_device_selection(saved_mic_device, &initial_mic_devices.0);
    let (initial_render_select, render_was_stale) = resolve_initial_device_selection(saved_render_device, &initial_render_devices.0);
    let mut device_message = use_signal(move || match (mic_was_stale, render_was_stale) {
        (true, true) => Some("選択していたマイクとスピーカーが見つからないため、システム既定に切り替えました。必要であれば選び直して保存してください。".to_string()),
        (true, false) => Some("選択していたマイクが見つからないため、システム既定に切り替えました。必要であれば選び直して保存してください。".to_string()),
        (false, true) => Some("選択していたスピーカーが見つからないため、システム既定に切り替えました。必要であれば選び直して保存してください。".to_string()),
        (false, false) => None,
    });
    let mut mic_devices = use_signal(move || initial_mic_devices);
    let mut render_devices = use_signal(move || initial_render_devices);
    let mut mic_device_select = use_signal(move || initial_mic_select);
    let mut render_device_select = use_signal(move || initial_render_select);

    // ---- 会話ヒント (RAG): 有効化・プロバイダ・デバウンス秒数・Cloudflare資格情報(他の設定とは独立) ----
    let mut hint_enabled_select = use_signal({
        let state = state.clone();
        move || state.app_settings.lock().unwrap().hint_enabled.unwrap_or(false)
    });
    let mut hint_provider_select = use_signal({
        let state = state.clone();
        move || state.app_settings.lock().unwrap().hint_provider.clone().unwrap_or_else(|| "cloudflare".to_string())
    });
    let mut hint_debounce_input = use_signal({
        let state = state.clone();
        move || state.app_settings.lock().unwrap().hint_debounce_seconds.map(|s| s.to_string()).unwrap_or_else(|| "15".to_string())
    });
    let mut hint_cloudflare_account_id_input = use_signal(String::new);
    let mut hint_cloudflare_api_token_input = use_signal(String::new);
    let mut hint_cloudflare_instance_input = use_signal(String::new);
    let mut hint_message = use_signal(|| None::<String>);

    // ---- 要約: プロンプトテンプレートの選択(プロバイダ・モデルの選択とは独立) ----
    let mut summary_template_select = use_signal({
        let state = state.clone();
        move || {
            let stored = state.app_settings.lock().unwrap().summary_template.clone();
            summary_template::select_key_for(&stored).to_string()
        }
    });
    // Only meaningful when `summary_template_select() == CUSTOM_TEMPLATE` — prefilled
    // with the existing custom text so re-opening Settings doesn't blank it out.
    let mut summary_template_custom_input = use_signal({
        let state = state.clone();
        move || {
            let stored = state.app_settings.lock().unwrap().summary_template.clone();
            if summary_template::select_key_for(&stored) == summary_template::CUSTOM_TEMPLATE {
                stored.unwrap_or_default()
            } else {
                String::new()
            }
        }
    });
    let mut summary_template_message = use_signal(|| None::<String>);

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
        let default_model = new_provider.default_model().to_string();
        model_input.set(default_model.clone());
        // Same "is this a `known_models()` entry, or should the freeform input show
        // instead" fallback as `model_select`'s own initial-value `use_signal` above.
        // Every provider except `Ollama` includes its `default_model()` as a
        // `known_models()` entry, so this only matters for `Ollama` (empty
        // `known_models()`) today — without it, switching to Ollama would select the
        // "カスタム..." option in the `<select>` while leaving the custom-text input
        // hidden, since that input's visibility is keyed off `model_select() ==
        // CUSTOM_MODEL`, not off `default_model()` directly.
        model_select.set(if new_provider.known_models().contains(&default_model.as_str()) { default_model } else { CUSTOM_MODEL.to_string() });
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

    // ==== Ollamaハンドラ ====

    // Persists `AppSettings::ollama_base_url` — `None` (falls back to `genai`'s own
    // `summarize::DEFAULT_OLLAMA_BASE_URL`) when the field is left blank, same "empty
    // input means unset, not an error" shape as `exports_root`. Same rollback-on-
    // failed-save pattern as `save_summary_template` below, so a write failure
    // (e.g. a temporarily read-only app_data_dir) doesn't leave the in-memory value
    // ahead of what's actually on disk.
    let save_ollama_base_url = {
        let state = state.clone();
        move |_| {
            let trimmed = ollama_base_url_input().trim().to_string();
            let new_value = if trimmed.is_empty() { None } else { Some(trimmed) };

            let save_result = {
                let mut settings = state.app_settings.lock().unwrap();
                let previous = settings.ollama_base_url.clone();
                settings.ollama_base_url = new_value;
                let result = settings.save(&state.app_data_dir);
                if result.is_err() {
                    settings.ollama_base_url = previous;
                }
                result
            };
            match save_result {
                Ok(()) => ollama_base_url_message.set(Some("保存しました".to_string())),
                Err(e) => ollama_base_url_message.set(Some(format!("保存に失敗しました: {e}"))),
            }
        }
    };

    // ==== 録音デバイスハンドラ ====

    // Re-runs `enumerate_capture_devices`/`enumerate_render_devices` — lets a
    // device plugged in (or unplugged) after Settings opened show up without
    // restarting the app. Factored out of the button's `onclick` (which needs a
    // `move |_evt|` closure) so `spawn_device_change_watcher` below can also call
    // it, without either call site fighting over which one "owns" the closure's
    // event-argument shape.
    let mut do_refresh_devices = move || {
        let (mic_list, mic_error) = load_capture_devices();
        let (render_list, render_error) = load_render_devices();
        mic_devices.set((mic_list.clone(), mic_error.clone()));
        render_devices.set((render_list.clone(), render_error.clone()));

        // See `reset_if_stale`'s doc comment: a device selected earlier in this
        // Settings session can disappear from the list a refresh just fetched.
        let mic_reset = reset_if_stale(mic_device_select, &mic_list);
        let render_reset = reset_if_stale(render_device_select, &render_list);

        device_message.set(match (mic_error, render_error) {
            (Some(e), _) => Some(format!("マイク一覧の取得に失敗しました: {e}")),
            (None, Some(e)) => Some(format!("スピーカー一覧の取得に失敗しました: {e}")),
            (None, None) => match (mic_reset, render_reset) {
                (true, true) => Some("選択していたマイクとスピーカーが見つからないため、システム既定に切り替えました。必要であれば選び直して保存してください。".to_string()),
                (true, false) => Some("選択していたマイクが見つからないため、システム既定に切り替えました。必要であれば選び直して保存してください。".to_string()),
                (false, true) => Some("選択していたスピーカーが見つからないため、システム既定に切り替えました。必要であれば選び直して保存してください。".to_string()),
                (false, false) => None,
            },
        });
    };
    let refresh_devices = move |_| do_refresh_devices();

    // OSからのデバイス着脱通知(Bluetoothヘッドセットの接続/切断など)を受け取り、
    // 手動でボタンを押さなくても一覧が更新されるようにする。
    spawn_device_change_watcher(do_refresh_devices);

    let onchange_mic_device = move |evt: FormEvent| {
        mic_device_select.set(evt.value());
        device_message.set(None);
    };
    let onchange_render_device = move |evt: FormEvent| {
        render_device_select.set(evt.value());
        device_message.set(None);
    };

    // Persists `AppSettings::microphone_device_id`/`render_device_id` —
    // `FOLLOW_SYSTEM_DEFAULT_DEVICE` maps back to `None` ("follow whatever the OS
    // reports as current default", the pre-existing behavior before this picker
    // existed). Same rollback-on-failed-save pattern as `save_ollama_base_url`
    // above.
    let save_devices = {
        let state = state.clone();
        move |_| {
            let mic = mic_device_select();
            let render = render_device_select();
            let new_mic = if mic == FOLLOW_SYSTEM_DEFAULT_DEVICE { None } else { Some(mic) };
            let new_render = if render == FOLLOW_SYSTEM_DEFAULT_DEVICE { None } else { Some(render) };

            let save_result = {
                let mut settings = state.app_settings.lock().unwrap();
                let previous_mic = settings.microphone_device_id.clone();
                let previous_render = settings.render_device_id.clone();
                settings.microphone_device_id = new_mic;
                settings.render_device_id = new_render;
                let result = settings.save(&state.app_data_dir);
                if result.is_err() {
                    settings.microphone_device_id = previous_mic;
                    settings.render_device_id = previous_render;
                }
                result
            };
            match save_result {
                Ok(()) => device_message.set(Some("保存しました。次回の録音開始時から反映されます".to_string())),
                Err(e) => device_message.set(Some(format!("保存に失敗しました: {e}"))),
            }
        }
    };

    // ==== 会話ヒント (RAG) ハンドラ ====

    let onchange_hint_enabled = move |evt: FormEvent| {
        hint_enabled_select.set(evt.checked());
        hint_message.set(None);
    };
    let onchange_hint_provider = move |evt: FormEvent| {
        hint_provider_select.set(evt.value());
        hint_message.set(None);
    };

    // Persists `AppSettings::hint_enabled`/`hint_provider`/`hint_debounce_seconds`
    // — same rollback-on-failed-save pattern as `save_devices` above. An empty
    // or unparseable debounce input falls back to 15 (matching
    // `spawn_hint_debounce_driver`'s own fallback when the setting is unset)
    // rather than saving garbage.
    let save_hint_settings = {
        let state = state.clone();
        move |_| {
            let enabled = hint_enabled_select();
            let provider = hint_provider_select();
            let debounce_seconds = hint_debounce_input().trim().parse::<u32>().ok().filter(|s| *s > 0).unwrap_or(15);
            hint_debounce_input.set(debounce_seconds.to_string());

            let save_result = {
                let mut settings = state.app_settings.lock().unwrap();
                let previous_enabled = settings.hint_enabled;
                let previous_provider = settings.hint_provider.clone();
                let previous_debounce = settings.hint_debounce_seconds;
                settings.hint_enabled = Some(enabled);
                settings.hint_provider = Some(provider);
                settings.hint_debounce_seconds = Some(debounce_seconds);
                let result = settings.save(&state.app_data_dir);
                if result.is_err() {
                    settings.hint_enabled = previous_enabled;
                    settings.hint_provider = previous_provider;
                    settings.hint_debounce_seconds = previous_debounce;
                }
                result
            };
            match save_result {
                Ok(()) => hint_message.set(Some("保存しました。次回の録音開始時から反映されます".to_string())),
                Err(e) => hint_message.set(Some(format!("保存に失敗しました: {e}"))),
            }
        }
    };

    // Saves the Cloudflare AI Search credential as one JSON blob under
    // `rhai_engine::{CLOUDFLARE_CREDENTIAL_SERVICE, CLOUDFLARE_AI_SEARCH_ACCOUNT}`
    // — same "collect several plain `<input>`s into a typed struct, serialize,
    // `credential_store.save`" shape as the summary Vertex AI credential above,
    // via the shared `rhai_engine::CloudflareCredentials` type so this can
    // never drift from the field names `rag/cloudflare.rs` actually reads.
    let save_hint_cloudflare_credential = {
        let state = state.clone();
        move |_| {
            let account_id = hint_cloudflare_account_id_input().trim().to_string();
            let api_token = hint_cloudflare_api_token_input().trim().to_string();
            let instance_name = hint_cloudflare_instance_input().trim().to_string();
            if account_id.is_empty() || api_token.is_empty() || instance_name.is_empty() {
                hint_message.set(Some("アカウントID・APIトークン・インスタンス名をすべて入力してください".to_string()));
                return;
            }

            let credentials = rhai_engine::CloudflareCredentials { account_id, api_token, instance_name };
            let serialized = match serde_json::to_string(&credentials) {
                Ok(s) => s,
                Err(e) => {
                    hint_message.set(Some(format!("資格情報のシリアライズに失敗しました: {e}")));
                    return;
                }
            };

            match state.credential_store.save(rhai_engine::CLOUDFLARE_CREDENTIAL_SERVICE, rhai_engine::CLOUDFLARE_AI_SEARCH_ACCOUNT, &serialized) {
                Ok(()) => {
                    hint_cloudflare_account_id_input.set(String::new());
                    hint_cloudflare_api_token_input.set(String::new());
                    hint_cloudflare_instance_input.set(String::new());
                    hint_message.set(Some("Cloudflareの資格情報を保存しました".to_string()));
                }
                Err(e) => hint_message.set(Some(format!("保存に失敗しました: {e}"))),
            }
        }
    };

    // ==== 要約プロンプトテンプレートハンドラ ====

    let onchange_summary_template = move |evt: FormEvent| {
        summary_template_select.set(evt.value());
        summary_template_message.set(None);
    };

    // Resolves the current selection ([`summary_template::NO_TEMPLATE`], a
    // built-in preset key, or [`summary_template::CUSTOM_TEMPLATE`]) into an
    // `AppSettings::summary_template` value and persists it via
    // `AppSettings::save` — the same non-secret store `export.rs`'s
    // `exports_root` already reads from (see `app_settings.rs`'s doc comment
    // for why this doesn't go through `credential_store`).
    let save_summary_template = {
        let state = state.clone();
        move |_| {
            let selected = summary_template_select();
            let new_value: Option<String> = if selected == summary_template::NO_TEMPLATE {
                None
            } else if selected == summary_template::CUSTOM_TEMPLATE {
                let text = summary_template_custom_input().trim().to_string();
                if text.is_empty() {
                    summary_template_message.set(Some("カスタムプロンプトを入力してください".to_string()));
                    return;
                }
                Some(text)
            } else {
                // Any other value is a preset key; fall back to `None` for a
                // stale/unknown key rather than panicking.
                SummaryTemplatePreset::from_key(&selected).map(|preset| preset.prompt().to_string())
            };

            let save_result = {
                let mut settings = state.app_settings.lock().unwrap();
                // Roll back to the pre-edit value if the write fails (Codex
                // review finding): without this, a failed save (e.g. a
                // temporarily read-only app_data_dir) would still leave the
                // in-memory `summary_template` on the new value, so the very
                // next summary generation would silently use an unsaved
                // template that reverts on the next restart — the opposite of
                // what "保存に失敗しました" is telling the user.
                let previous = settings.summary_template.clone();
                settings.summary_template = new_value;
                let result = settings.save(&state.app_data_dir);
                if result.is_err() {
                    settings.summary_template = previous;
                }
                result
            };
            match save_result {
                Ok(()) => summary_template_message.set(Some("保存しました".to_string())),
                Err(e) => summary_template_message.set(Some(format!("保存に失敗しました: {e}"))),
            }
        }
    };

    // Rust's `"{ident}"` format-string capture only accepts a bare identifier, not a
    // `module::CONST` path — bound to a local first so the hint text below can
    // interpolate it directly.
    let ollama_default_base_url = summarize::DEFAULT_OLLAMA_BASE_URL;

    rsx! {
        style { "{STYLE}" }
        main { class: "settings-container",
            div { class: "settings-header",
                button { onclick: move |_| screen.set(Screen::Main), "← 戻る" }
                h1 { "設定" }
            }

            section { class: "settings-section",
                h2 { "録音デバイス" }
                if device_selection_supported() {
                    p { class: "hint", "マイクやスピーカーが複数ある場合、録音に使うデバイスを選べます。「システム既定」を選ぶとOSの既定デバイスに追従します。" }
                    label {
                        "マイク(自分の音声)"
                        select {
                            onchange: onchange_mic_device,
                            option {
                                value: FOLLOW_SYSTEM_DEFAULT_DEVICE,
                                selected: mic_device_select() == FOLLOW_SYSTEM_DEFAULT_DEVICE,
                                "システム既定"
                            }
                            for d in mic_devices().0 {
                                option { value: "{d.id}", selected: mic_device_select() == d.id, "{d.option_label()}" }
                            }
                        }
                    }
                    // Shown regardless of which option is selected above — "シ
                    // ステム既定" itself doesn't reveal which physical device
                    // that resolves to, and even when a specific device is
                    // already pinned, it's useful to see whether the OS
                    // default has since drifted away from it.
                    if let Some(current) = mic_devices().0.iter().find(|d| d.is_default) {
                        p { class: "hint", "現在のシステム既定のマイク: {current.friendly_name}" }
                    }
                    label {
                        "スピーカー(相手の音声・ループバック)"
                        select {
                            onchange: onchange_render_device,
                            option {
                                value: FOLLOW_SYSTEM_DEFAULT_DEVICE,
                                selected: render_device_select() == FOLLOW_SYSTEM_DEFAULT_DEVICE,
                                "システム既定"
                            }
                            for d in render_devices().0 {
                                option { value: "{d.id}", selected: render_device_select() == d.id, "{d.option_label()}" }
                            }
                        }
                    }
                    if let Some(current) = render_devices().0.iter().find(|d| d.is_default) {
                        p { class: "hint", "現在のシステム既定のスピーカー: {current.friendly_name}" }
                    }
                    div { style: "display: flex; gap: 0.5em;",
                        button { class: "primary", onclick: save_devices, "この設定を保存" }
                        button { onclick: refresh_devices, "デバイス一覧を更新" }
                    }
                } else {
                    p { class: "hint", "このプラットフォームでは実際のマイク/スピーカー録音に対応していないため、デバイス選択はできません(開発用のスタブ録音のみ)。" }
                }
                if let Some(msg) = device_message() {
                    p { class: "status-badge", "{msg}" }
                }
            }

            section { class: "settings-section",
                h2 { "会話ヒント (RAG)" }
                p { class: "hint", "録音中の会話をもとに、RAGサービスから「今話すと良さそうなこと」のヒントをリアルタイムで表示します。BYOAI(ユーザー自身のアカウント)方式です。既定では無効 — 資格情報を設定してから有効にしてください。" }
                label { class: "consent",
                    input {
                        r#type: "checkbox",
                        checked: hint_enabled_select(),
                        onchange: onchange_hint_enabled,
                    }
                    "会話ヒントを有効にする"
                }

                if hint_enabled_select() {
                    label {
                        "使用するRAGプロバイダ"
                        select {
                            onchange: onchange_hint_provider,
                            option { value: "cloudflare", selected: hint_provider_select() == "cloudflare", "Cloudflare AI Search" }
                            option { value: "vertex", selected: hint_provider_select() == "vertex", "Google Vertex AI (要約用の資格情報を流用)" }
                            option { value: "bedrock", selected: hint_provider_select() == "bedrock", "AWS Bedrock (⚠️ 既知の問題により現在利用不可)" }
                        }
                    }
                    label {
                        "ヒント生成までの静寂時間(秒、最後の発話からこの秒数、会話が途切れたら生成)"
                        input {
                            r#type: "number",
                            min: "1",
                            value: "{hint_debounce_input}",
                            oninput: move |e| hint_debounce_input.set(e.value()),
                        }
                    }
                    if hint_provider_select() == "cloudflare" {
                        h3 { "Cloudflare AI Search の資格情報" }
                        p { class: "status-badge", if hint_cloudflare_is_configured(&state) { "設定済み" } else { "未設定" } }
                        label {
                            "アカウントID"
                            input {
                                r#type: "text",
                                value: "{hint_cloudflare_account_id_input}",
                                oninput: move |e| hint_cloudflare_account_id_input.set(e.value()),
                            }
                        }
                        label {
                            "APIトークン"
                            input {
                                r#type: "password",
                                value: "{hint_cloudflare_api_token_input}",
                                oninput: move |e| hint_cloudflare_api_token_input.set(e.value()),
                            }
                        }
                        label {
                            "インスタンス名"
                            input {
                                r#type: "text",
                                value: "{hint_cloudflare_instance_input}",
                                oninput: move |e| hint_cloudflare_instance_input.set(e.value()),
                            }
                        }
                        button { class: "primary", onclick: save_hint_cloudflare_credential, "資格情報を保存" }
                    } else if hint_provider_select() == "vertex" {
                        p { class: "hint", "要約機能の「Google Vertex AI」用に設定済みの資格情報をそのまま使用します。追加の設定は不要です。" }
                    } else {
                        p { class: "hint", "AWS Bedrock RAGは、要約機能のBedrock APIキーと資格情報の形式が競合する既知の問題により、現時点では動作しません。" }
                    }
                }
                button { class: "primary", onclick: save_hint_settings, "この設定を保存" }
                if let Some(msg) = hint_message() {
                    p { class: "status-badge", "{msg}" }
                }
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
                } else if summary_edit_provider() == SummaryProvider::ClaudeOAuth {
                    // Like the CLI-based providers above, no API key of this app's
                    // own to save — this one authenticates as the official `ant`
                    // CLI's own OAuth login instead of `claude`/`codex`'s.
                    p { class: "hint", "APIキーは不要です。事前に `ant auth login` でログインしておいてください。要約自体は claude CLIのサブプロセスではなく、antが発行したトークンで直接APIを呼びます。" }
                    p { class: "status-badge", "{summary_cli_status_text(summary_edit_provider(), summary_cli_available())}" }
                } else if summary_edit_provider() == SummaryProvider::Ollama {
                    // Local Ollama server (`SummaryProvider::api_key_account() ==
                    // None`) — no credential form here; without this branch the
                    // `else` below would still render the API key input, but
                    // `save_summary_credential`'s `api_key_account()` guard silently
                    // no-ops on save, so the input would look functional while doing
                    // nothing.
                    p { class: "hint", "このプロバイダはAPIキー不要です。下の「Ollama設定」でサーバーのbase URLを設定してください。" }
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

            section { class: "settings-section",
                h2 { "Ollama設定" }
                p { class: "hint", "要約プロバイダに「Ollama (ローカル)」を選んだときに接続するローカルサーバーのbase URLです。プロバイダの選択とは独立に登録しておけます。" }
                label {
                    "base URL"
                    input {
                        r#type: "text",
                        placeholder: "{ollama_default_base_url}",
                        value: "{ollama_base_url_input}",
                        oninput: move |e| ollama_base_url_input.set(e.value()),
                    }
                }
                p { class: "hint", "未入力のまま保存すると既定値({ollama_default_base_url})を使用します。" }
                button { class: "primary", onclick: save_ollama_base_url, "この設定を保存" }
                if let Some(msg) = ollama_base_url_message() {
                    p { class: "status-badge", "{msg}" }
                }
            }

            section { class: "settings-section",
                h2 { "要約プロンプトテンプレート" }
                p { class: "hint", "要約生成時にLLMへ渡すシステムプロンプトを選べます。使用する要約プロバイダ・モデルの選択とは独立です。" }
                label {
                    "テンプレート"
                    select {
                        onchange: onchange_summary_template,
                        option {
                            value: summary_template::NO_TEMPLATE,
                            selected: summary_template_select() == summary_template::NO_TEMPLATE,
                            "組み込みデフォルト"
                        }
                        for preset in SummaryTemplatePreset::ALL {
                            option {
                                value: "{preset.key()}",
                                selected: summary_template_select() == preset.key(),
                                "{preset.label()}"
                            }
                        }
                        option {
                            value: summary_template::CUSTOM_TEMPLATE,
                            selected: summary_template_select() == summary_template::CUSTOM_TEMPLATE,
                            "カスタム..."
                        }
                    }
                }
                if summary_template_select() == summary_template::CUSTOM_TEMPLATE {
                    label {
                        "カスタムプロンプト"
                        textarea {
                            placeholder: "要約生成時にLLMに渡すシステムプロンプトを入力",
                            value: "{summary_template_custom_input}",
                            oninput: move |e| summary_template_custom_input.set(e.value()),
                        }
                    }
                } else if let Some(preset) = SummaryTemplatePreset::from_key(&summary_template_select()) {
                    p { class: "hint", "プレビュー:" }
                    div { class: "summary-template-preview", "{preset.prompt()}" }
                }
                button { class: "primary", onclick: save_summary_template, "この設定を保存" }
                if let Some(msg) = summary_template_message() {
                    p { class: "status-badge", "{msg}" }
                }
            }
        }
    }
}
