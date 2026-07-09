//! spike-plan.md SPIKE-10 検証手順1・2の自動化。
//!
//! この環境(D-Busセッションはあるが`org.freedesktop.secrets`を提供する
//! Secret Serviceデーモンが登録されていないLinux)は、まさに検証手順2が
//! 求める「Linuxでgnome-keyring不在時」のテストケースそのものである。

use spike_10_credential_store::{CredentialStore, EncryptedFileStore, FallbackCredentialStore, OsKeyringStore};

#[test]
fn os_keyring_observed_behavior_in_this_environment() {
    // このテストは合否を断定しない。実機(3 OS)でどう振る舞うかを記録する
    // ためのものであり、Secret Service不在のLinuxでは失敗するのが期待値。
    let store = OsKeyringStore;
    let result = store.save("spike10-test-service", "spike10-test-account", "hunter2");
    match &result {
        Ok(()) => {
            eprintln!("OS keyring save succeeded in this environment (Secret Service is registered)");
            // 成功した場合はround-tripとdeleteまで確認する。
            let loaded = store
                .load("spike10-test-service", "spike10-test-account")
                .expect("load should succeed after save");
            assert_eq!(loaded, "hunter2");
            store
                .delete("spike10-test-service", "spike10-test-account")
                .expect("delete should succeed");
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

    store
        .save("spike10-fallback-service", "user@example.com", "s3cr3t-token")
        .expect("save should succeed");
    let loaded = store
        .load("spike10-fallback-service", "user@example.com")
        .expect("load should succeed");
    assert_eq!(loaded, "s3cr3t-token");

    store
        .delete("spike10-fallback-service", "user@example.com")
        .expect("delete should succeed");
    let after_delete = store.load("spike10-fallback-service", "user@example.com");
    assert!(after_delete.is_err(), "loading a deleted credential should fail");
}

#[test]
fn encrypted_file_store_persists_across_separate_instances() {
    // マスターキーがファイルに保存され、プロセス再起動をまたいで同じ鍵を
    // 再利用できることを確認する(design.mdのトークン保存はアプリ再起動を
    // またいで有効である必要があるため)。
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");

    {
        let store = EncryptedFileStore::new(tmp_dir.path()).expect("failed to create store (1st)");
        store
            .save("persist-service", "acct", "persisted-secret")
            .expect("save should succeed");
    }
    {
        // 新しいインスタンス(プロセス再起動相当)。
        let store = EncryptedFileStore::new(tmp_dir.path()).expect("failed to create store (2nd)");
        let loaded = store
            .load("persist-service", "acct")
            .expect("load should succeed with a fresh store instance");
        assert_eq!(loaded, "persisted-secret");
    }
}

#[test]
fn fallback_store_succeeds_end_to_end_even_when_os_keyring_is_unavailable() {
    // design.md §12.4の本体: OSキーリングが使えなくても、フォールバック経由で
    // 最終的にround-tripが成立すること。CIやこの開発環境のようにSecret
    // Serviceが登録されていない場合でも、アプリ全体としてはトークン保存が
    // 機能し続ける、という合否基準に対応する。
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let store =
        FallbackCredentialStore::new(tmp_dir.path()).expect("failed to create fallback store");

    store
        .save("app-service", "app-account", "app-secret-value")
        .expect("save should succeed via primary or fallback");
    let loaded = store
        .load("app-service", "app-account")
        .expect("load should succeed via primary or fallback");
    assert_eq!(loaded, "app-secret-value");
    store
        .delete("app-service", "app-account")
        .expect("delete should succeed via primary or fallback");
}
