//! Deepgram's pre-recorded ("batch") speech-to-text API:
//! `POST https://api.deepgram.com/v1/listen` with raw PCM16 audio as the request body
//! and session config as query parameters (the same parameter names as the streaming
//! adapter in `lib.rs`, via [`common_query_params`](super::common_query_params)),
//! returning one synchronous JSON response instead of a stream of WebSocket messages.
//! No `CloseStream`/`Metadata` handshake is needed — the HTTP response *is* the final
//! result.
//!
//! Response shape (`results.channels[0].alternatives[0].transcript`/`words[]`, with
//! each word carrying `speaker` when diarization is requested) is based on Deepgram's
//! public API reference for the pre-recorded endpoint. **An actual API key has not
//! been used to verify this against a live response** — this has only been exercised
//! against the local mock server in this module's tests, so treat the
//! `DeepgramBatch*` deserialization structs as best-effort until confirmed against a
//! real call.

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use stt_api::{BatchAudioInput, BatchSttProvider, BatchTranscript, SttError, SttSessionConfig, Word};

use crate::{common_query_params, encode_query, pcm_f32_to_linear16_le, DEFAULT_MODEL};

const BATCH_LISTEN_URL: &str = "https://api.deepgram.com/v1/listen";

/// A configured Deepgram batch provider. Unlike [`crate::DeepgramProvider`] (one
/// WebSocket session per call), this holds a reusable [`reqwest::Client`] since batch
/// requests are just independent HTTP calls with no session state to keep alive.
pub struct DeepgramBatchProvider {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl DeepgramBatchProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Test/local-mock-server seam: points requests at `base_url` instead of
    /// Deepgram's own endpoint. Not part of the public API surface — production
    /// callers always use [`DeepgramBatchProvider::new`], which talks to Deepgram
    /// directly.
    #[cfg(test)]
    fn with_base_url(api_key: impl Into<String>, base_url: String) -> DeepgramBatchProviderForTest {
        DeepgramBatchProviderForTest {
            inner: Self::new(api_key),
            base_url,
        }
    }
}

#[async_trait]
impl BatchSttProvider for DeepgramBatchProvider {
    async fn transcribe_batch(
        &self,
        audio: BatchAudioInput<'_>,
        config: SttSessionConfig,
    ) -> Result<BatchTranscript, SttError> {
        transcribe_batch_at(&self.http, BATCH_LISTEN_URL, &self.api_key, &self.model, audio, config).await
    }
}

/// Test-only wrapper that swaps in a local mock server's base URL. Kept separate from
/// [`DeepgramBatchProvider`] rather than adding a public `base_url` field/setter there,
/// so nothing outside this crate's tests can accidentally point production traffic at
/// an arbitrary URL.
#[cfg(test)]
struct DeepgramBatchProviderForTest {
    inner: DeepgramBatchProvider,
    base_url: String,
}

#[cfg(test)]
#[async_trait]
impl BatchSttProvider for DeepgramBatchProviderForTest {
    async fn transcribe_batch(
        &self,
        audio: BatchAudioInput<'_>,
        config: SttSessionConfig,
    ) -> Result<BatchTranscript, SttError> {
        transcribe_batch_at(
            &self.inner.http,
            &self.base_url,
            &self.inner.api_key,
            &self.inner.model,
            audio,
            config,
        )
        .await
    }
}

async fn transcribe_batch_at(
    http: &reqwest::Client,
    listen_url: &str,
    api_key: &str,
    model: &str,
    audio: BatchAudioInput<'_>,
    config: SttSessionConfig,
) -> Result<BatchTranscript, SttError> {
    if audio.sample_rate_hz == 0 {
        return Err(SttError::PermanentError(
            "sample_rate_hz must be nonzero".to_string(),
        ));
    }
    if audio.channels == 0 {
        return Err(SttError::PermanentError(
            "channels must be nonzero".to_string(),
        ));
    }

    let url = build_batch_url(listen_url, model, &config, audio.sample_rate_hz, audio.channels);
    let body = pcm_f32_to_linear16_le(audio.pcm);

    let response = http
        .post(&url)
        .header("Authorization", format!("Token {api_key}"))
        .header("Content-Type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .map_err(map_request_error)?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        return Err(map_status_error(status, body_text));
    }

    let parsed: DeepgramBatchResponse = response.json().await.map_err(|err| {
        SttError::Transport(format!("failed to parse deepgram batch response: {err}"))
    })?;
    translate_batch_response(parsed)
}

fn build_batch_url(
    listen_url: &str,
    model: &str,
    config: &SttSessionConfig,
    sample_rate_hz: u32,
    channels: u16,
) -> String {
    let mut params = common_query_params(model, config);
    params.push(("encoding", "linear16".to_string()));
    params.push(("sample_rate", sample_rate_hz.to_string()));
    params.push(("channels", channels.to_string()));
    format!("{listen_url}?{}", encode_query(&params))
}

fn map_request_error(err: reqwest::Error) -> SttError {
    if err.is_timeout() {
        SttError::Timeout
    } else {
        SttError::Transport(err.to_string())
    }
}

fn map_status_error(status: StatusCode, body: String) -> SttError {
    match status.as_u16() {
        401 | 403 => SttError::AuthenticationFailed(body),
        429 => SttError::RateLimited,
        500..=599 => SttError::Transport(format!("HTTP {status}: {body}")),
        _ => SttError::PermanentError(format!("HTTP {status}: {body}")),
    }
}

#[derive(Debug, Deserialize)]
struct DeepgramBatchResponse {
    results: DeepgramBatchResults,
}

#[derive(Debug, Deserialize)]
struct DeepgramBatchResults {
    #[serde(default)]
    channels: Vec<DeepgramBatchChannel>,
}

#[derive(Debug, Deserialize)]
struct DeepgramBatchChannel {
    #[serde(default)]
    alternatives: Vec<DeepgramBatchAlternative>,
}

#[derive(Debug, Deserialize)]
struct DeepgramBatchAlternative {
    transcript: String,
    #[serde(default)]
    words: Vec<DeepgramBatchWord>,
}

#[derive(Debug, Deserialize)]
struct DeepgramBatchWord {
    word: String,
    start: f64,
    end: f64,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    speaker: Option<u32>,
}

fn translate_batch_response(response: DeepgramBatchResponse) -> Result<BatchTranscript, SttError> {
    let alternative = response
        .results
        .channels
        .into_iter()
        .next()
        .and_then(|channel| channel.alternatives.into_iter().next())
        .ok_or_else(|| {
            SttError::PermanentError("deepgram batch response had no transcription alternatives".to_string())
        })?;

    let mut transcript = BatchTranscript::new(alternative.transcript);
    if !alternative.words.is_empty() {
        let words = alternative
            .words
            .into_iter()
            .map(|w| Word {
                text: w.word,
                start_ms: Some((w.start * 1000.0).round() as u64),
                end_ms: Some((w.end * 1000.0).round() as u64),
                confidence: w.confidence,
                speaker: w.speaker,
            })
            .collect();
        transcript = transcript.with_words(words);
    }
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode as AxumStatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    #[test]
    fn build_batch_url_includes_shared_and_batch_only_params() {
        let config = SttSessionConfig::new(16_000).with_diarization(true);
        let url = build_batch_url(BATCH_LISTEN_URL, DEFAULT_MODEL, &config, 16_000, 1);
        assert!(url.starts_with(BATCH_LISTEN_URL));
        assert!(url.contains("model=nova-3"));
        assert!(url.contains("language=ja"));
        assert!(url.contains("punctuate=true"));
        assert!(url.contains("diarize=true"));
        assert!(url.contains("encoding=linear16"));
        assert!(url.contains("sample_rate=16000"));
        assert!(url.contains("channels=1"));
        // Streaming-only params must never leak into a batch request.
        assert!(!url.contains("interim_results"));
        assert!(!url.contains("vad_events"));
    }

    #[test]
    fn translate_batch_response_maps_transcript_and_words() {
        let raw = r#"{
            "results": {
                "channels": [{
                    "alternatives": [{
                        "transcript": "こんにちは",
                        "words": [
                            { "word": "こんにちは", "start": 0.1, "end": 0.6, "confidence": 0.98, "speaker": 0 }
                        ]
                    }]
                }]
            }
        }"#;
        let parsed: DeepgramBatchResponse = serde_json::from_str(raw).unwrap();
        let transcript = translate_batch_response(parsed).unwrap();
        assert_eq!(transcript.text, "こんにちは");
        let words = transcript.words.unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "こんにちは");
        assert_eq!(words[0].start_ms, Some(100));
        assert_eq!(words[0].end_ms, Some(600));
        assert_eq!(words[0].speaker, Some(0));
    }

    #[test]
    fn translate_batch_response_errors_when_no_alternatives() {
        let raw = r#"{"results": {"channels": []}}"#;
        let parsed: DeepgramBatchResponse = serde_json::from_str(raw).unwrap();
        let err = translate_batch_response(parsed).unwrap_err();
        assert!(matches!(err, SttError::PermanentError(_)));
    }

    /// Minimal axum mock of Deepgram's pre-recorded endpoint: echoes back a canned
    /// response shaped like the real API, and records the last request's headers/body
    /// length so tests can assert on what was actually sent. Modeled after
    /// `upload-client`'s `mock_server.rs` (spawn a real localhost server rather than
    /// mocking at the `reqwest` layer), scoped down since batch STT needs neither
    /// fault injection nor idempotency-cache behavior.
    #[derive(Default)]
    struct MockState {
        last_authorization: Mutex<Option<String>>,
        last_body_len: Mutex<usize>,
    }

    async fn mock_listen(
        State(state): State<Arc<MockState>>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> impl IntoResponse {
        *state.last_authorization.lock().unwrap() =
            headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
        *state.last_body_len.lock().unwrap() = body.len();

        (
            AxumStatusCode::OK,
            Json(serde_json::json!({
                "metadata": { "request_id": "mock-request-id" },
                "results": {
                    "channels": [{
                        "alternatives": [{
                            "transcript": "テストです",
                            "words": [
                                { "word": "テスト", "start": 0.0, "end": 0.4, "confidence": 0.9, "speaker": 0 },
                                { "word": "です", "start": 0.4, "end": 0.8, "confidence": 0.9, "speaker": 0 }
                            ]
                        }]
                    }]
                }
            })),
        )
    }

    async fn spawn_mock_server() -> (String, Arc<MockState>) {
        let state = Arc::new(MockState::default());
        let app = Router::new().route("/v1/listen", post(mock_listen)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1/listen"), state)
    }

    #[tokio::test]
    async fn transcribe_batch_round_trips_through_mock_server() {
        let (base_url, state) = spawn_mock_server().await;
        let provider = DeepgramBatchProvider::with_base_url("mock-api-key", base_url);

        let pcm = vec![0.0f32; 1600]; // 0.1s of silence at 16kHz, mono
        let audio = BatchAudioInput { pcm: &pcm, sample_rate_hz: 16_000, channels: 1 };
        let config = SttSessionConfig::new(16_000).with_diarization(true);

        let transcript = provider.transcribe_batch(audio, config).await.unwrap();

        assert_eq!(transcript.text, "テストです");
        let words = transcript.words.unwrap();
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "テスト");
        assert_eq!(words[1].speaker, Some(0));

        assert_eq!(*state.last_authorization.lock().unwrap(), Some("Token mock-api-key".to_string()));
        // 1600 f32 samples -> 1600 PCM16 samples -> 3200 bytes on the wire.
        assert_eq!(*state.last_body_len.lock().unwrap(), 3200);
    }

    #[tokio::test]
    async fn transcribe_batch_rejects_zero_sample_rate() {
        let provider = DeepgramBatchProvider::new("mock-api-key");
        let pcm = vec![0.0f32; 10];
        let audio = BatchAudioInput { pcm: &pcm, sample_rate_hz: 0, channels: 1 };
        let err = provider
            .transcribe_batch(audio, SttSessionConfig::new(16_000))
            .await
            .unwrap_err();
        assert!(matches!(err, SttError::PermanentError(_)));
    }
}
