//! design.md §13.1 推奨API契約のモックサーバ(axum)。
//! spike-plan.md SPIKE-08検証手順1: ランダムに5xx/429/timeout/
//! 「受領済みなのに応答喪失」を注入する。

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::rng::AtomicRng;

#[derive(Debug, Clone)]
pub struct FaultConfig {
    /// 書き込みを一切行わずにエラーを返す確率(「サーバに届かなかった」相当)。
    pub pre_process_fault_probability: f64,
    /// 書き込みは正常に行うが、成功応答を返さない確率
    /// (「受領済みなのに応答喪失」相当。design.md §13.3の想定シナリオ)。
    pub post_process_fault_probability: f64,
    /// クライアント側タイムアウトを誘発するための疑似スリープを注入する確率。
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

/// 実際に「書き込み処理」が実行された回数(=idempotency cacheをすり抜けて
/// 本体ロジックへ到達した回数)。クライアントが同じIdempotency-Keyで何度
/// リトライしても、ここが1を超えなければ「重複登録0件」が成立している。
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
    rng: AtomicRng,
}

impl AppState {
    pub fn new(fault_config: FaultConfig, seed: u64) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            idempotency_cache: Mutex::new(HashMap::new()),
            stats: Mutex::new(ServerStats::default()),
            fault_config,
            rng: AtomicRng::new(seed),
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/recording-sessions", post(create_session))
        .route("/v1/recording-sessions/:id", get(get_session))
        .route(
            "/v1/recording-sessions/:id/tracks/:track/segments/:sequence",
            put(upload_segment),
        )
        .route("/v1/recording-sessions/:id/finalize", post(finalize_session))
        .with_state(state)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(manifest): Json<serde_json::Value>,
) -> impl IntoResponse {
    state.stats.lock().unwrap().requests_received += 1;
    let session_id = manifest
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown-session")
        .to_string();
    let mut sessions = state.sessions.lock().unwrap();
    sessions.entry(session_id.clone()).or_insert(SessionRecord {
        manifest,
        segments: HashMap::new(),
        finalized: false,
    });
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "session_id": session_id })),
    )
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let sessions = state.sessions.lock().unwrap();
    match sessions.get(&session_id) {
        Some(s) => {
            let total_bytes: usize = s.segments.values().map(|seg| seg.size).sum();
            let mut segments: Vec<_> = s
                .segments
                .iter()
                .map(|((track, sequence), seg)| {
                    serde_json::json!({
                        "track": track,
                        "sequence": sequence,
                        "sha256": seg.sha256,
                        "size": seg.size,
                    })
                })
                .collect();
            segments.sort_by_key(|v| {
                (
                    v["track"].as_str().unwrap_or_default().to_string(),
                    v["sequence"].as_u64().unwrap_or(0),
                )
            });
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
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ),
    }
}

/// フォールト注入の判定だけを行う(実際にどう応答するかは呼び出し側で決める)。
enum FaultDecision {
    None,
    PreProcess,
    PostProcessHidden,
    SimulateTimeout,
}

fn roll_fault(state: &AppState) -> FaultDecision {
    if state
        .rng
        .next_bool_with_probability(state.fault_config.pre_process_fault_probability)
    {
        return FaultDecision::PreProcess;
    }
    if state
        .rng
        .next_bool_with_probability(state.fault_config.post_process_fault_probability)
    {
        return FaultDecision::PostProcessHidden;
    }
    if state
        .rng
        .next_bool_with_probability(state.fault_config.timeout_simulation_probability)
    {
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
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "missing Idempotency-Key" })),
            )
        }
    };

    // design.md §13.3: 「APIが受領済みの場合は成功扱い」。同じIdempotency-Key
    // での再送は、本体ロジックへ到達させず常にキャッシュ済みの結果を返す。
    {
        let cache = state.idempotency_cache.lock().unwrap();
        if let Some(cached) = cache.get(&idempotency_key) {
            return (
                StatusCode::from_u16(cached.status).unwrap(),
                Json(cached.body.clone()),
            );
        }
    }

    match roll_fault(&state) {
        FaultDecision::PreProcess => {
            state.stats.lock().unwrap().faults_injected += 1;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "simulated pre-process fault" })),
            );
        }
        _ => {}
    }

    let expected_sha256 = header_str(&headers, "content-sha256").unwrap_or("").to_string();
    let computed_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(&body);
        hex::encode(hasher.finalize())
    };
    if !expected_sha256.is_empty() && expected_sha256 != computed_sha256 {
        // 恒久エラー(400系)。リトライしても直らないため再送規則の対象外。
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "sha256 mismatch" })),
        );
    }

    // ここが「本体の書き込み」。idempotency cacheを通過した場合のみ到達する。
    {
        let mut sessions = state.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&session_id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            );
        };
        session.segments.insert(
            (track.clone(), sequence),
            SegmentRecord {
                sha256: computed_sha256.clone(),
                size: body.len(),
            },
        );
    }
    {
        let mut stats = state.stats.lock().unwrap();
        *stats
            .segment_write_counts
            .entry((session_id.clone(), track.clone(), sequence))
            .or_insert(0) += 1;
    }

    let response_body = serde_json::json!({
        "status": "accepted",
        "session_id": session_id,
        "track": track,
        "sequence": sequence,
    });
    state.idempotency_cache.lock().unwrap().insert(
        idempotency_key,
        CachedResponse {
            status: 200,
            body: response_body.clone(),
        },
    );

    match roll_fault(&state) {
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

async fn finalize_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(summary): Json<serde_json::Value>,
) -> impl IntoResponse {
    state.stats.lock().unwrap().requests_received += 1;
    let expected_count = summary
        .get("expected_segment_count")
        .and_then(|v| v.as_u64());

    let mut sessions = state.sessions.lock().unwrap();
    let Some(session) = sessions.get_mut(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        );
    };
    if let Some(expected) = expected_count {
        if (session.segments.len() as u64) < expected {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "not all segments received",
                    "received": session.segments.len(),
                    "expected": expected,
                })),
            );
        }
    }
    session.finalized = true;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "session_id": session_id, "finalized": true })),
    )
}
