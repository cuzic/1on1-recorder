//! The user's selected live-transcription STT provider (task #47/#48/#49):
//! [`SttProviderKind`] plus the `credential-store` location its selection is
//! persisted under. Split out of `live_transcription` (rather than living inside
//! that `#[cfg(feature = "windows-supervisor")]`-gated module) since these are
//! plain data with no dependency on `credential-store`/`tracing`/capture crates —
//! `apps/desktop`'s settings screen needs to read/write the selection on every
//! platform (Linux dev builds included), not just where live transcription itself
//! actually runs. `live_transcription` re-exports these under its own path too, so
//! existing `live_transcription::SttProviderKind` usage on the Windows build keeps
//! working unchanged.

/// `credential-store` service name under which the user's STT provider selection
/// (see [`SELECTED_STT_PROVIDER_ACCOUNT`]) is stored. Same `"1on1-recorder"` string
/// as `summarize::CREDENTIAL_SERVICE`, kept as its own constant rather than a shared
/// one so this crate doesn't need a dependency on `summarize` just to reuse a string
/// literal (task #47).
pub const CREDENTIAL_SERVICE: &str = "1on1-recorder";

/// Account name under which the user's currently selected [`SttProviderKind`] (e.g.
/// `"deepgram"`) is stored — mirrors `summarize::SELECTED_PROVIDER_ACCOUNT`'s
/// "settings UI writes it, capture pipeline reads it" pattern.
pub const SELECTED_STT_PROVIDER_ACCOUNT: &str = "stt-selected-provider";

/// Which `stt-api::SttProvider` adapter a live transcription session should use. One
/// variant per adapter crate under `crates/stt-*`; `stt_wiring::build_stt_provider`
/// (task #47/#48, in `live_transcription`) constructs all four — every `match` over
/// this enum stays exhaustiveness-checked as adapters are added, instead of an
/// unimplemented one silently doing nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttProviderKind {
    Deepgram,
    OpenAi,
    Google,
    AssemblyAi,
}

impl SttProviderKind {
    /// Parses the string stored under [`SELECTED_STT_PROVIDER_ACCOUNT`]. `None` for
    /// anything unrecognized (a stale value from a newer build, a corrupted entry,
    /// etc.) rather than an error — callers fall back to [`SttProviderKind::default`],
    /// same as "no selection made yet".
    pub fn from_account_value(value: &str) -> Option<Self> {
        match value {
            "deepgram" => Some(Self::Deepgram),
            "openai" => Some(Self::OpenAi),
            "google" => Some(Self::Google),
            "assemblyai" => Some(Self::AssemblyAi),
            _ => None,
        }
    }

    pub fn as_account_value(self) -> &'static str {
        match self {
            Self::Deepgram => "deepgram",
            Self::OpenAi => "openai",
            Self::Google => "google",
            Self::AssemblyAi => "assemblyai",
        }
    }
}

impl Default for SttProviderKind {
    /// Deepgram was the only adapter wired in before task #47 — defaulting to it
    /// when nothing is selected keeps that behavior unchanged.
    fn default() -> Self {
        Self::Deepgram
    }
}
