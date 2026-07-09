//! design.md §13(UploadAdapter)・§13.3(再送規則)のクライアント実装。
//! reqwest + exponential backoff + jitter。

use crate::error::UploadError;
use crate::rng::AtomicRng;
use sha2::{Digest, Sha256};
use std::time::Duration;

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub struct UploadClient {
    http: reqwest::Client,
    base_url: String,
    rng: AtomicRng,
    max_attempts: u32,
}

impl UploadClient {
    pub fn new(base_url: String, request_timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            base_url,
            rng: AtomicRng::new(0x1357_2468_1357_2468),
            max_attempts: 8,
        }
    }

    /// `manifest`はsession_idを含むJSON(design.md §13.1のPOST /v1/recording-sessions
    /// ボディ)。URLパス自体にはsession_idを含まない契約のため、引数として
    /// 別途受け取らない。
    pub async fn create_session(&self, manifest: &serde_json::Value) -> Result<(), UploadError> {
        let url = format!("{}/v1/recording-sessions", self.base_url);
        let request = self
            .http
            .post(url)
            .header("Authorization", "Bearer test-token")
            .json(manifest);
        self.send_with_retry(request).await
    }

    /// design.md §13.2: Idempotency-Keyは`{session_id}:{track}:{sequence}`
    /// から決定的に導出する(再試行・プロセス再起動をまたいでも同じ値になる
    /// ことが、サーバ側の重複排除が機能するための前提)。
    pub async fn upload_segment(
        &self,
        session_id: &str,
        track: &str,
        sequence: u64,
        data: &[u8],
    ) -> Result<(), UploadError> {
        let idempotency_key = format!("{session_id}:{track}:{sequence}");
        let sha256 = sha256_hex(data);
        let url = format!(
            "{}/v1/recording-sessions/{session_id}/tracks/{track}/segments/{sequence}",
            self.base_url
        );
        let request = self
            .http
            .put(url)
            .header("Authorization", "Bearer test-token")
            .header("Idempotency-Key", idempotency_key)
            .header("Content-SHA256", sha256)
            .header("Content-Type", "audio/ogg; codecs=opus")
            .body(data.to_vec());
        self.send_with_retry(request).await
    }

    pub async fn finalize_session(
        &self,
        session_id: &str,
        expected_segment_count: u64,
    ) -> Result<(), UploadError> {
        let url = format!("{}/v1/recording-sessions/{session_id}/finalize", self.base_url);
        let request = self
            .http
            .post(url)
            .header("Authorization", "Bearer test-token")
            .json(&serde_json::json!({ "expected_segment_count": expected_segment_count }));
        self.send_with_retry(request).await
    }

    async fn send_with_retry(&self, request: reqwest::RequestBuilder) -> Result<(), UploadError> {
        let mut used_auth_refresh = false;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let req = request
                .try_clone()
                .expect("request body must be clonable for retry (Vec<u8>/json bodies are)");
            let resp = req.send().await;
            match self.classify(resp).await {
                Ok(()) => return Ok(()),
                Err(UploadError::Unauthorized) if !used_auth_refresh => {
                    // design.md §13.3: 401はトークン更新後に1回だけ再送。
                    // このspikeでは実際のトークン更新処理は行わない(モック
                    // サーバが401を返さないため経路自体は未使用だが、
                    // 分類・リトライ回数制御は正しく実装しておく)。
                    used_auth_refresh = true;
                    continue;
                }
                Err(UploadError::Retryable(msg)) if attempt < self.max_attempts => {
                    let delay = self.compute_backoff(attempt);
                    tracing::debug!(attempt, ?delay, %msg, "retrying after transient error");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn classify(
        &self,
        resp: Result<reqwest::Response, reqwest::Error>,
    ) -> Result<(), UploadError> {
        let resp = match resp {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Err(UploadError::Retryable(format!("timeout: {e}")))
            }
            Err(e) => return Err(UploadError::Retryable(format!("connection error: {e}"))),
        };
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body_text = resp.text().await.unwrap_or_default();
        Err(UploadError::classify_status(status, body_text))
    }

    fn compute_backoff(&self, attempt: u32) -> Duration {
        // design.md §13.3: exponential backoff + jitter。spikeでは検証時間を
        // 短くするため基準値を小さくしている(本番実装では秒オーダーを想定)。
        let base_ms = 20u64;
        let max_ms = 500u64;
        let exp = base_ms.saturating_mul(1u64 << attempt.min(8));
        let capped = exp.min(max_ms);
        let jitter_frac = 0.5 + self.rng.next_f64() * 0.5;
        Duration::from_millis(((capped as f64) * jitter_frac).max(5.0) as u64)
    }
}
