//! Amazon Bedrock Knowledge Bases integration.
//!
//! Uses AWS SigV4 signing implemented inline to avoid the complex aws-sigv4 API.
//! Credentials are read from the credential store under account "bedrock-api-key".

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use credential_store::CredentialStore;

use crate::SettingsProvider;

pub async fn search(
    query: &str,
    options: &rhai::Map,
    credential_store: &Arc<credential_store::FallbackCredentialStore>,
    settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let knowledge_base_id = get_opt_string(options, "knowledge_base_id")
        .or_else(|| settings.get("bedrock_knowledge_base_id"))
        .ok_or("knowledge_base_id is required for Bedrock RAG")?;
    let region = get_opt_string(options, "region").unwrap_or_else(|| "us-east-1".to_string());
    let max_results = options.get("max_results").and_then(|v| v.as_int().ok()).unwrap_or(5) as usize;

    let creds = load_bedrock_credentials(credential_store)?;

    let body = serde_json::json!({
        "retrievalQuery": { "text": query },
        "retrievalConfiguration": {
            "vectorSearchConfiguration": { "numberOfResults": max_results }
        },
    });
    let body_str = serde_json::to_string(&body).unwrap_or_default();

    let service = "bedrock-agent-runtime";
    let host = format!("bedrock-agent-runtime.{region}.amazonaws.com");
    let url = format!("https://{host}/knowledgebases/{knowledge_base_id}/retrieve");
    let method = "POST";
    let content_type = "application/json";

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let amz_date = format_amz_date(now);
    let date_stamp = &amz_date[..8];

    let canonical_headers = format!("content-type:{content_type}\nhost:{host}\nx-amz-date:{amz_date}\n");
    let signed_headers = "content-type;host;x-amz-date";

    let payload_hash = sha256_hex(&body_str);
    let canonical_request = format!(
        "{method}\n/retrieve\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let canonical_request_hash = sha256_hex(&canonical_request);

    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{date_stamp}/{region}/{service}/aws4_request\n{canonical_request_hash}"
    );

    let signing_key = derive_signing_key(&creds.secret_access_key, date_stamp, &region, service);
    let signature = hmac_sha256_hex(&signing_key, &string_to_sign);

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}/{}/{}/aws4_request,SignedHeaders={},Signature={}",
        creds.access_key_id, date_stamp, region, service, signed_headers, signature
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", content_type)
        .header("Host", &host)
        .header("X-Amz-Date", &amz_date)
        .header("Authorization", &auth_header)
        .body(body_str)
        .send()
        .await
        .map_err(|e| format!("Bedrock RAG search failed: {e}"))?;

    let json: serde_json::Value = resp.json().await.map_err(|e| format!("failed to parse response: {e}"))?;

    let mut results = rhai::Array::new();
    if let Some(retrieval_results) = json["retrievalResults"].as_array() {
        for r in retrieval_results {
            let mut m = rhai::Map::new();
            m.insert("text".into(), rhai::Dynamic::from(
                r["content"]["text"].as_str().unwrap_or("").to_string()
            ));
            m.insert("score".into(), rhai::Dynamic::from(
                r["score"].as_f64().unwrap_or(0.0)
            ));
            if let Some(loc) = r["location"].as_object() {
                m.insert("source".into(), rhai::Dynamic::from(
                    loc.get("s3Location").and_then(|s| s["uri"].as_str()).unwrap_or("").to_string()
                ));
            }
            results.push(rhai::Dynamic::from_map(m));
        }
    }

    Ok(rhai::Dynamic::from_array(results))
}

pub async fn index(
    _documents: &rhai::Dynamic,
    _options: &rhai::Map,
    _credential_store: &Arc<credential_store::FallbackCredentialStore>,
    _settings: &dyn SettingsProvider,
) -> Result<rhai::Dynamic, String> {
    let mut m = rhai::Map::new();
    m.insert("indexed".into(), rhai::Dynamic::from(0_i64));
    m.insert("message".into(), rhai::Dynamic::from(
        "Bedrock KB auto-indexes from S3. Use S3 sync for document ingestion.".to_string()
    ));
    Ok(rhai::Dynamic::from_map(m))
}

struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
}

fn load_bedrock_credentials(
    credential_store: &Arc<credential_store::FallbackCredentialStore>,
) -> Result<AwsCredentials, String> {
    let raw = credential_store
        .load("1on1-recorder", "bedrock-api-key")
        .map_err(|_| "Bedrock credentials not configured".to_string())?;
    let json: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("invalid credentials JSON: {e}"))?;
    Ok(AwsCredentials {
        access_key_id: json["access_key_id"].as_str().unwrap_or("").to_string(),
        secret_access_key: json["secret_access_key"].as_str().unwrap_or("").to_string(),
    })
}

fn get_opt_string(map: &rhai::Map, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.clone().try_cast::<String>())
}

fn format_amz_date(ts: u64) -> String {
    use chrono::TimeZone;
    let dt = chrono::Utc.timestamp_opt(ts as i64, 0).unwrap();
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn hmac_sha256_hex(key: &[u8], msg: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(msg.as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

fn derive_signing_key(secret_key: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    
    

    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date);
    let k_region = hmac_sha256(&k_date, region);
    let k_service = hmac_sha256(&k_region, service);
    hmac_sha256(&k_service, "aws4_request")
}

fn hmac_sha256(key: &[u8], msg: &str) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(msg.as_bytes());
    mac.finalize().into_bytes().to_vec()
}