use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use recorder_domain::{AudioSegment, RemoteSession, SessionManifest, SessionSummary, UploadAdapter, UploadError, UploadReceipt};
use reqwest::{RequestBuilder, Response, StatusCode};

use crate::token_provider::TokenProvider;

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// HTTP implementation of `UploadAdapter` (design.md §13.1's recommended contract).
/// Response body contract: `create_session` expects `{"session_id": "<remote id>"}`.
pub struct HttpUploadClient {
    http: reqwest::Client,
    base_url: String,
    token_provider: Arc<dyn TokenProvider>,
    max_attempts: u32,
}

impl HttpUploadClient {
    pub fn new(base_url: String, request_timeout: Duration, token_provider: Arc<dyn TokenProvider>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            .expect("failed to build reqwest client");
        Self { http, base_url, token_provider, max_attempts: 8 }
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// `build_request` is called fresh on every attempt (not just cloned) so a
    /// refreshed token from a 401 recovery reaches the retried request.
    async fn send_with_retry<F>(&self, mut build_request: F) -> Result<Response, UploadError>
    where
        F: FnMut(&str) -> RequestBuilder,
    {
        let mut token = self.token_provider.access_token().await?;
        let mut used_auth_refresh = false;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let response = build_request(&token).send().await;
            match Self::classify(response).await {
                Ok(response) => return Ok(response),
                Err(UploadError::AuthExpired) if !used_auth_refresh => {
                    used_auth_refresh = true;
                    self.token_provider.refresh().await?;
                    token = self.token_provider.access_token().await?;
                }
                Err(e) if e.is_retryable() && attempt < self.max_attempts => {
                    let delay = Self::compute_backoff(attempt);
                    tracing::debug!(attempt, ?delay, %e, "retrying after transient upload error");
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn classify(response: Result<Response, reqwest::Error>) -> Result<Response, UploadError> {
        let response = match response {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return Err(UploadError::Timeout),
            Err(e) => return Err(UploadError::Transport(e.to_string())),
        };
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        Err(classify_status(status, body))
    }

    fn compute_backoff(attempt: u32) -> Duration {
        // design.md §13.3: exponential backoff + jitter.
        let base_ms = 200u64;
        let max_ms = 30_000u64;
        let exp = base_ms.saturating_mul(1u64 << attempt.min(8));
        let capped = exp.min(max_ms);
        let jitter_frac = 0.5 + rand::random::<f64>() * 0.5;
        Duration::from_millis(((capped as f64) * jitter_frac).max(5.0) as u64)
    }
}

fn classify_status(status: StatusCode, body: String) -> UploadError {
    if status == StatusCode::UNAUTHORIZED {
        UploadError::AuthExpired
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        UploadError::RateLimited
    } else if status.is_server_error() {
        UploadError::ServerError { status: status.as_u16() }
    } else {
        UploadError::PermanentClientError { status: status.as_u16(), reason: body }
    }
}

#[async_trait]
impl UploadAdapter for HttpUploadClient {
    async fn create_session(&self, manifest: &SessionManifest) -> Result<RemoteSession, UploadError> {
        let body = serde_json::to_value(manifest).map_err(|e| UploadError::Transport(e.to_string()))?;
        let url = format!("{}/v1/recording-sessions", self.base_url);
        let response = self
            .send_with_retry(|token| self.http.post(&url).bearer_auth(token).json(&body))
            .await?;

        let parsed: serde_json::Value = response.json().await.map_err(|e| UploadError::Transport(e.to_string()))?;
        let remote_session_id = parsed
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| UploadError::Transport("create_session response missing session_id".to_string()))?
            .to_string();

        Ok(RemoteSession { session_id: manifest.session_id, remote_session_id })
    }

    async fn upload_segment(&self, remote: &RemoteSession, segment: &AudioSegment) -> Result<UploadReceipt, UploadError> {
        let data = tokio::fs::read(&segment.local_path).await.map_err(|e| UploadError::Transport(e.to_string()))?;
        let sha256 = sha256_hex(&data);
        // design.md §13.2: derived deterministically from (session_id, track,
        // sequence) so retries and process restarts always produce the same key —
        // the server's dedup only works if this never changes across resends.
        let idempotency_key = format!("{}:{}:{}", remote.session_id, segment.track, segment.sequence);
        let url = format!(
            "{}/v1/recording-sessions/{}/tracks/{}/segments/{}",
            self.base_url,
            remote.remote_session_id,
            segment.track.as_manifest_str(),
            segment.sequence
        );

        self.send_with_retry(|token| {
            self.http
                .put(&url)
                .bearer_auth(token)
                .header("Idempotency-Key", idempotency_key.clone())
                .header("Content-SHA256", sha256.clone())
                .header("Content-Type", "audio/ogg; codecs=opus")
                .body(data.clone())
        })
        .await?;

        Ok(UploadReceipt { track: segment.track, sequence: segment.sequence, accepted_at: chrono::Utc::now() })
    }

    async fn finalize_session(&self, remote: &RemoteSession, summary: &SessionSummary) -> Result<(), UploadError> {
        let body = serde_json::to_value(summary).map_err(|e| UploadError::Transport(e.to_string()))?;
        let url = format!("{}/v1/recording-sessions/{}/finalize", self.base_url, remote.remote_session_id);

        self.send_with_retry(|token| self.http.post(&url).bearer_auth(token).json(&body)).await?;
        Ok(())
    }
}
