//! spike-plan.md SPIKE-08: チャンクアップロードと Idempotency-Key。
//! design.md §13(アップロードAPI境界)をモックサーバ+実HTTPクライアント+
//! 実SQLiteで検証する。

pub mod client;
pub mod error;
pub mod rng;
pub mod server;
pub mod spool_db;

pub use client::UploadClient;
pub use error::UploadError;
pub use server::{AppState, FaultConfig};
pub use spool_db::SpoolDb;

use std::net::SocketAddr;
use std::sync::Arc;

/// テスト用にモックサーバを127.0.0.1のランダムな空きポートで起動する。
/// 戻り値のbase_urlへ`UploadClient`を向ければよい。サーバはバックグラウンド
/// タスクとして動き続ける(テストプロセス終了まで生存)。
pub async fn spawn_test_server(fault_config: FaultConfig, seed: u64) -> (String, Arc<AppState>) {
    let state = AppState::new(fault_config, seed);
    let app = server::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind test server");
    let addr: SocketAddr = listener.local_addr().expect("failed to get local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("test server crashed");
    });
    (format!("http://{addr}"), state)
}
