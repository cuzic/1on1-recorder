# upload-client

HTTP implementation of `recorder_domain::UploadAdapter` (design.md §13), ported from
`spikes/spike-08-chunked-upload`.

Application-internal, not a publishing candidate: the retry/idempotency mechanics are
generic, but the crate is wired directly to `recorder-domain`'s manifest/segment/error
types.

## What changed from the spike

- **No hardcoded token.** `HttpUploadClient::new` takes a `token_provider: Arc<dyn
  TokenProvider>`. `credential-store` (task #5) will provide a real implementation;
  `StaticTokenProvider` here is for tests/local runs only.
- **No `UploadAdapter`-specific error type.** Errors are classified directly into
  `recorder_domain::UploadError`, whose variants already encode design.md §13.3's
  retry rules (`is_retryable`, `needs_token_refresh_before_retry`) — see
  `client::classify_status`.
- **No `SpoolDb`.** Tracking which segments still need uploading, and marking them
  `Completed`, is `session-store`'s job (`SessionStore::pending_uploads` /
  `update_upload_state`). This crate only knows how to send one
  manifest/segment/summary over HTTP and turn the response into a typed result — it
  holds no state of its own beyond the `reqwest::Client`.
- **The mock server is a Cargo feature (`mock-server`), not a default part of the
  crate.** It exists for this crate's own tests and other crates' dev-dependencies
  (e.g. `app-service`'s pseudo-source E2E pipeline), never for production use.

## Retry behavior (design.md §13.3)

- Timeout, 5xx, and 429 are retried with exponential backoff + jitter, up to
  `max_attempts` (default 8, configurable via `with_max_attempts`).
- 401 triggers exactly one `TokenProvider::refresh()` call, then one retry with the
  refreshed token — never a second refresh for the same request.
- Any other 4xx is permanent; the caller gets `UploadError::PermanentClientError` and
  should not retry.
- `Idempotency-Key` is `{session_id}:{track}:{sequence}`, deterministic across
  retries and process restarts, matching the server-side dedup this whole scheme
  depends on. See `tests/fault_injection.rs`'s
  `resumes_after_simulated_crash_without_duplicate_registration` for the restart
  scenario end to end.
