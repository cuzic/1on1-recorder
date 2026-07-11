//! Provisions the bearer token `apps/desktop` reads via `credential-store` —
//! there's no in-app "log in" screen (not part of Phase 1A's UI scope), so this
//! is the only way to set it right now. Writes directly to the OS keyring
//! (Windows Credential Manager / macOS Keychain / Linux Secret Service) under the
//! same service/account `apps/desktop/src-tauri/src/config.rs` reads from
//! (`"1on1-recorder"` / `"api-token"`).
//!
//! Usage: `cargo run -p credential-store --example set_token -- <token>`

use credential_store::{CredentialStore, OsKeyringStore};

const SERVICE: &str = "1on1-recorder";
const ACCOUNT: &str = "api-token";

fn main() {
    let token = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: cargo run -p credential-store --example set_token -- <token>");
        std::process::exit(1);
    });

    OsKeyringStore.save(SERVICE, ACCOUNT, &token).unwrap_or_else(|e| {
        eprintln!("failed to save token to the OS keyring: {e}");
        std::process::exit(1);
    });

    println!("saved a token for service={SERVICE:?} account={ACCOUNT:?}");
}
