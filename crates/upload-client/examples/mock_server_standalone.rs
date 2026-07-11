//! A standalone, long-running instance of this crate's mock server (design.md
//! §13.1's recommended API contract), for manual/local testing — e.g. task #9's
//! Phase 1A acceptance test, which needs *some* reachable endpoint to point
//! `RECORDER_API_BASE_URL` at, and no real production API exists yet.
//!
//! Not for production use — see `mock_server`'s own doc comment. Faults are
//! disabled by default here (unlike this crate's own fault-injection tests) since
//! the point of this binary is a stable target for a real 30-minute recording,
//! not to re-exercise retry logic that's already covered by
//! `tests/fault_injection.rs`.
//!
//! Usage: `cargo run -p upload-client --example mock_server_standalone --features mock-server`
//! (binds to `127.0.0.1:8787` by default; override with `MOCK_SERVER_PORT`).

#[tokio::main]
async fn main() {
    tracing_subscriber_fallback_init();

    let port: u16 = std::env::var("MOCK_SERVER_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(8787);
    let fault_config = upload_client::mock_server::FaultConfig {
        pre_process_fault_probability: 0.0,
        post_process_fault_probability: 0.0,
        timeout_simulation_probability: 0.0,
        timeout_sleep: std::time::Duration::from_millis(0),
    };
    let state = upload_client::mock_server::AppState::new(fault_config);
    let app = upload_client::mock_server::router(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("failed to bind — is the port already in use?");
    println!("mock upload API listening on http://127.0.0.1:{port} (Ctrl+C to stop)");
    axum::serve(listener, app).await.expect("server error");
}

/// A bare-bones `tracing` subscriber so `tracing::debug!` calls in `upload-client`
/// (e.g. retry logging) are visible when running this manually; not something
/// other examples/tests in this crate need, since they don't print to a terminal.
fn tracing_subscriber_fallback_init() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let _ = tracing_subscriber::registry().with(tracing_subscriber::fmt::layer()).with(tracing_subscriber::EnvFilter::from_default_env()).try_init();
}
