//! Settings screen (#31 Deepgram API key, #37 summary LLM provider + API key).
//! Reachable from `ui::App` via a `Signal<Screen>` swap — no router crate needed
//! for two screens. Secrets are never displayed once saved (`credential-store`
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
}

impl SummaryProvider {
    const ALL: [SummaryProvider; 2] = [SummaryProvider::Claude, SummaryProvider::OpenAi];

    pub(crate) fn key(self) -> &'static str {
        match self {
            SummaryProvider::Claude => "claude",
            SummaryProvider::OpenAi => "openai",
        }
    }

    fn label(self) -> &'static str {
        match self {
            SummaryProvider::Claude => "Claude (Anthropic)",
            SummaryProvider::OpenAi => "OpenAI",
        }
    }

    pub(crate) fn default_model(self) -> &'static str {
        match self {
            SummaryProvider::Claude => "claude-sonnet-4-5",
            SummaryProvider::OpenAi => "gpt-4o-mini",
        }
    }

    pub(crate) fn api_key_account(self) -> &'static str {
        match self {
            SummaryProvider::Claude => summarize::CLAUDE_API_KEY_ACCOUNT,
            SummaryProvider::OpenAi => summarize::OPENAI_API_KEY_ACCOUNT,
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
.settings-section select {
  padding: 0.5em;
  border-radius: 6px;
  border: 1px solid #555;
  font-size: 1em;
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

    let mut deepgram_configured = use_signal({
        let state = state.clone();
        move || state.credential_store.load(stt_deepgram::CREDENTIAL_SERVICE, stt_deepgram::DEEPGRAM_API_KEY_ACCOUNT).is_ok()
    });
    let mut deepgram_key_input = use_signal(String::new);
    let mut deepgram_message = use_signal(|| None::<String>);

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

    let save_deepgram = {
        let state = state.clone();
        move |_| {
            let key = deepgram_key_input().trim().to_string();
            if key.is_empty() {
                deepgram_message.set(Some("APIキーを入力してください".to_string()));
                return;
            }
            match state.credential_store.save(stt_deepgram::CREDENTIAL_SERVICE, stt_deepgram::DEEPGRAM_API_KEY_ACCOUNT, &key) {
                Ok(()) => {
                    deepgram_configured.set(true);
                    deepgram_key_input.set(String::new());
                    deepgram_message.set(Some("保存しました".to_string()));
                }
                Err(e) => deepgram_message.set(Some(format!("保存に失敗しました: {e}"))),
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
                h2 { "Deepgram (音声認識)" }
                p { class: "status-badge", if deepgram_configured() { "設定済み" } else { "未設定" } }
                label {
                    "APIキー"
                    input {
                        r#type: "password",
                        placeholder: "Deepgram APIキー",
                        value: "{deepgram_key_input}",
                        oninput: move |e| deepgram_key_input.set(e.value()),
                    }
                }
                button { class: "primary", onclick: save_deepgram, "保存" }
                if let Some(msg) = deepgram_message() {
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
