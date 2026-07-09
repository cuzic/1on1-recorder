//! spike-plan.md SPIKE-10: OS 資格情報ストアへのトークン保存(design.md §12.4)。

pub mod encrypted_file;
pub mod error;
pub mod os_keyring;

pub use encrypted_file::EncryptedFileStore;
pub use error::StoreError;
pub use os_keyring::OsKeyringStore;

pub trait CredentialStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError>;
    fn load(&self, service: &str, account: &str) -> Result<String, StoreError>;
    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError>;
}

/// design.md §12.4: 「Linuxでヘッドレス・Secret Service不在環境の
/// フォールバック方針を決められる」ことの実装。まずOSキーリングを試み、
/// バックエンド不在(Linuxで`org.freedesktop.secrets`が登録されていない等)を
/// 検出した場合のみ暗号化ファイルへフォールバックする。
///
/// **フォールバックする条件を絞っている理由**: OSキーリングが「起動している
/// が認証を拒否した」「一時的にロックされている」場合まで無条件にファイルへ
/// フォールバックすると、ユーザーが意図せず低強度の保護へ落ちてしまう。
/// ここでは`keyring`クレートが返すエラーのうち、バックエンドそのものが
/// 存在しないことを示すものだけをフォールバック対象とする。
pub struct FallbackCredentialStore {
    primary: OsKeyringStore,
    fallback: EncryptedFileStore,
}

impl FallbackCredentialStore {
    pub fn new(fallback_dir: impl Into<std::path::PathBuf>) -> Result<Self, StoreError> {
        Ok(Self {
            primary: OsKeyringStore,
            fallback: EncryptedFileStore::new(fallback_dir)?,
        })
    }

    fn should_fallback(err: &StoreError) -> bool {
        match err {
            StoreError::Backend(msg) => {
                // keyringクレートのLinux(secret-service backend)実装は、
                // Secret Serviceプロバイダ不在時にD-Bus接続エラー文字列を
                // 返す(実際にこの環境で観測した文字列: "org.freedesktop.secrets"
                // や"NoSuchMethod"/"ServiceUnknown"を含む)。この実装依存の
                // 判定はSPIKE-11以降で正式なエラー分類に置き換えるべき
                // 暫定措置であることに注意。
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
            Err(e) if Self::should_fallback(&e) => self.fallback.load(service, account),
            Err(e) => Err(e),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError> {
        match self.primary.delete(service, account) {
            Ok(()) => Ok(()),
            Err(e) if Self::should_fallback(&e) => self.fallback.delete(service, account),
            Err(e) => Err(e),
        }
    }
}
