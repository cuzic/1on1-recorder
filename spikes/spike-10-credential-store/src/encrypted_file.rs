//! spike-plan.md SPIKE-10 検証手順2: Linuxでgnome-keyring(Secret Service)が
//! 不在の場合のフォールバック。design.md §12.4は「フォールバック(拒否 or
//! 暗号化ファイル)の判断材料を得る」とだけ定めているため、ここでは暗号化
//! ファイル方式を実装して実際に成立するかを確かめる。
//!
//! **保護レベルについての注記**: マスターキー自体をファイルシステム上に
//! 平文で置き、OSのファイル権限(0600)だけを保護境界にしている。これは
//! Windows Credential Manager(DPAPI, ユーザーログイン資格情報に紐づく)や
//! macOS Keychain(OS自体が管理する鍵ストア)と同じ強度の保護ではない。
//! 「同一マシン上の他ユーザーからは読めない」程度の保護であり、マシンの
//! ディスクを直接読める攻撃者やroot権限には無力である。実運用でこの経路を
//! 採用する場合はRESULT.mdにこの限界を明記すること。

use crate::error::StoreError;
use crate::CredentialStore;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use rand::RngCore;
use std::path::{Path, PathBuf};

pub struct EncryptedFileStore {
    dir: PathBuf,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl EncryptedFileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { dir })
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join(".master.key")
    }

    fn entry_path(&self, service: &str, account: &str) -> PathBuf {
        let id = format!("{service}:{account}");
        self.dir.join(hex_encode(id.as_bytes()))
    }

    fn load_or_create_master_key(&self) -> Result<[u8; 32], StoreError> {
        let path = self.key_path();
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                return Ok(key);
            }
        }
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        write_private_file(&path, &key)?;
        Ok(key)
    }
}

fn write_private_file(path: &Path, data: &[u8]) -> Result<(), StoreError> {
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    // aes-gcm crate自身のAeadCore::generate_nonceは使わず、randクレートの
    // OsRngで直接12バイトの乱数を引く(aes-gcmが内部で使うrand_coreの
    // バージョンとrandクレートのバージョンが食い違うとトレイト境界が
    // 合わない可能性があるため、依存関係を明示的に1本化する)。
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| StoreError::Crypto(e.to_string()))?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>, StoreError> {
    if data.len() < 12 {
        return Err(StoreError::Crypto("ciphertext too short".to_string()));
    }
    let (nonce_bytes, ciphertext) = data.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| StoreError::Crypto(e.to_string()))
}

impl CredentialStore for EncryptedFileStore {
    fn save(&self, service: &str, account: &str, secret: &str) -> Result<(), StoreError> {
        let key = self.load_or_create_master_key()?;
        let ciphertext = encrypt(&key, secret.as_bytes())?;
        write_private_file(&self.entry_path(service, account), &ciphertext)
    }

    fn load(&self, service: &str, account: &str) -> Result<String, StoreError> {
        let path = self.entry_path(service, account);
        let data = std::fs::read(&path).map_err(|_| StoreError::NotFound {
            service: service.to_string(),
            account: account.to_string(),
        })?;
        let key = self.load_or_create_master_key()?;
        let plaintext = decrypt(&key, &data)?;
        String::from_utf8(plaintext).map_err(|e| StoreError::Crypto(e.to_string()))
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), StoreError> {
        let path = self.entry_path(service, account);
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
