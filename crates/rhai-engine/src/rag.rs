//! RAG (Retrieval-Augmented Generation) API support for multiple providers.
//!
//! Each provider is a separate module. The dispatcher routes `rag_search` and
//! `rag_index` commands to the appropriate provider based on the `provider` field
//! in the arguments.
//!
//! Supported providers:
//! - `vertex` — Google Cloud Vertex AI RAG Engine / Discovery Engine
//! - `bedrock` — Amazon Bedrock Knowledge Bases
//! - `cloudflare` — Cloudflare AI Search (formerly "AutoRAG")
//! - (more to be added: Azure Cognitive Search, Hyperspell, Contextual.ai, etc.)

mod bedrock;
mod cloudflare;
mod vertex;

use std::sync::Arc;

use crate::SettingsProvider;

// Re-exported one hop at a time (here, then again in `lib.rs`) so
// `apps/desktop/src/settings.rs` can write credentials under the exact same
// `(service, account)` pair `cloudflare::search` reads — without depending on
// `crates/rhai-engine` internals any deeper than these two constants.
pub use cloudflare::{CloudflareCredentials, CLOUDFLARE_AI_SEARCH_ACCOUNT, CREDENTIAL_SERVICE as CLOUDFLARE_CREDENTIAL_SERVICE};

/// Dispatches a RAG search to the appropriate provider.
pub async fn rag_search(
    args: &rhai::Map,
    credential_store: &Arc<credential_store::FallbackCredentialStore>,
    settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let provider = get_string(args, "provider")?;
    let query = get_string(args, "query")?;
    let options = args.get("options").and_then(|v| v.clone().try_cast::<rhai::Map>()).unwrap_or_default();

    match provider.as_str() {
        "vertex" => vertex::search(&query, &options, credential_store, settings).await,
        "bedrock" => bedrock::search(&query, &options, credential_store, settings).await,
        "cloudflare" => cloudflare::search(&query, &options, credential_store, settings).await,
        other => Err(format!("unsupported RAG provider: {other}")),
    }
}

/// Dispatches a RAG index operation to the appropriate provider.
pub async fn rag_index(
    args: &rhai::Map,
    credential_store: &Arc<credential_store::FallbackCredentialStore>,
    settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let provider = get_string(args, "provider")?;
    let documents = args.get("documents").ok_or("missing argument: documents")?;
    let options = args.get("options").and_then(|v| v.clone().try_cast::<rhai::Map>()).unwrap_or_default();

    match provider.as_str() {
        "vertex" => vertex::index(documents, &options, credential_store, settings).await,
        "bedrock" => bedrock::index(documents, &options, credential_store, settings).await,
        "cloudflare" => cloudflare::index(documents, &options, credential_store, settings).await,
        other => Err(format!("unsupported RAG provider: {other}")),
    }
}

fn get_string(args: &rhai::Map, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.clone().try_cast::<String>())
        .ok_or_else(|| format!("missing required argument: {key}"))
}