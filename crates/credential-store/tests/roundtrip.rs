//! Ported from spike-10-credential-store's `tests/roundtrip.rs`.

use credential_store::{CredentialStore, CredentialStoreTokenProvider, EncryptedFileStore, FallbackCredentialStore, OsKeyringStore};
use std::sync::Arc;
use upload_client::TokenProvider;

#[test]
fn os_keyring_observed_behavior_in_this_environment() {
    // This test doesn't assert a fixed outcome — it records how the OS keyring
    // actually behaves in whatever environment CI/dev runs in. On a headless Linux
    // without a Secret Service provider registered, failure is the expected result.
    let store = OsKeyringStore;
    let result = store.save("cred-store-test-service", "cred-store-test-account", "hunter2");
    match &result {
        Ok(()) => {
            eprintln!("OS keyring save succeeded in this environment (Secret Service is registered)");
            let loaded = store.load("cred-store-test-service", "cred-store-test-account").expect("load should succeed after save");
            assert_eq!(loaded, "hunter2");
            store.delete("cred-store-test-service", "cred-store-test-account").expect("delete should succeed");
        }
        Err(e) => {
            eprintln!("OS keyring save failed as expected in a headless Linux without Secret Service: {e}");
        }
    }
}

#[test]
fn encrypted_file_fallback_roundtrip_succeeds_regardless_of_os_keyring() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let store = EncryptedFileStore::new(tmp_dir.path()).expect("failed to create store");

    store.save("cred-store-fallback-service", "user@example.com", "s3cr3t-token").expect("save should succeed");
    let loaded = store.load("cred-store-fallback-service", "user@example.com").expect("load should succeed");
    assert_eq!(loaded, "s3cr3t-token");

    store.delete("cred-store-fallback-service", "user@example.com").expect("delete should succeed");
    let after_delete = store.load("cred-store-fallback-service", "user@example.com");
    assert!(after_delete.is_err(), "loading a deleted credential should fail");
}

#[test]
fn encrypted_file_store_persists_across_separate_instances() {
    // The master key is stored on disk and reused across a process restart — a
    // stored token must survive the app being closed and reopened.
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");

    {
        let store = EncryptedFileStore::new(tmp_dir.path()).expect("failed to create store (1st)");
        store.save("persist-service", "acct", "persisted-secret").expect("save should succeed");
    }
    {
        let store = EncryptedFileStore::new(tmp_dir.path()).expect("failed to create store (2nd)");
        let loaded = store.load("persist-service", "acct").expect("load should succeed with a fresh store instance");
        assert_eq!(loaded, "persisted-secret");
    }
}

#[test]
fn fallback_store_succeeds_end_to_end_even_when_os_keyring_is_unavailable() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let store = FallbackCredentialStore::new(tmp_dir.path()).expect("failed to create fallback store");

    store.save("app-service", "app-account", "app-secret-value").expect("save should succeed via primary or fallback");
    let loaded = store.load("app-service", "app-account").expect("load should succeed via primary or fallback");
    assert_eq!(loaded, "app-secret-value");
    store.delete("app-service", "app-account").expect("delete should succeed via primary or fallback");
}

#[tokio::test]
async fn credential_store_token_provider_reads_through_to_the_underlying_store() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let store = Arc::new(EncryptedFileStore::new(tmp_dir.path()).expect("failed to create store"));
    store.save("upload-api", "recorder", "bearer-abc123").expect("save should succeed");

    let provider = CredentialStoreTokenProvider::new(store, "upload-api", "recorder");
    let token = provider.access_token().await.expect("access_token should read through to the store");
    assert_eq!(token, "bearer-abc123");

    // A static-token provider's refresh is a no-op that never errors.
    provider.refresh().await.expect("refresh should be a no-op success");
}
