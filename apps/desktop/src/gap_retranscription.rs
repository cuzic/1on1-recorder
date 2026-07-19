//! Task #92: gap re-transcription UI state — the transcript panel's per-gap
//! "この区間を再文字起こしする" button and the STT-provider-support check that
//! decides whether to show it at all, built on top of #91's
//! `app_service::retranscribe_gap`.
//!
//! Split out of `ui.rs` for the same reason `transcription_status.rs` is:
//! `app_service::retranscribe_gap`/`RetranscribeError`/`supports_batch_retranscription`
//! only exist when the `live-transcription` feature is compiled in, which
//! `apps/desktop`'s `Cargo.toml` only does for the `cfg(windows)` `app-service`
//! dependency edge (see that file's comment) — every other platform needs a
//! `#[cfg(not(windows))]` fallback that reports "not supported"/"unavailable"
//! instead of failing to compile. Unlike `transcription_status.rs`'s mirrored
//! enum, there is nothing here worth mirroring across platforms (a re-transcribe
//! attempt is either wired to the real thing or structurally unreachable — the
//! button that would trigger it never renders when
//! `supports_batch_retranscription` is `false`), so the `#[cfg(not(windows))]`
//! arms below are just stubs `ui.rs` can call unconditionally without its own
//! `#[cfg]`s.

use credential_store::CredentialStore;
use session_store::{SessionStore, TranscriptSegment, TranscriptionGap};

/// Per-gap client-side state for the "この区間を再文字起こしする" button
/// (`ui::App`'s `gap_retranscribe_state: Signal<HashMap<i64, GapRetranscribeState>>`,
/// keyed by `TranscriptionGap::id`) — a gap missing from that map reads as
/// idle/not-yet-attempted, mirroring how `ui::App`'s other per-action state
/// (`summary_busy`/`summary_message`, etc.) has no explicit "idle" variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapRetranscribeState {
    Loading,
    Error(String),
}

/// Which `credential-store`-selected STT provider (`app_service::SELECTED_STT_PROVIDER_ACCOUNT`,
/// the same key `settings::Settings`'s STT picker writes) is active right now —
/// read fresh on every call rather than cached, since it can change from the
/// settings screen at any time between renders. Falls back to
/// `SttProviderKind::default()` (Deepgram) the same way `settings::SttProvider::from_key`
/// does when nothing has been saved yet.
pub fn selected_provider_kind(credential_store: &dyn CredentialStore) -> app_service::SttProviderKind {
    credential_store
        .load(app_service::CREDENTIAL_SERVICE, app_service::SELECTED_STT_PROVIDER_ACCOUNT)
        .ok()
        .and_then(|key| app_service::SttProviderKind::from_account_value(&key))
        .unwrap_or_default()
}

/// Whether `kind` can be used for #91's gap re-transcription on this build —
/// the check the retranscribe button's visibility is gated on (task #92's
/// requirement #2: no button, just explanatory text, when this is `false`).
/// Always `false` on a non-Windows build regardless of `kind`, since
/// `app_service::retranscribe_gap` itself isn't compiled in there (see this
/// module's doc comment) — there is no adapter to be "supported" in the first
/// place, independent of `app_service::supports_batch_retranscription`'s own
/// per-provider answer on Windows.
#[cfg(windows)]
pub fn supports_batch_retranscription(kind: app_service::SttProviderKind) -> bool {
    app_service::supports_batch_retranscription(kind)
}
#[cfg(not(windows))]
pub fn supports_batch_retranscription(_kind: app_service::SttProviderKind) -> bool {
    false
}

/// Re-transcribes `gap` (task #91) and translates any failure into a Japanese
/// message fit to show right next to the gap marker it came from — the
/// per-gap counterpart of `transcription_status::describe`'s "translate the
/// domain error into a message a non-technical user can act on" role.
///
/// On non-Windows builds this is unreachable in practice (`supports_batch_retranscription`
/// is always `false` there, so `ui.rs` never renders the button that would call
/// this), but still needs a real body so `ui.rs`'s call site can stay
/// `#[cfg]`-free — see this module's doc comment.
#[cfg(windows)]
pub async fn retranscribe(
    gap: TranscriptionGap,
    provider_kind: app_service::SttProviderKind,
    store: &SessionStore,
    credential_store: &dyn CredentialStore,
) -> Result<Vec<TranscriptSegment>, String> {
    app_service::retranscribe_gap(gap, provider_kind, store, credential_store).await.map_err(describe_error)
}
#[cfg(not(windows))]
pub async fn retranscribe(
    _gap: TranscriptionGap,
    _provider_kind: app_service::SttProviderKind,
    _store: &SessionStore,
    _credential_store: &dyn CredentialStore,
) -> Result<Vec<TranscriptSegment>, String> {
    Err("この環境では再文字起こしに対応していません".to_string())
}

/// Every [`app_service::RetranscribeError`] variant listed explicitly (no
/// `_ => ...` catch-all) — same reasoning as that type's own doc comment gives
/// for its `UnsupportedProvider` variant: a case added there without a
/// matching arm here should fail to compile rather than silently fall back to
/// a generic message.
#[cfg(windows)]
fn describe_error(err: app_service::RetranscribeError) -> String {
    match err {
        app_service::RetranscribeError::GapStillOpen { .. } => "この区間はまだ接続が回復していないため、再文字起こしできません".to_string(),
        app_service::RetranscribeError::UnsupportedProvider { .. } => "選択中のSTTプロバイダは再文字起こしに対応していません".to_string(),
        app_service::RetranscribeError::CredentialMissing { .. } => "設定画面でAPIキーを設定してください".to_string(),
        app_service::RetranscribeError::NoAudioInRange { .. } => "この区間の録音データが見つかりませんでした".to_string(),
        app_service::RetranscribeError::SegmentStore(e) => format!("音声の読み込みに失敗しました: {e}"),
        app_service::RetranscribeError::Stt(e) => format!("文字起こしに失敗しました: {e}"),
        app_service::RetranscribeError::SessionStore(e) => format!("保存に失敗しました: {e}"),
    }
}
