//! design.md §13.3 再送規則の分類。

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("transient error (timeout/5xx/429), retryable: {0}")]
    Retryable(String),
    #[error("permanent error (4xx other than 401/429): {0}")]
    Permanent(String),
    #[error("unauthorized (401), needs token refresh then single retry")]
    Unauthorized,
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl UploadError {
    pub fn classify_status(status: reqwest::StatusCode, body: String) -> Self {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            UploadError::Unauthorized
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            UploadError::Retryable(format!("{status}: {body}"))
        } else {
            UploadError::Permanent(format!("{status}: {body}"))
        }
    }
}
