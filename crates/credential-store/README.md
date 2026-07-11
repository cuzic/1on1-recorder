# credential-store

OS keyring credential storage (design.md §12.4), ported from
`spikes/spike-10-credential-store`.

Application-internal, not a publishing candidate (placed here rather than under
`session-store`, per Codex's review of the original task list — credentials are an
auth/config boundary, not part of the session/segment ledger).

## Contents

- `OsKeyringStore` — Windows Credential Manager / macOS Keychain / Linux Secret
  Service, all through the `keyring` crate's one API.
- `EncryptedFileStore` — AES-256-GCM encrypted file fallback for when the OS keyring
  backend itself is unavailable (e.g. headless Linux with no Secret Service
  provider). See its module doc for the protection-level caveat: this is *not* as
  strong as DPAPI or Keychain, only OS file permissions.
- `FallbackCredentialStore` — tries the keyring first, falls back to the encrypted
  file only on a narrow set of "backend doesn't exist" errors (not on access-denied
  or locked, to avoid silently dropping to weaker protection).
- `CredentialStoreTokenProvider<S>` — adapts any `CredentialStore` into
  `upload_client::TokenProvider`, so `HttpUploadClient` can be handed a real
  credential-backed token source without depending on this crate directly.

## Known gaps

- `os_keyring_observed_behavior_in_this_environment` and
  `fallback_store_succeeds_end_to_end_even_when_os_keyring_is_unavailable` have only
  been run against this Linux dev environment's absent Secret Service (the fallback
  path). Per Codex's review, a real Windows Credential Manager check (does `save`/
  `load`/`delete` actually round-trip through DPAPI on real hardware) is still
  outstanding and should happen before Phase 1A's completion test (task #9).
- `CredentialStoreTokenProvider::refresh` is a no-op, matching a static bearer token
  with nothing to refresh. A future OAuth-style credential source would need a real
  implementation here.
