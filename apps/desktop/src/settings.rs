//! Settings screen (#31/#49 STT provider + credentials, #37 summary LLM provider +
//! API key). Reachable from `ui::App` via a `Signal<Screen>` swap — no router crate
//! needed for two screens. Secrets are never displayed once saved (`credential-store`
//! `load` is only used to decide "設定済み/未設定", not to populate an input).

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
    /// `save_stt` below).
    fn api_key_service_account(self) -> Option<(&'static str, &'static str)> {
        match self {
            SttProvider::Deepgram => Some((stt_deepgram::CREDENTIAL_SERVICE, stt_deepgram::DEEPGRAM_API_KEY_ACCOUNT)),
            SttProvider::OpenAi => Some((stt_openai::CREDENTIAL_SERVICE, stt_openai::OPENAI_STT_API_KEY_ACCOUNT)),
            SttProvider::AssemblyAi => Some((stt_assemblyai::CREDENTIAL_SERVICE, stt_assemblyai::ASSEMBLYAI_API_KEY_ACCOUNT)),
            SttProvider::Google => None,
        }
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
}

impl SummaryProvider {
    const ALL: [SummaryProvider; 6] = [
        SummaryProvider::Claude,
        SummaryProvider::OpenAi,
        SummaryProvider::Gemini,
        SummaryProvider::Groq,
        SummaryProvider::DeepSeek,
        SummaryProvider::XAi,
    ];

    pub(crate) fn key(self) -> &'static str {
        match self {
            SummaryProvider::Claude => "claude",
            SummaryProvider::OpenAi => "openai",
            SummaryProvider::Gemini => "gemini",
            SummaryProvider::Groq => "groq",
            SummaryProvider::DeepSeek => "deepseek",
            SummaryProvider::XAi => "xai",
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
        }
    }

    pub(crate) fn api_key_account(self) -> &'static str {
        match self {
            SummaryProvider::Claude => summarize::CLAUDE_API_KEY_ACCOUNT,
            SummaryProvider::OpenAi => summarize::OPENAI_API_KEY_ACCOUNT,
            SummaryProvider::Gemini => summarize::GEMINI_API_KEY_ACCOUNT,
            SummaryProvider::Groq => summarize::GROQ_API_KEY_ACCOUNT,
            SummaryProvider::DeepSeek => summarize::DEEPSEEK_API_KEY_ACCOUNT,
            SummaryProvider::XAi => summarize::XAI_API_KEY_ACCOUNT,
        }
    }

    pub(crate) fn from_key(key: &str) -> Self {
        Self::ALL.into_iter().find(|p| p.key() == key).unwrap_or(SummaryProvider::Claude)
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

    let mut stt_provider = use_signal({
        let state = state.clone();
        move || {
            state
                .credential_store
                .load(app_service::CREDENTIAL_SERVICE, app_service::SELECTED_STT_PROVIDER_ACCOUNT)
                .ok()
                .map(|key| SttProvider::from_key(&key))
                .unwrap_or(SttProvider::Deepgram)
        }
    });
    let mut stt_configured = use_signal({
        let state = state.clone();
        move || stt_provider_is_configured(&state, stt_provider())
    });
    let mut stt_key_input = use_signal(String::new);
    let mut stt_google_project_input = use_signal(String::new);
    let mut stt_google_location_input = use_signal(|| "global".to_string());
    let mut stt_google_json_input = use_signal(String::new);
    let mut stt_message = use_signal(|| None::<String>);

    let mut provider = use_signal({
        let state = state.clone();
        move || {
            state
                .credential_store
                .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT)
                .ok()
                .map(|key| SummaryProvider::from_key(&key))
                .unwrap_or(SummaryProvider::Claude)
        }
    });
    let mut model_input = use_signal({
        let state = state.clone();
        move || {
            state
                .credential_store
                .load(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_MODEL_ACCOUNT)
                .unwrap_or_else(|_| provider().default_model().to_string())
        }
    });
    let mut provider_key_configured = use_signal({
        let state = state.clone();
        move || state.credential_store.load(summarize::CREDENTIAL_SERVICE, provider().api_key_account()).is_ok()
    });
    let mut provider_key_input = use_signal(String::new);
    let mut summary_message = use_signal(|| None::<String>);

    let onchange_stt_provider = {
        let state = state.clone();
        move |evt: FormEvent| {
            let new_provider = SttProvider::from_key(&evt.value());
            stt_provider.set(new_provider);
            stt_key_input.set(String::new());
            stt_google_project_input.set(String::new());
            stt_google_location_input.set("global".to_string());
            stt_google_json_input.set(String::new());
            stt_configured.set(stt_provider_is_configured(&state, new_provider));
            stt_message.set(None);
        }
    };

    let save_stt = {
        let state = state.clone();
        move |_| {
            let current_provider = stt_provider();
            if let Err(e) = state.credential_store.save(app_service::CREDENTIAL_SERVICE, app_service::SELECTED_STT_PROVIDER_ACCOUNT, current_provider.key()) {
                stt_message.set(Some(format!("保存に失敗しました: {e}")));
                return;
            }

            if current_provider.is_google() {
                let project = stt_google_project_input().trim().to_string();
                let location = stt_google_location_input().trim().to_string();
                let json = stt_google_json_input().trim().to_string();
                if project.is_empty() || location.is_empty() || json.is_empty() {
                    stt_message.set(Some("プロジェクトID・ロケーション・サービスアカウントJSONをすべて入力してください".to_string()));
                    return;
                }
                let credentials = stt_google::GoogleSttCredentials::new(project, location).with_service_account_json(json);
                let serialized = match serde_json::to_string(&credentials) {
                    Ok(s) => s,
                    Err(e) => {
                        stt_message.set(Some(format!("認証情報の変換に失敗しました: {e}")));
                        return;
                    }
                };
                match state.credential_store.save(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT, &serialized) {
                    Ok(()) => {
                        stt_configured.set(true);
                        stt_google_project_input.set(String::new());
                        stt_google_location_input.set("global".to_string());
                        stt_google_json_input.set(String::new());
                        stt_message.set(Some("保存しました".to_string()));
                    }
                    Err(e) => stt_message.set(Some(format!("保存に失敗しました: {e}"))),
                }
                return;
            }

            let key = stt_key_input().trim().to_string();
            if key.is_empty() {
                stt_message.set(Some("APIキーを入力してください".to_string()));
                return;
            }
            // `SttProvider::Google` is handled above and always returns before here,
            // so this is always `Some` — see `SttProvider::api_key_service_account`.
            let Some((service, account)) = current_provider.api_key_service_account() else {
                stt_message.set(Some("未対応のプロバイダです".to_string()));
                return;
            };
            match state.credential_store.save(service, account, &key) {
                Ok(()) => {
                    stt_configured.set(true);
                    stt_key_input.set(String::new());
                    stt_message.set(Some("保存しました".to_string()));
                }
                Err(e) => stt_message.set(Some(format!("保存に失敗しました: {e}"))),
            }
        }
    };

    let onchange_provider = {
        let state = state.clone();
        move |evt: FormEvent| {
            let new_provider = SummaryProvider::from_key(&evt.value());
            provider.set(new_provider);
            model_input.set(new_provider.default_model().to_string());
            provider_key_input.set(String::new());
            provider_key_configured.set(state.credential_store.load(summarize::CREDENTIAL_SERVICE, new_provider.api_key_account()).is_ok());
            summary_message.set(None);
        }
    };

    let save_summary = {
        let state = state.clone();
        move |_| {
            let current_provider = provider();
            let model = {
                let m = model_input().trim().to_string();
                if m.is_empty() {
                    current_provider.default_model().to_string()
                } else {
                    m
                }
            };

            if let Err(e) = state.credential_store.save(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_PROVIDER_ACCOUNT, current_provider.key()) {
                summary_message.set(Some(format!("保存に失敗しました: {e}")));
                return;
            }
            if let Err(e) = state.credential_store.save(summarize::CREDENTIAL_SERVICE, summarize::SELECTED_MODEL_ACCOUNT, &model) {
                summary_message.set(Some(format!("保存に失敗しました: {e}")));
                return;
            }
            model_input.set(model);

            let key = provider_key_input().trim().to_string();
            if !key.is_empty() {
                match state.credential_store.save(summarize::CREDENTIAL_SERVICE, current_provider.api_key_account(), &key) {
                    Ok(()) => {
                        provider_key_configured.set(true);
                        provider_key_input.set(String::new());
                    }
                    Err(e) => {
                        summary_message.set(Some(format!("APIキーの保存に失敗しました: {e}")));
                        return;
                    }
                }
            }
            summary_message.set(Some("保存しました".to_string()));
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
                h2 { "音声認識 (STT)" }
                label {
                    "プロバイダ"
                    select {
                        onchange: onchange_stt_provider,
                        for p in SttProvider::ALL {
                            option { value: "{p.key()}", selected: p == stt_provider(), "{p.label()}" }
                        }
                    }
                }
                p { class: "status-badge", if stt_configured() { "設定済み" } else { "未設定" } }
                if stt_provider().is_google() {
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
                button { class: "primary", onclick: save_stt, "保存" }
                if let Some(msg) = stt_message() {
                    p { class: "status-badge", "{msg}" }
                }
            }

            section { class: "settings-section",
                h2 { "要約 (LLM)" }
                label {
                    "プロバイダ"
                    select {
                        onchange: onchange_provider,
                        for p in SummaryProvider::ALL {
                            option { value: "{p.key()}", selected: p == provider(), "{p.label()}" }
                        }
                    }
                }
                label {
                    "モデル"
                    input {
                        r#type: "text",
                        value: "{model_input}",
                        oninput: move |e| model_input.set(e.value()),
                    }
                }
                p { class: "status-badge", if provider_key_configured() { "APIキー設定済み" } else { "APIキー未設定" } }
                label {
                    "APIキー"
                    input {
                        r#type: "password",
                        placeholder: "APIキー",
                        value: "{provider_key_input}",
                        oninput: move |e| provider_key_input.set(e.value()),
                    }
                }
                button { class: "primary", onclick: save_summary, "保存" }
                if let Some(msg) = summary_message() {
                    p { class: "status-badge", "{msg}" }
                }
            }
        }
    }
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
