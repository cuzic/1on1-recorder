//! HTTP implementation of `recorder_domain::UploadAdapter`, ported from
//! spike-08-chunked-upload. Unlike the spike, this crate has no database of its own —
//! tracking which segments still need uploading is `session-store`'s job
//! (`SessionStore::pending_uploads`/`update_upload_state`); this crate only knows how
//! to send one segment/manifest/summary over HTTP and classify the result.

mod client;
mod token_provider;

#[cfg(feature = "mock-server")]
pub mod mock_server;

pub use client::HttpUploadClient;
pub use token_provider::{StaticTokenProvider, TokenProvider};
