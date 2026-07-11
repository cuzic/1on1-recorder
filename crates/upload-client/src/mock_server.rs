//! A mock implementation of design.md §13.1's recommended API contract, for this
//! crate's own tests and other crates' integration tests (e.g. `app-service`'s
//! pseudo-source E2E pipeline). Not part of the published API surface — see the
//! `mock-server` feature gate in `Cargo.toml`.
//!
//! Ported from spike-08-chunked-upload's mock server, largely unchanged: it injects
//! random pre-process faults (never wrote), post-process faults (wrote, but the
//! response was lost), and simulated timeouts, and honors `Idempotency-Key` so a
//! resent request that already succeeded is answered from cache rather than
//! double-counted.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct FaultConfig {
    /// Probability of returning an error without ever writing anything (simulates the
    /// request never reaching the server).
    pub pre_process_fault_probability: f64,
    /// Probability of writing successfully but not returning a success response
    /// (simulates a response lost in transit — design.md §13.3's "API received it,
    /// but the client doesn't know that" scenario).
    pub post_process_fault_probability: f64,
    /// Probability of sleeping past the client's timeout before responding.
    pub timeout_simulation_probability: f64,
    pub timeout_sleep: Duration,
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            pre_process_fault_probability: 0.10,
            post_process_fault_probability: 0.10,
            timeout_simulation_probability: 0.10,
            timeout_sleep: Duration::from_millis(800),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedResponse {
    status: u16,
    body: serde_json::Value,
}

#[derive(Debug, Default)]
struct SessionRecord {
    manifest: serde_json::Value,
    segments: HashMap<(String, u64), SegmentRecord>,
    finalized: bool,
}

#[derive(Debug, Clone)]
struct SegmentRecord {
    sha256: String,
    size: usize,
}

/// How many times a segment's write logic actually ran (i.e. got past the
/// idempotency cache). If this never exceeds 1 per key, retries never caused a
/// server-side duplicate — the thing design.md §13.3's dedup guarantee is for.
#[derive(Debug, Default)]
pub struct ServerStats {
    pub segment_write_counts: HashMap<(String, String, u64), u32>,
    pub requests_received: u64,
    pub faults_injected: u64,
}

pub struct AppState {
    sessions: Mutex<HashMap<String, SessionRecord>>,
    idempotency_cache: Mutex<HashMap<String, CachedResponse>>,
    pub stats: Mutex<ServerStats>,
    fault_config: FaultConfig,
}

impl AppState {
    pub fn new(fault_config: FaultConfig) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            idempotency_cache: Mutex::new(HashMap::new()),
            stats: Mutex::new(ServerStats::default()),
            fault_config,
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/recording-sessions", post(create_session))
        .route("/v1/recording-sessions/:id", get(get_session))
        .route("/v1/recording-sessions/:id/tracks/:track/segments/:sequence", put(upload_segment))
        .route("/v1/recording-sessions/:id/finalize", post(finalize_session))
        .with_state(state)
}

/// Starts the mock server on a random free localhost port; the returned base URL is
/// what an `HttpUploadClient` should point at. The server runs as a background task
/// for the test process's lifetime.
pub async fn spawn_test_server(fault_config: FaultConfig) -> (String, Arc<AppState>) {
    let state = AppState::new(fault_config);
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("failed to bind test server");
    let addr: SocketAddr = listener.local_addr().expect("failed to get local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("test server crashed");
    });
    (format!("http://{addr}"), state)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn create_session(State(state): State<Arc<AppState>>, Json(manifest): Json<serde_json::Value>) -> impl IntoResponse {
    state.stats.lock().unwrap().requests_received += 1;
    let session_id = manifest.get("session_id").and_then(|v| v.as_str()).unwrap_or("unknown-session").to_string();
    let mut sessions = state.sessions.lock().unwrap();
    sessions.entry(session_id.clone()).or_insert(SessionRecord { manifest, segments: HashMap::new(), finalized: false });
    (StatusCode::CREATED, Json(serde_json::json!({ "session_id": session_id })))
}

async fn get_session(State(state): State<Arc<AppState>>, Path(session_id): Path<String>) -> impl IntoResponse {
    let sessions = state.sessions.lock().unwrap();
    match sessions.get(&session_id) {
        Some(s) => {
            let total_bytes: usize = s.segments.values().map(|seg| seg.size).sum();
            let mut segments: Vec<_> = s
                .segments
                .iter()
                .map(|((track, sequence), seg)| serde_json::json!({ "track": track, "sequence": sequence, "sha256": seg.sha256, "size": seg.size }))
                .collect();
            segments.sort_by_key(|v| (v["track"].as_str().unwrap_or_default().to_string(), v["sequence"].as_u64().unwrap_or(0)));
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "session_id": session_id,
                    "manifest": s.manifest,
                    "segment_count": s.segments.len(),
                    "total_bytes": total_bytes,
                    "segments": segments,
                    "finalized": s.finalized,
                })),
            )
        }
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "session not found" }))),
    }
}

enum FaultDecision {
    None,
    PreProcess,
    PostProcessHidden,
    SimulateTimeout,
}

fn roll_fault(fault_config: &FaultConfig) -> FaultDecision {
    if rand::random::<f64>() < fault_config.pre_process_fault_probability {
        return FaultDecision::PreProcess;
    }
    if rand::random::<f64>() < fault_config.post_process_fault_probability {
        return FaultDecision::PostProcessHidden;
    }
    if rand::random::<f64>() < fault_config.timeout_simulation_probability {
        return FaultDecision::SimulateTimeout;
    }
    FaultDecision::None
}

async fn upload_segment(
    State(state): State<Arc<AppState>>,
    Path((session_id, track, sequence)): Path<(String, String, u64)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    state.stats.lock().unwrap().requests_received += 1;

    let idempotency_key = match header_str(&headers, "idempotency-key") {
        Some(k) => k.to_string(),
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "missing Idempotency-Key" }))),
    };

    // design.md §13.3: "if the API already received it, treat it as a success" — a
    // resend with the same Idempotency-Key is answered from cache, never re-run.
    {
        let cache = state.idempotency_cache.lock().unwrap();
        if let Some(cached) = cache.get(&idempotency_key) {
            return (StatusCode::from_u16(cached.status).unwrap(), Json(cached.body.clone()));
        }
    }

    if let FaultDecision::PreProcess = roll_fault(&state.fault_config) {
        state.stats.lock().unwrap().faults_injected += 1;
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "simulated pre-process fault" })));
    }

    let expected_sha256 = header_str(&headers, "content-sha256").unwrap_or("").to_string();
    let computed_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(&body);
        hex::encode(hasher.finalize())
    };
    if !expected_sha256.is_empty() && expected_sha256 != computed_sha256 {
        // Permanent (4xx) error: retrying won't fix a hash mismatch.
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "sha256 mismatch" })));
    }

    // The actual write — only reached once per Idempotency-Key thanks to the cache
    // check above.
    {
        let mut sessions = state.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&session_id) else {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "session not found" })));
        };
        session
            .segments
            .insert((track.clone(), sequence), SegmentRecord { sha256: computed_sha256.clone(), size: body.len() });
    }
    {
        let mut stats = state.stats.lock().unwrap();
        *stats.segment_write_counts.entry((session_id.clone(), track.clone(), sequence)).or_insert(0) += 1;
    }

    let response_body = serde_json::json!({ "status": "accepted", "session_id": session_id, "track": track, "sequence": sequence });
    state.idempotency_cache.lock().unwrap().insert(idempotency_key, CachedResponse { status: 200, body: response_body.clone() });

    match roll_fault(&state.fault_config) {
        FaultDecision::PostProcessHidden => {
            state.stats.lock().unwrap().faults_injected += 1;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "simulated response loss (write already committed)" })),
            )
        }
        FaultDecision::SimulateTimeout => {
            state.stats.lock().unwrap().faults_injected += 1;
            tokio::time::sleep(state.fault_config.timeout_sleep).await;
            (StatusCode::OK, Json(response_body))
        }
        _ => (StatusCode::OK, Json(response_body)),
    }
}

async fn finalize_session(State(state): State<Arc<AppState>>, Path(session_id): Path<String>, Json(summary): Json<serde_json::Value>) -> impl IntoResponse {
    state.stats.lock().unwrap().requests_received += 1;
    let expected_count = summary
        .get("segment_counts_by_track")
        .and_then(|v| v.as_object())
        .map(|m| m.values().filter_map(|v| v.as_u64()).sum::<u64>());

    let mut sessions = state.sessions.lock().unwrap();
    let Some(session) = sessions.get_mut(&session_id) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "session not found" })));
    };
    if let Some(expected) = expected_count {
        if (session.segments.len() as u64) < expected {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "not all segments received", "received": session.segments.len(), "expected": expected })),
            );
        }
    }
    session.finalized = true;
    (StatusCode::OK, Json(serde_json::json!({ "session_id": session_id, "finalized": true })))
}
