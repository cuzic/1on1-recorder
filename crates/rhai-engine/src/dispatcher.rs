//! Generic async command dispatcher.
//!
//! Rhai scripts call `call_async(name, args)` which bridges through an
//! MPSC channel to this async worker. New commands are added by:
//! 1. Adding a handler function here
//! 2. Adding a `match` arm in `async_worker`
//! 3. Adding a curry wrapper in `std.rhai`

use std::sync::Arc;

use credential_store::CredentialStore;
use local_broker::LocalBroker;
use recorder_domain::SessionId;
use session_store::SessionStore;
use summarize::{Summarizer, TranscriptTurn};

use crate::SettingsProvider;

pub struct AsyncCommand {
    pub name: String,
    pub args: rhai::Map,
    pub reply: std::sync::mpsc::Sender<Result<rhai::Dynamic, String>>,
}

pub async fn async_worker(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<AsyncCommand>,
    broker: LocalBroker,
    store: Arc<SessionStore>,
    credential_store: Arc<credential_store::FallbackCredentialStore>,
    settings: Arc<dyn SettingsProvider>,
) {
    while let Some(cmd) = rx.recv().await {
        let result = dispatch(&cmd, &broker, &store, &credential_store, &*settings).await;
        cmd.reply.send(result).ok();
    }
}

async fn dispatch(
    cmd: &AsyncCommand,
    broker: &LocalBroker,
    store: &SessionStore,
    credential_store: &Arc<credential_store::FallbackCredentialStore>,
    settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    match cmd.name.as_str() {
        "ai_summarize" => handle_ai_summarize(&cmd.args, credential_store, settings).await,
        "list_segments" => handle_list_segments(&cmd.args, store),
        "save_summary" => handle_save_summary(&cmd.args, store),
        "publish_event" => handle_publish_event(&cmd.args, broker),
        "get_setting" => handle_get_setting(&cmd.args, settings),
        "get_selected_model" => handle_get_selected_model(settings),
        "get_session_metadata" => handle_get_session_metadata(&cmd.args, settings),
        "format_turns" => handle_format_turns(&cmd.args),
        "rag_search" => crate::rag::rag_search(&cmd.args, credential_store, settings).await,
        "rag_index" => crate::rag::rag_index(&cmd.args, credential_store, settings).await,
        _ => Err(format!("unknown async command: {}", cmd.name)),
    }
}

async fn handle_ai_summarize(
    args: &rhai::Map,
    credential_store: &Arc<credential_store::FallbackCredentialStore>,
    settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let model = get_string(args, "model")?;
    let system_prompt = get_string(args, "system_prompt")?;
    let turns_raw = args.get("turns").ok_or("missing argument: turns")?;

    let turns: Vec<TranscriptTurn> = if let Some(arr) = turns_raw.clone().try_cast::<rhai::Array>() {
        arr.iter()
            .filter_map(|v| {
                let m = v.clone().try_cast::<rhai::Map>()?;
                let speaker = m.get("speaker")?.to_string();
                let text = m.get("text")?.to_string();
                Some(TranscriptTurn { speaker: Some(speaker), text })
            })
            .collect()
    } else {
        return Err("turns must be an array".into());
    };

    let options = summarize::SummarizeOptions::new(&model)
        .with_system_prompt(system_prompt);

    let summarizer = build_summarizer(&model, credential_store, settings)?;
    let text = summarizer.summarize(&turns, &options).await.map_err(|e| e.to_string())?;
    Ok(rhai::Dynamic::from(text))
}

fn build_summarizer(
    _model: &str,
    credential_store: &Arc<credential_store::FallbackCredentialStore>,
    settings: &dyn SettingsProvider,
) -> Result<Box<dyn Summarizer>, String> {
    let provider_key = settings.get("summary_provider_key").unwrap_or_else(|| "claude".to_string());
    let provider = ProviderInfo::from_key(&provider_key);

    if let Some(backend) = provider.cli_backend() {
        return Ok(Box::new(summarize::CliSummarizer(backend)));
    }

    if provider.is_vertex() {
        let account = provider.api_key_account().unwrap_or("");
        let raw = credential_store
            .load(summarize::CREDENTIAL_SERVICE, account)
            .unwrap_or_default();
        let credentials = serde_json::from_str::<summarize::VertexCredentials>(&raw)
            .map_err(|e| format!("Vertex認証情報の読み込みに失敗: {e}"))?;
        return Ok(Box::new(summarize::GenaiSummarizer(summarize::build_vertex_client(credentials))));
    }

    if provider_key == "ollama" {
        let base_url = settings.get("ollama_base_url");
        return Ok(Box::new(summarize::GenaiSummarizer(summarize::build_ollama_client(base_url))));
    }

    if let Some(account) = provider.api_key_account() {
        let resolver = summarize::credential_store_auth_resolver(
            Arc::clone(credential_store),
            account.to_string(),
        );
        let client = genai::Client::builder().with_auth_resolver(resolver).build();
        return Ok(Box::new(summarize::GenaiSummarizer(client)));
    }

    let client = genai::Client::builder().build();
    Ok(Box::new(summarize::GenaiSummarizer(client)))
}

fn handle_list_segments(
    args: &rhai::Map,
    store: &SessionStore,
) -> Result<rhai::Dynamic, String> {
    let session_id_str = get_string(args, "session_id")?;
    let session_id: SessionId = session_id_str.parse().map_err(|e| format!("invalid session_id: {e}"))?;
    let segments = store.list_transcript_segments(session_id).map_err(|e| e.to_string())?;

    let mut arr = rhai::Array::new();
    for seg in &segments {
        let mut m = rhai::Map::new();
        m.insert("segment_id".into(), rhai::Dynamic::from(
            transcript_event::segment_id_for_segment(seg.session_id, seg.track, seg.start_ms, seg.end_ms)
        ));
        m.insert("text".into(), rhai::Dynamic::from(seg.text.clone()));
        m.insert("speaker_label".into(), rhai::Dynamic::from(speaker_label(seg.track, seg.speaker)));
        m.insert("is_final".into(), rhai::Dynamic::from(seg.is_final));
        m.insert("start_ms".into(), rhai::Dynamic::from(seg.start_ms.unwrap_or(0) as i64));
        m.insert("end_ms".into(), rhai::Dynamic::from(seg.end_ms.unwrap_or(0) as i64));
        arr.push(rhai::Dynamic::from_map(m));
    }
    Ok(rhai::Dynamic::from_array(arr))
}

fn handle_save_summary(
    args: &rhai::Map,
    store: &SessionStore,
) -> Result<rhai::Dynamic, String> {
    let session_id_str = get_string(args, "session_id")?;
    let session_id: SessionId = session_id_str.parse().map_err(|e| format!("invalid session_id: {e}"))?;
    let text = get_string(args, "text")?;
    let provider_model = get_string(args, "provider_model")?;

    let summary = session_store::Summary {
        session_id,
        text,
        provider_model,
        generated_at: chrono::Utc::now(),
    };
    store.insert_summary(&summary).map_err(|e| e.to_string())?;
    Ok(rhai::Dynamic::UNIT)
}

fn handle_publish_event(
    args: &rhai::Map,
    broker: &LocalBroker,
) -> Result<rhai::Dynamic, String> {
    let subject = get_string(args, "subject")?;
    // Pass through as raw map — the event is already structured
    let data = args.get("data").cloned().unwrap_or(rhai::Dynamic::UNIT);
    let json = serde_json::to_vec(&rhai_dynamic_to_value(&data)).map_err(|e| e.to_string())?;
    broker.publish_bytes(&subject, json).map_err(|e| e.to_string())?;
    Ok(rhai::Dynamic::UNIT)
}

fn handle_get_setting(
    args: &rhai::Map,
    settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let key = get_string(args, "key")?;
    Ok(settings.get(&key).map(rhai::Dynamic::from).unwrap_or(rhai::Dynamic::UNIT))
}

fn handle_get_selected_model(
    settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    Ok(rhai::Dynamic::from(settings.selected_model()))
}

fn handle_get_session_metadata(
    args: &rhai::Map,
    settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let session_id_str = get_string(args, "session_id")?;
    let session_id: SessionId = session_id_str.parse().map_err(|e| format!("invalid session_id: {e}"))?;
    Ok(rhai::Dynamic::from_map(settings.session_metadata(session_id)))
}

fn handle_format_turns(
    args: &rhai::Map,
) -> Result<rhai::Dynamic, String> {
    let turns_raw = args.get("turns").ok_or("missing argument: turns")?;
    let format = args.get("format")
        .and_then(|v| v.clone().try_cast::<String>())
        .unwrap_or_else(|| "text".to_string());

    let arr = turns_raw.clone().try_cast::<rhai::Array>().ok_or("turns must be an array")?;
    let mut output = String::new();
    for turn in arr.iter() {
        if let Some(m) = turn.clone().try_cast::<rhai::Map>() {
            let speaker = m.get("speaker").map(|s| s.to_string()).unwrap_or_else(|| "不明".into());
            let text = m.get("text").map(|s| s.to_string()).unwrap_or_default();
            match format.as_str() {
                "markdown" => output.push_str(&format!("**{speaker}**: {text}\n\n")),
                _ => output.push_str(&format!("[{speaker}]: {text}\n")),
            }
        }
    }
    Ok(rhai::Dynamic::from(output))
}

fn get_string(args: &rhai::Map, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.clone().try_cast::<String>())
        .ok_or_else(|| format!("missing required argument: {key}"))
}

fn speaker_label(track: Option<recorder_domain::TrackKind>, speaker: Option<u32>) -> String {
    let base = match track {
        Some(recorder_domain::TrackKind::SelfMic) => "自分",
        Some(recorder_domain::TrackKind::RemoteAudio) => "相手",
        None => "不明",
    };
    match speaker {
        Some(n) => format!("{base} (話者{})", n + 1),
        None => base.to_string(),
    }
}

/// Converts a rhai::Dynamic to a serde_json::Value for publishing.
fn rhai_dynamic_to_value(d: &rhai::Dynamic) -> serde_json::Value {
    if d.is::<rhai::Map>() {
        let map = d.clone().try_cast::<rhai::Map>().unwrap();
        let mut obj = serde_json::Map::new();
        for (k, v) in map.iter() {
            obj.insert(k.to_string(), rhai_dynamic_to_value(v));
        }
        serde_json::Value::Object(obj)
    } else if d.is::<rhai::Array>() {
        let arr = d.clone().try_cast::<rhai::Array>().unwrap();
        serde_json::Value::Array(arr.iter().map(rhai_dynamic_to_value).collect())
    } else if d.is::<i64>() {
        serde_json::Value::Number(d.clone().try_cast::<i64>().unwrap().into())
    } else if d.is::<f64>() {
        serde_json::Number::from_f64(d.clone().try_cast::<f64>().unwrap()).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null)
    } else if d.is::<bool>() {
        serde_json::Value::Bool(d.clone().try_cast::<bool>().unwrap())
    } else if d.is::<String>() {
        serde_json::Value::String(d.clone().try_cast::<String>().unwrap_or_default())
    } else if d.is_unit() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(d.to_string())
    }
}

// ── Provider info (duplicated from desktop crate's SummaryProvider) ──

struct ProviderInfo;

impl ProviderInfo {
    fn from_key(key: &str) -> ProviderVariant {
        match key {
            "openai" => ProviderVariant::OpenAi,
            "gemini" => ProviderVariant::Gemini,
            "groq" => ProviderVariant::Groq,
            "deepseek" => ProviderVariant::DeepSeek,
            "xai" => ProviderVariant::Xai,
            "claude-vertex" => ProviderVariant::ClaudeVertex,
            "gemini-vertex" => ProviderVariant::GeminiVertex,
            "claude-bedrock" => ProviderVariant::ClaudeBedrock,
            "claude-cli" => ProviderVariant::ClaudeCli,
            "codex" => ProviderVariant::CodexCli,
            "ollama" => ProviderVariant::Ollama,
            _ => ProviderVariant::Claude,
        }
    }
}

enum ProviderVariant {
    Claude, OpenAi, Gemini, Groq, DeepSeek, Xai,
    ClaudeVertex, GeminiVertex, ClaudeBedrock,
    ClaudeCli, CodexCli, Ollama,
}

impl ProviderVariant {
    fn api_key_account(&self) -> Option<&str> {
        match self {
            Self::Claude => Some(summarize::CLAUDE_API_KEY_ACCOUNT),
            Self::OpenAi => Some(summarize::OPENAI_API_KEY_ACCOUNT),
            Self::Gemini => Some(summarize::GEMINI_API_KEY_ACCOUNT),
            Self::Groq => Some(summarize::GROQ_API_KEY_ACCOUNT),
            Self::DeepSeek => Some(summarize::DEEPSEEK_API_KEY_ACCOUNT),
            Self::Xai => Some(summarize::XAI_API_KEY_ACCOUNT),
            Self::ClaudeVertex => Some(summarize::CLAUDE_VERTEX_CREDENTIALS_ACCOUNT),
            Self::GeminiVertex => Some(summarize::GEMINI_VERTEX_CREDENTIALS_ACCOUNT),
            Self::ClaudeBedrock => Some(summarize::BEDROCK_API_KEY_ACCOUNT),
            Self::ClaudeCli | Self::CodexCli | Self::Ollama => None,
        }
    }

    fn cli_backend(&self) -> Option<summarize::cli_backend::CliBackend> {
        match self {
            Self::ClaudeCli => Some(summarize::cli_backend::CliBackend::ClaudeCode),
            Self::CodexCli => Some(summarize::cli_backend::CliBackend::Codex),
            _ => None,
        }
    }

    fn is_vertex(&self) -> bool {
        matches!(self, Self::ClaudeVertex | Self::GeminiVertex)
    }
}