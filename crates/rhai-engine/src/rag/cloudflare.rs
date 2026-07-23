//! Cloudflare AI Search (formerly "AutoRAG") integration.
//!
//! Bring-your-own-account: the user configures their own Cloudflare account ID,
//! API token, and AI Search instance name via the settings screen; this crate
//! only queries an already-configured instance, it never creates or indexes one
//! (see `index()` below).
//!
//! **Unverified against a live account** — the request/response shapes below are
//! based on Cloudflare's REST API reference as of this writing, not a live call
//! from this sandbox (no network egress to Cloudflare here). Same caveat as
//! `vertex.rs`/`bedrock.rs` in this module: verify against a real account before
//! relying on it, and expect to adjust field names in `parse_chunk` if Cloudflare's
//! actual response shape differs.

use std::sync::Arc;

use credential_store::CredentialStore;
use serde::{Deserialize, Serialize};

use crate::SettingsProvider;

/// Deliberately distinct from every `summarize::*_ACCOUNT` constant (see
/// `rag/bedrock.rs`'s doc comment on the collision bug this avoids) — this is a
/// dedicated credential slot, not shared with the summarize feature's own
/// provider credentials.
pub const CREDENTIAL_SERVICE: &str = "1on1-recorder";
pub const CLOUDFLARE_AI_SEARCH_ACCOUNT: &str = "cloudflare-ai-search-credentials";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareCredentials {
    pub account_id: String,
    pub api_token: String,
    pub instance_name: String,
}

pub async fn search(
    query: &str,
    options: &rhai::Map,
    credential_store: &Arc<credential_store::FallbackCredentialStore>,
    _settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let max_results = options.get("max_results").and_then(|v| v.as_int().ok()).unwrap_or(5) as usize;

    let credentials = load_credentials(credential_store)?;
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai-search/instances/{}/search",
        credentials.account_id, credentials.instance_name
    );

    let body = serde_json::json!({
        "messages": [{ "role": "user", "content": query }],
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", credentials.api_token))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Cloudflare AI Search request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Cloudflare AI Search returned {status}: {text}"));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| format!("failed to parse response: {e}"))?;

    let mut results = rhai::Array::new();
    for chunk in extract_chunks(&json).into_iter().take(max_results) {
        results.push(rhai::Dynamic::from_map(parse_chunk(chunk)));
    }
    Ok(rhai::Dynamic::from_array(results))
}

/// Cloudflare's own account-management REST API (not this app) is how an AI
/// Search instance and its document corpus get created/populated — this app
/// only ever queries an instance the user already set up, so there is no
/// document-upload path to implement here. Mirrors `bedrock.rs::index()`'s
/// same "out of scope, explain why" shape rather than erroring outright.
pub async fn index(
    _documents: &rhai::Dynamic,
    _options: &rhai::Map,
    _credential_store: &Arc<credential_store::FallbackCredentialStore>,
    _settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let mut m = rhai::Map::new();
    m.insert("indexed".into(), rhai::Dynamic::from(0_i64));
    m.insert(
        "message".into(),
        rhai::Dynamic::from("Cloudflare AI Search instances are configured/populated in the Cloudflare dashboard, not from this app.".to_string()),
    );
    Ok(rhai::Dynamic::from_map(m))
}

fn load_credentials(credential_store: &Arc<credential_store::FallbackCredentialStore>) -> Result<CloudflareCredentials, String> {
    let raw = credential_store
        .load(CREDENTIAL_SERVICE, CLOUDFLARE_AI_SEARCH_ACCOUNT)
        .map_err(|_| "Cloudflare AI Search credentials not configured".to_string())?;
    serde_json::from_str(&raw).map_err(|e| format!("invalid Cloudflare credentials JSON: {e}"))
}

/// Cloudflare's `/search` response wraps its chunk array somewhere under
/// `result` (matching the rest of Cloudflare's v4 API convention of a top-level
/// `{success, result, errors}` envelope) — tries a couple of plausible
/// locations rather than committing to one exact shape, since this hasn't been
/// verified against a live response (see this module's doc comment).
fn extract_chunks(json: &serde_json::Value) -> Vec<&serde_json::Value> {
    for path in [&["result", "data"][..], &["result", "chunks"][..], &["data"][..], &["chunks"][..]] {
        let mut cur = json;
        let mut ok = true;
        for key in path {
            match cur.get(key) {
                Some(next) => cur = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(arr) = cur.as_array() {
                return arr.iter().collect();
            }
        }
    }
    Vec::new()
}

/// Maps a Cloudflare chunk to the same `#{text, score, source}`-ish shape
/// `vertex.rs`/`bedrock.rs` already return, so `rag_search`'s Rhai-side callers
/// don't need per-provider branching. Field names (`text`, `content`, `score`,
/// `item.key`) are best-effort per Cloudflare's docs, not confirmed live.
fn parse_chunk(chunk: &serde_json::Value) -> rhai::Map {
    let mut m = rhai::Map::new();
    let text = chunk.get("text").or_else(|| chunk.get("content")).and_then(|v| v.as_str()).unwrap_or("");
    m.insert("text".into(), rhai::Dynamic::from(text.to_string()));
    m.insert("score".into(), rhai::Dynamic::from(chunk.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0)));
    let source = chunk.get("item").and_then(|item| item.get("key")).and_then(|v| v.as_str()).unwrap_or("");
    m.insert("source".into(), rhai::Dynamic::from(source.to_string()));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_chunks_from_result_data_shape() {
        let json = serde_json::json!({
            "success": true,
            "result": { "data": [{"text": "hello", "score": 0.9, "item": {"key": "doc1"}}] },
        });
        let chunks = extract_chunks(&json);
        assert_eq!(chunks.len(), 1);
        let m = parse_chunk(chunks[0]);
        assert_eq!(m.get("text").unwrap().to_string(), "hello");
        assert_eq!(m.get("source").unwrap().to_string(), "doc1");
    }

    #[test]
    fn extracts_chunks_from_bare_data_shape() {
        let json = serde_json::json!({ "data": [{"content": "world", "score": 0.5}] });
        let chunks = extract_chunks(&json);
        assert_eq!(chunks.len(), 1);
        let m = parse_chunk(chunks[0]);
        assert_eq!(m.get("text").unwrap().to_string(), "world");
    }

    #[test]
    fn extracts_nothing_from_unrecognized_shape() {
        let json = serde_json::json!({ "unexpected": [] });
        assert!(extract_chunks(&json).is_empty());
    }

    #[test]
    fn credentials_roundtrip_through_json() {
        let creds = CloudflareCredentials {
            account_id: "acct123".to_string(),
            api_token: "token456".to_string(),
            instance_name: "my-instance".to_string(),
        };
        let json = serde_json::to_string(&creds).unwrap();
        let back: CloudflareCredentials = serde_json::from_str(&json).unwrap();
        assert_eq!(back.account_id, "acct123");
        assert_eq!(back.instance_name, "my-instance");
    }
}
