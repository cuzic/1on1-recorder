//! OS keyring credential storage (design.md §12.4), ported from
//! spike-10-credential-store. Application-internal, not a publishing candidate —
//! it's a thin, app-specific adapter over the `keyring` crate.

mod encrypted_file;
mod error;
mod os_keyring;
mod token_provider_adapter;

pub use encrypted_file::EncryptedFileStore;
pub use error::StoreError;
pub use os_keyring::OsKeyringStore;
pub use token_provider_adapter::CredentialStoreTokenProvider;

pub trait CredentialStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError>;
    fn load(&self, service: &str, account: &str) -> Result<String, StoreError>;
    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError>;
}

/// design.md §12.4: tries the OS keyring first, and falls back to
/// `EncryptedFileStore` only when the backend itself is unavailable (e.g. a headless
/// Linux with no Secret Service provider registered).
///
/// **Why the fallback condition is narrow**: if this fell back on *any* keyring error
/// — including "the backend is running but denied access" or "temporarily locked" —
/// a user could silently drop to the weaker encrypted-file protection without
/// intending to. Only errors that indicate the backend itself doesn't exist trigger
/// the fallback.
pub struct FallbackCredentialStore {
    primary: OsKeyringStore,
    fallback: EncryptedFileStore,
}

/// Windows Credential Manager's hard per-entry cap (`CRED_MAX_CREDENTIAL_BLOB_SIZE`
/// in `windows-sys`) — 2,560 bytes. Crucially, the `keyring` crate's Windows backend
/// (`keyring::windows::WinCredential::set_password`, keyring 2.3.3) measures this
/// limit **after** converting the password to UTF-16LE, because that's the native
/// encoding Windows stores credential blobs in:
/// `password.encode_utf16().count() * 2 > CRED_MAX_CREDENTIAL_BLOB_SIZE`. For
/// ASCII/JSON secrets (one UTF-16 code unit per byte) this halves the effective
/// usable size to ~1,280 bytes — smaller than a typical Google Cloud service-account
/// key JSON (~2.3 KB; see `summarize::VertexCredentials`, task #57), which will
/// therefore *always* fail to save through the OS keyring on Windows, not just in
/// some edge case.
///
/// Worse, that failure doesn't trigger [`FallbackCredentialStore::should_fallback`]:
/// the `keyring` crate raises `Error::TooLong` (message: `"Attribute 'password' is
/// longer than platform limit of 2560 chars"`), which — correctly, per that method's
/// own doc comment — isn't one of the "backend doesn't exist" errors that method
/// looks for. Without the preemptive check below, a user would get a `TooLong` error
/// surfaced to the UI (not a panic, not a silent failure) but **no way to actually
/// save** Vertex AI credentials on Windows via this store.
///
/// Applied unconditionally (not `#[cfg(windows)]`): macOS Keychain and Linux Secret
/// Service have no comparably small limit, but checking on every platform keeps the
/// save→load→delete behavior identical everywhere, keeps this path exercised by
/// tests on non-Windows CI, and means a value that's safe today stays safe if a
/// future platform target turns out to have its own tight limit.
const WINDOWS_CRED_MAX_BLOB_BYTES: usize = 2560;

/// See [`WINDOWS_CRED_MAX_BLOB_BYTES`] for why this is measured in UTF-16 code units,
/// not UTF-8 bytes or `char`s.
fn exceeds_os_keyring_blob_limit(secret: &str) -> bool {
    secret.encode_utf16().count() * 2 > WINDOWS_CRED_MAX_BLOB_BYTES
}

/// Decides what [`FallbackCredentialStore::load`] should return once the primary
/// has already reported `NotFound` and the fallback has been consulted. If the
/// fallback also reports `NotFound`, `primary_not_found` is the right answer; but if
/// the fallback fails for a *different* reason (e.g. a corrupted or unreadable
/// encrypted file — `Crypto`/`Io`), that real error must be surfaced, not silently
/// rewritten to `NotFound` — otherwise a genuinely broken stored credential would be
/// misreported as "never configured" instead of "present but unreadable".
fn resolve_after_primary_not_found(primary_not_found: StoreError, fallback_result: Result<String, StoreError>) -> Result<String, StoreError> {
    fallback_result.map_err(|fallback_err| if matches!(fallback_err, StoreError::NotFound { .. }) { primary_not_found } else { fallback_err })
}

impl FallbackCredentialStore {
    pub fn new(fallback_dir: impl Into<std::path::PathBuf>) -> Result<Self, StoreError> {
        Ok(Self { primary: OsKeyringStore, fallback: EncryptedFileStore::new(fallback_dir)? })
    }

    fn should_fallback(err: &StoreError) -> bool {
        match err {
            StoreError::Backend(msg) => {
                // The `keyring` crate's Linux (secret-service) backend reports a
                // missing Secret Service provider as a D-Bus connection error string
                // (observed in this environment: messages containing "org.freedesktop.
                // secrets", "NoSuchMethod"/"ServiceUnknown"). This implementation-
                // dependent string match is a stopgap — replace it with a proper error
                // classification once `keyring` (or a wrapper) exposes one.
                let lower = msg.to_lowercase();
                lower.contains("no such method")
                    || lower.contains("serviceunknown")
                    || lower.contains("was not provided by any")
                    || lower.contains("could not connect")
                    || lower.contains("platform secure storage failure")
            }
            _ => false,
        }
    }
}

impl CredentialStore for FallbackCredentialStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError> {
        // Preemptive size check (see `WINDOWS_CRED_MAX_BLOB_BYTES`): route oversized
        // secrets straight to the encrypted file store instead of letting the OS
        // keyring reject them with an error `should_fallback` wouldn't recognize.
        if exceeds_os_keyring_blob_limit(secret) {
            tracing::warn!(
                service,
                account,
                utf16_bytes = secret.encode_utf16().count() * 2,
                "secret exceeds the OS keyring's blob size limit; saving to the encrypted file store instead"
            );
            return self.fallback.save(service, account, secret);
        }

        match self.primary.save(service, account, secret) {
            Ok(()) => Ok(()),
            Err(e) if Self::should_fallback(&e) => {
                tracing::warn!(error = %e, "OS keyring unavailable; falling back to encrypted file store");
                self.fallback.save(service, account, secret)
            }
            Err(e) => Err(e),
        }
    }

    fn load(&self, service: &str, account: &str) -> Result<String, StoreError> {
        match self.primary.load(service, account) {
            Ok(secret) => Ok(secret),
            // `NotFound` from the primary doesn't necessarily mean the credential
            // doesn't exist at all — `save` may have routed it straight to the
            // fallback store (oversized secret, or the OS keyring was unavailable at
            // save time but is reachable now). Check the fallback before giving up.
            Err(e @ StoreError::NotFound { .. }) => resolve_after_primary_not_found(e, self.fallback.load(service, account)),
            Err(e) if Self::should_fallback(&e) => self.fallback.load(service, account),
            Err(e) => Err(e),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError> {
        // Always attempt both stores: `save` may have written the secret to either
        // one (see above), and both `delete` implementations already treat "nothing
        // to delete" as `Ok(())`, so this can't turn a real absence into an error —
        // it only prevents a delete that reports success while leaving a copy behind
        // in whichever store didn't get queried.
        let primary_result = self.primary.delete(service, account);
        let fallback_result = self.fallback.delete(service, account);
        match primary_result {
            Ok(()) => fallback_result,
            Err(e) if Self::should_fallback(&e) => fallback_result,
            Err(e) => {
                // Still report the primary's hard error, but don't let a fallback
                // failure pass silently either — surface whichever result is worse.
                fallback_result?;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic size estimate for `summarize::VertexCredentials` (task #57) with an
    /// inline service-account JSON: a typical GCP service-account key (RSA-2048
    /// private key PEM + ~9 metadata fields) is itself already ~2.3 KB as compact
    /// UTF-8 JSON, and `VertexCredentials` embeds that whole JSON *as an escaped
    /// string* inside its own `{"project_id":...,"location":...,"service_account":
    /// {"json":"..."}}` wrapper — every `"` and the ~27 PEM newlines (`\n` becomes
    /// `\\n`) in the inner JSON get re-escaped, which only grows it further. This
    /// synthesizes a same-shape secret without depending on a real private key.
    fn synthetic_vertex_credentials_json() -> String {
        let pem_body: String = (0..27).map(|_| "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n").collect();
        let service_account_json = format!(
            "{{\"type\":\"service_account\",\"project_id\":\"example-project\",\"private_key_id\":\"0123456789abcdef0123456789abcdef01234567\",\"private_key\":\"-----BEGIN PRIVATE KEY-----\\n{pem_body}-----END PRIVATE KEY-----\\n\",\"client_email\":\"svc-account@example-project.iam.gserviceaccount.com\",\"client_id\":\"123456789012345678901\",\"auth_uri\":\"https://accounts.google.com/o/oauth2/auth\",\"token_uri\":\"https://oauth2.googleapis.com/token\",\"auth_provider_x509_cert_url\":\"https://www.googleapis.com/oauth2/v1/certs\",\"client_x509_cert_url\":\"https://www.googleapis.com/robot/v1/metadata/x509/svc-account%40example-project.iam.gserviceaccount.com\",\"universe_domain\":\"googleapis.com\"}}"
        );
        // Mirrors `serde_json::to_string(&VertexCredentials{ project_id, location,
        // service_account: Json(service_account_json) })` without depending on the
        // `summarize` crate (which would be a circular dev-dependency).
        let escaped = service_account_json.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{{\"project_id\":\"example-project\",\"location\":\"global\",\"service_account\":{{\"json\":\"{escaped}\"}}}}")
    }

    #[test]
    fn synthetic_vertex_credentials_json_documents_the_real_world_size_problem() {
        let secret = synthetic_vertex_credentials_json();
        // The raw UTF-8 size alone is already in the "~2.3 KB, close to 2,560 bytes"
        // range the task description worried about...
        assert!(secret.len() > 2_300, "expected a realistic ~2.3 KB+ secret, got {} bytes", secret.len());
        // ...but on Windows the actual comparison is against the UTF-16LE-encoded
        // size, which is roughly double the UTF-8 size for ASCII content (one UTF-16
        // code unit, i.e. 2 bytes, per ASCII byte) — so this isn't a near-the-limit
        // edge case, it overshoots the 2,560-byte limit by roughly 2x.
        let utf16_bytes = secret.encode_utf16().count() * 2;
        let expected_roughly_doubled = secret.len() * 2;
        assert!(
            utf16_bytes.abs_diff(expected_roughly_doubled) < 50,
            "expected UTF-16 size (~{expected_roughly_doubled}) to roughly double the UTF-8 size ({}), got {utf16_bytes} bytes",
            secret.len()
        );
        assert!(utf16_bytes > WINDOWS_CRED_MAX_BLOB_BYTES, "UTF-16 size should exceed the 2,560-byte platform limit");
        assert!(exceeds_os_keyring_blob_limit(&secret));
    }

    #[test]
    fn exceeds_os_keyring_blob_limit_is_false_for_small_ascii_secrets() {
        // A bare API key or OAuth token — the common case — must not be redirected
        // away from the OS keyring.
        assert!(!exceeds_os_keyring_blob_limit("sk-abc123-a-normal-sized-api-key"));
        assert!(!exceeds_os_keyring_blob_limit(""));
    }

    #[test]
    fn exceeds_os_keyring_blob_limit_boundary_is_exact() {
        // 1,280 ASCII chars -> 2,560 UTF-16 bytes: exactly at the limit, not over it.
        let at_limit = "a".repeat(WINDOWS_CRED_MAX_BLOB_BYTES / 2);
        assert!(!exceeds_os_keyring_blob_limit(&at_limit));
        // One character more pushes it over.
        let over_limit = "a".repeat(WINDOWS_CRED_MAX_BLOB_BYTES / 2 + 1);
        assert!(exceeds_os_keyring_blob_limit(&over_limit));
    }

    #[test]
    fn exceeds_os_keyring_blob_limit_accounts_for_utf16_doubling_of_non_ascii() {
        // Non-ASCII (e.g. Japanese) text is already >1 UTF-8 byte per `char`, but the
        // UTF-16 code unit count is what matters here, not UTF-8 byte length: 900
        // BMP characters need only 900 UTF-16 code units (1,800 bytes), well under
        // the limit, even though they'd take up to 2,700 UTF-8 bytes.
        let text: String = "あ".repeat(900);
        assert!(text.len() > WINDOWS_CRED_MAX_BLOB_BYTES, "UTF-8 byte length should exceed the limit for this test to be meaningful");
        assert!(!exceeds_os_keyring_blob_limit(&text));
    }

    #[test]
    fn fallback_store_round_trips_a_realistically_sized_vertex_credentials_secret() {
        let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = FallbackCredentialStore::new(tmp_dir.path()).expect("failed to create fallback store");
        let secret = synthetic_vertex_credentials_json();
        assert!(exceeds_os_keyring_blob_limit(&secret), "test secret should exceed the OS keyring limit");

        // Whether or not the OS keyring backend is available in this environment,
        // a secret this size must be saveable and loadable — either because the
        // preemptive size check routed it to the encrypted file store directly, or
        // (in this sandboxed environment) because the OS keyring is unavailable and
        // `should_fallback` caught it. The important guarantee under test is that
        // `load` finds it regardless of which path `save` took.
        store.save("vertex-test-service", "vertex-test-account", &secret).expect("save should succeed despite exceeding the OS keyring's blob limit");
        let loaded = store.load("vertex-test-service", "vertex-test-account").expect("load should find the secret regardless of which backend save() used");
        assert_eq!(loaded, secret);

        store.delete("vertex-test-service", "vertex-test-account").expect("delete should succeed");
        assert!(store.load("vertex-test-service", "vertex-test-account").is_err(), "credential should be gone from both backends after delete");
    }

    #[test]
    fn fallback_store_delete_removes_from_both_backends_even_when_primary_reports_success() {
        // Regression test for the pre-fix bug: if `save()` ever writes only to the
        // fallback store (oversized secret, or the OS keyring was down at save
        // time) while the OS keyring reports "no entry" as `Ok(())` on delete
        // (`OsKeyringStore::delete`'s NoEntry-is-Ok behavior), a `delete()` that only
        // touches the primary would return `Ok(())` while leaving the secret behind
        // in the fallback file store. `FallbackCredentialStore::delete` must always
        // touch both.
        let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let fallback = EncryptedFileStore::new(tmp_dir.path()).expect("failed to create fallback store");
        fallback.save("orphan-service", "orphan-account", "left-over-secret").expect("seed the fallback store directly");

        let store = FallbackCredentialStore { primary: OsKeyringStore, fallback: EncryptedFileStore::new(tmp_dir.path()).expect("failed to create fallback store") };
        store.delete("orphan-service", "orphan-account").expect("delete should succeed");

        let fallback = EncryptedFileStore::new(tmp_dir.path()).expect("failed to create fallback store");
        assert!(fallback.load("orphan-service", "orphan-account").is_err(), "secret must be gone from the fallback store after delete");
    }

    #[test]
    fn resolve_after_primary_not_found_surfaces_real_fallback_errors_instead_of_masking_them() {
        // Regression test for a bug introduced by this task's own first draft: a
        // corrupted/unreadable fallback entry (Crypto/Io) must not be reported to
        // the caller as `NotFound` just because the primary also said `NotFound`.
        let primary_not_found = StoreError::NotFound { service: "s".to_string(), account: "a".to_string() };
        let fallback_err = StoreError::Crypto("ciphertext corrupted".to_string());
        let result = resolve_after_primary_not_found(primary_not_found, Err(fallback_err));
        assert!(matches!(result, Err(StoreError::Crypto(_))), "expected the real Crypto error to surface, got {result:?}");
    }

    #[test]
    fn resolve_after_primary_not_found_returns_not_found_when_fallback_also_lacks_it() {
        let primary_not_found = StoreError::NotFound { service: "s".to_string(), account: "a".to_string() };
        let fallback_not_found = StoreError::NotFound { service: "s".to_string(), account: "a".to_string() };
        let result = resolve_after_primary_not_found(primary_not_found, Err(fallback_not_found));
        assert!(matches!(result, Err(StoreError::NotFound { .. })));
    }

    #[test]
    fn resolve_after_primary_not_found_returns_the_secret_when_fallback_has_it() {
        let primary_not_found = StoreError::NotFound { service: "s".to_string(), account: "a".to_string() };
        let result = resolve_after_primary_not_found(primary_not_found, Ok("secret".to_string()));
        assert_eq!(result.unwrap(), "secret");
    }
}
