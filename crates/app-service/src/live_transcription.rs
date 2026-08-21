//! Real-time transcription side channel (design's "録音中にリアルタイムで文字起こしを
//! 表示したい"): consumes the raw-PCM side channel `windows_frame_collector::collect_frames`
//! feeds (mirroring how `level_sink` is fed, for the same "cheap side channel, batch
//! `run_pipeline` stays untouched" reason — see that module's doc comment), streams it
//! into per-track sessions of whichever `stt-*` adapter the user selected (Deepgram,
//! OpenAI, Google, or AssemblyAI — see [`SttProviderKind`]/`stt_wiring::build_stt_provider`,
//! task #47/#48), resampling to that provider's required rate first (see
//! `crate::resample` and `stt_wiring::target_sample_rate_hz`), and persists every
//! `SttEvent::PartialTranscript`/`FinalTranscript` via
//! `SessionStore::insert_transcript_segment`.
//!
//! Gated behind the `live-transcription` feature (see `app-service`'s `Cargo.toml`):
//! without it, [`run_live_transcription`] below compiles to a stub that just drains
//! and discards `audio_rx`, so `windows_session::run_windows_capture_session` doesn't
//! need any `#[cfg]` of its own at the call site, and a plain `--features
//! windows-supervisor` build never pulls in any `stt-*` adapter's websocket/TLS/gRPC
//! stack.
//!
//! Like the rest of `windows_supervisor`/`windows_session`, never run against a real
//! STT provider connection or real Windows hardware in this environment —
//! cross-compile-checked only (see this crate's README).
//!
//! No macOS equivalent yet: `macos_frame_collector`/`macos_session` would need the
//! identical `stt_sink`/`run_live_transcription` wiring once `capture-macos` is ever
//! actually compiled/run (see that crate's own doc comment) — out of scope here since
//! macOS capture itself isn't in scope yet.

use std::sync::{Arc, Mutex};

use credential_store::CredentialStore;
// `local-broker`はapp-serviceの必須（非optional）依存のため、featureに関わらず
// 常にコンパイルできる。`#[cfg(not(feature = "live-transcription"))]`側のstub版
// `run_live_transcription`もこの型を引数に取るため、モジュール直下で読み込む
// （以前は`stt_wiring`サブモジュール内だけでimportしており、live-transcription
// feature無効時にstub関数がこの型を解決できずコンパイルエラーになっていた）。
use local_broker::LocalBroker;
use recorder_domain::{SessionId, TrackKind};
use session_store::SessionStore;
use tokio::sync::mpsc::Receiver;

/// Per-track Deepgram connection status (task #52): the desktop UI can't otherwise
/// tell "nobody has spoken yet" from "STT is broken" when the transcript panel is
/// empty. Updated by [`run_live_transcription`] as each track's session state
/// changes, via the same `Arc<Mutex<_>>` side-channel shape as `LevelSnapshot` (see
/// `windows_frame_collector`). Two independent fields rather than one aggregate
/// status, since Self and Remote each open their own Deepgram session (see this
/// module's doc comment) and can fail independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackTranscriptionStatus {
    /// No Deepgram API key configured — a connection was never attempted.
    NotConfigured,
    /// `start_session` is in flight.
    Connecting,
    /// Session open and streaming.
    Connected,
    /// `start_session` failed, or the session reported an `SttEvent::Error` after
    /// connecting. Carries `SttError::to_string()` rather than the typed error
    /// itself, so this type — and the desktop crate's mirror of it (see
    /// `apps/desktop/src/transcription_status.rs`) — never needs an `stt-api`
    /// dependency.
    Error(String),
    /// `live-transcription` feature disabled, or the running platform has no live
    /// transcription wiring at all (e.g. macOS/dev builds — see this module's doc
    /// comment on scope).
    Unavailable,
}

impl Default for TrackTranscriptionStatus {
    /// Before the first update lands, "unknown yet" reads closer to `NotConfigured`
    /// than to claiming a connection is already in flight.
    fn default() -> Self {
        Self::NotConfigured
    }
}

#[derive(Debug, Clone, Default)]
pub struct TranscriptionStatus {
    pub self_status: TrackTranscriptionStatus,
    pub remote_status: TrackTranscriptionStatus,
}

fn set_status(sink: &Option<Arc<Mutex<TranscriptionStatus>>>, track: TrackKind, status: TrackTranscriptionStatus) {
    let Some(sink) = sink else { return };
    let mut guard = sink.lock().unwrap();
    match track {
        TrackKind::SelfMic => guard.self_status = status,
        TrackKind::RemoteAudio => guard.remote_status = status,
    }
}

fn set_both_status(sink: &Option<Arc<Mutex<TranscriptionStatus>>>, status: TrackTranscriptionStatus) {
    let Some(sink) = sink else { return };
    let mut guard = sink.lock().unwrap();
    guard.self_status = status.clone();
    guard.remote_status = status;
}

/// Same label format as `apps/desktop/src/transcript.rs::speaker_label` and
/// `rhai-engine`'s dispatcher-local copy — duplicated here rather than shared,
/// since `app-service` can't depend on the desktop binary crate and pulling in
/// `rhai-engine` just for this would be backwards (that crate depends on
/// `recorder-domain`, not the other way around).
fn speaker_label(track: Option<TrackKind>, speaker: Option<u32>) -> String {
    let base = match track {
        Some(TrackKind::SelfMic) => "自分",
        Some(TrackKind::RemoteAudio) => "相手",
        None => "不明",
    };
    match speaker {
        Some(n) => format!("{base} (話者{})", n + 1),
        None => base.to_string(),
    }
}

/// Re-exported from `crate::stt_provider_kind` (task #49) so existing
/// `live_transcription::{CREDENTIAL_SERVICE, SELECTED_STT_PROVIDER_ACCOUNT,
/// SttProviderKind}` paths keep working — that module is the canonical home now,
/// since it has no `windows-supervisor`-only dependencies and needs to be reachable
/// from every platform (e.g. `apps/desktop`'s settings screen).
pub use crate::stt_provider_kind::{SttProviderKind, CREDENTIAL_SERVICE, SELECTED_STT_PROVIDER_ACCOUNT};

#[cfg(feature = "live-transcription")]
mod stt_wiring {
    use super::*;
    use crate::resample::resample;
    use crate::silence_gate::{GateAction, GateConfig, SilenceGate};
    use crate::timestamp_mapper::TimestampMapper;
    // LocalBrokerはモジュール直下（`super`）でimport済み（`use super::*;`で入る）。
    use session_store::TranscriptSegment;
    use stt_api::{AudioChunk, KeepAliveEffect, SttError, SttEvent, SttProvider, SttSession, SttSessionConfig};
    use stt_assemblyai::AssemblyAIProvider;
    use stt_deepgram::DeepgramProvider;
    use stt_google::{GoogleProvider, GoogleSttCredentials};
    use stt_openai::OpenAiProvider;
    use transcript_event::{self, EventEnvelope, Finality, TranscriptEvent, UtteranceEndReason};
    // `Receiver<(TrackKind, Vec<f32>, u32)>` (the `audio_rx` side channel) comes
    // from `super::*` above; `UnboundedReceiver` is only needed in here, for each
    // provider's per-track `SttEvent` stream (`SttProvider::start_session`'s
    // return type) — kept local rather than added to the outer `use` so a
    // `windows-supervisor`-only build (this module doesn't exist without
    // `live-transcription`) doesn't warn about an unused import.
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::task::{JoinError, JoinHandle};
    use tokio::time::{Duration, Instant};

    /// Shortest STT outage worth persisting to `transcription_gaps` (task
    /// #90). Every disconnect opens a gap row immediately (see
    /// `open_gap`/`ReconnectState::open_gap_id`) since its eventual length
    /// isn't known up front, but a track that reconnects (or a keepalive
    /// that succeeds again) within about one `reconnect_backoff` attempt of
    /// going down hasn't actually lost any transcript worth flagging for
    /// task #91/#92's manual re-transcription UI — recording every one of
    /// those would make the gap list mostly noise instead of the handful of
    /// outages that genuinely dropped speech. 1s is comfortably above
    /// `reconnect_backoff`'s 500ms floor (so a same-attempt reconnect never
    /// counts) while still well under what a real network/provider outage
    /// worth re-transcribing looks like in practice.
    const MIN_RECORDED_GAP_MS: u64 = 1_000;

    /// How long the Remote track's STT session may go without any real (Send/
    /// SendStitched) audio before [`run_live_transcription`]'s keepalive timer
    /// calls `SttSession::keep_alive` to stop the provider's idle-connection
    /// timeout from firing during a long silence-gated gap. Comfortably shorter
    /// than any known provider's idle timeout, with headroom for the 1s timer
    /// granularity below.
    const REMOTE_KEEPALIVE_IDLE_THRESHOLD: Duration = Duration::from_secs(5);

    /// Bound on [`SttProvider::start_session`] (task #85): without this, a
    /// black-holed network (connect sent, nothing ever comes back) hangs
    /// `run_live_transcription` forever, which — via `windows_session`'s
    /// `tokio::join!(capture_fut, live_transcription_fut)` — takes the whole
    /// recording-stop/save/upload path down with it. 10s is comfortably above how
    /// long opening a WebSocket/gRPC connection and any auth handshake takes on a
    /// healthy network (sub-second in practice), while still bounding the worst
    /// case to something a user waiting to stop a recording will tolerate. A
    /// timeout here is treated exactly like a `start_session` `Err`: that track
    /// just runs without live transcription for the rest of the session.
    const START_SESSION_TIMEOUT: Duration = Duration::from_secs(10);

    /// Bound on [`SttSession::finalize`] (task #81): `finalize` waits for the
    /// provider to drain and acknowledge whatever audio is still in flight
    /// server-side, so it can legitimately take longer than opening the
    /// connection did — but it must not be unbounded, since it runs on recording
    /// stop, ahead of the audio-save/upload path (see `START_SESSION_TIMEOUT`'s
    /// doc comment for why an unbounded await here is a problem). 8s gives the
    /// provider room to drain a few seconds of buffered audio without making the
    /// user wait indefinitely for a black-holed connection to time out on its own.
    const FINALIZE_TIMEOUT: Duration = Duration::from_secs(8);

    /// Errors from [`build_stt_provider`] — distinct from `stt_api::SttError`, which
    /// covers failures *within* an already-constructed provider's session, not
    /// "there is no provider to construct at all".
    #[derive(Debug, thiserror::Error)]
    pub enum SttProviderFactoryError {
        #[error("no credential configured for STT provider {kind:?}: {source}")]
        CredentialMissing {
            kind: SttProviderKind,
            #[source]
            source: credential_store::StoreError,
        },
        /// Google's stored credential is a JSON blob (see [`GoogleSttCredentials`]
        /// and this module's `build_stt_provider` arm for `SttProviderKind::Google`),
        /// not a bare string like the other three providers' — so unlike
        /// `CredentialMissing`, this is reachable even once *something* is stored
        /// under the account, if that something isn't valid JSON for the shape.
        #[error("stored credential for STT provider {kind:?} is not valid: {source}")]
        InvalidCredential {
            kind: SttProviderKind,
            #[source]
            source: serde_json::Error,
        },
    }

    /// Constructs the `Box<dyn SttProvider>` for `kind`, loading whatever credential
    /// that provider needs from `credential_store`. Every [`SttProviderKind`] variant
    /// is listed explicitly (no `_ => ...` catch-all) so adding a fifth adapter's enum
    /// variant without a matching arm here fails to compile instead of silently
    /// falling through.
    pub fn build_stt_provider(kind: SttProviderKind, credential_store: &dyn CredentialStore) -> Result<Box<dyn SttProvider>, SttProviderFactoryError> {
        match kind {
            SttProviderKind::Deepgram => {
                let api_key = credential_store
                    .load(stt_deepgram::CREDENTIAL_SERVICE, stt_deepgram::DEEPGRAM_API_KEY_ACCOUNT)
                    .map_err(|source| SttProviderFactoryError::CredentialMissing { kind, source })?;
                Ok(Box::new(DeepgramProvider::new(api_key)))
            }
            SttProviderKind::OpenAi => {
                let api_key = credential_store
                    .load(stt_openai::CREDENTIAL_SERVICE, stt_openai::OPENAI_STT_API_KEY_ACCOUNT)
                    .map_err(|source| SttProviderFactoryError::CredentialMissing { kind, source })?;
                Ok(Box::new(OpenAiProvider::new(api_key)))
            }
            SttProviderKind::Google => {
                let raw = credential_store
                    .load(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT)
                    .map_err(|source| SttProviderFactoryError::CredentialMissing { kind, source })?;
                let credentials: GoogleSttCredentials =
                    serde_json::from_str(&raw).map_err(|source| SttProviderFactoryError::InvalidCredential { kind, source })?;
                Ok(Box::new(GoogleProvider::new(credentials)))
            }
            SttProviderKind::AssemblyAi => {
                let api_key = credential_store
                    .load(stt_assemblyai::CREDENTIAL_SERVICE, stt_assemblyai::ASSEMBLYAI_API_KEY_ACCOUNT)
                    .map_err(|source| SttProviderFactoryError::CredentialMissing { kind, source })?;
                Ok(Box::new(AssemblyAIProvider::new(api_key)))
            }
        }
    }

    /// Sample rate each provider's session must be opened at (task #48):
    /// `stt-openai` requires exactly 24kHz and `stt-assemblyai` requires exactly
    /// 16kHz mono PCM16, both hard-rejecting any other rate at `start_session` (see
    /// each crate's module doc comment) — `capture_rate_hz` audio is resampled down
    /// to these before `send_audio` (see `run_live_transcription` below).
    /// `stt-deepgram` accepts whatever rate it's given and `stt-google` only requires
    /// a nonzero one, so for those the capture pipeline's own nominal rate is used
    /// unchanged, avoiding a needless resample. Centralized here — one place to
    /// update if a provider's required rate ever changes — rather than duplicated at
    /// each call site.
    fn target_sample_rate_hz(kind: SttProviderKind, capture_rate_hz: u32) -> u32 {
        match kind {
            SttProviderKind::OpenAi => 24_000,
            SttProviderKind::AssemblyAi => 16_000,
            SttProviderKind::Deepgram | SttProviderKind::Google => capture_rate_hz,
        }
    }

    /// Opens one `provider` session bounded by [`START_SESSION_TIMEOUT`] and
    /// updates `status_sink` for `track_kind` accordingly (task #85). A timeout is
    /// reported the same way a `start_session` `Err` already was — `Error` status
    /// carrying `SttError::Timeout`'s message, `None` returned — so callers (i.e.
    /// [`run_live_transcription`]) don't need a third case: that track simply runs
    /// without live transcription for the rest of the session either way.
    async fn start_session_with_timeout(
        provider: &dyn SttProvider,
        config: SttSessionConfig,
        track: &'static str,
        track_kind: TrackKind,
        status_sink: &Option<Arc<Mutex<TranscriptionStatus>>>,
    ) -> Option<(Box<dyn SttSession>, UnboundedReceiver<SttEvent>)> {
        match tokio::time::timeout(START_SESSION_TIMEOUT, provider.start_session(config)).await {
            Ok(Ok((session, events))) => {
                set_status(status_sink, track_kind, TrackTranscriptionStatus::Connected);
                Some((session, events))
            }
            Ok(Err(err)) => {
                tracing::warn!(%err, track, "live transcription: failed to start STT session");
                set_status(status_sink, track_kind, TrackTranscriptionStatus::Error(err.to_string()));
                None
            }
            Err(_) => {
                tracing::warn!(track, timeout_secs = START_SESSION_TIMEOUT.as_secs(), "live transcription: start_session timed out");
                set_status(status_sink, track_kind, TrackTranscriptionStatus::Error(stt_api::SttError::Timeout.to_string()));
                None
            }
        }
    }

    /// Backoff between STT reconnect attempts (task #82): same doubling-with-cap
    /// shape as `windows_supervisor::backoff_for_attempt` (500ms base, doubling,
    /// capped at 30s) — kept as its own local function rather than shared, since
    /// that one lives in a module this crate's `live-transcription` code has no
    /// other reason to depend on, and the two backoffs govern unrelated
    /// subsystems (capture device rebinding vs. STT session reconnects) that
    /// happen to want the same curve.
    fn reconnect_backoff(attempt: u32) -> Duration {
        let base_ms = 500u64;
        let max_ms = 30_000u64;
        Duration::from_millis(base_ms.saturating_mul(1u64 << attempt.min(6)).min(max_ms))
    }

    /// The pair [`SttProvider::start_session`] (fresh connect or reconnect)
    /// resolves to on success — named so [`ReconnectState::in_flight`] and
    /// [`attempt_reconnect`]'s return type don't have to spell it out twice.
    type ReconnectSessionPair = (Box<dyn SttSession>, UnboundedReceiver<SttEvent>);

    /// Per-track backoff state for reconnecting a disconnected STT session (task
    /// #82) — shared by the Self track (a loose local in
    /// [`run_live_transcription`]) and the Remote track (a field on
    /// [`RemoteGateState`]). Kept as its own small struct, consulted from a
    /// `select!` arm via `tokio::time::sleep_until`, rather than a direct
    /// `tokio::time::sleep(...).await` at the point a disconnect is detected —
    /// the latter would block every other branch of the loop (`audio_rx`, the
    /// other track's events, the keepalive timer) for the whole backoff
    /// duration, which is exactly the bug this task exists to avoid.
    ///
    /// Task #87 went a step further: even the backoff-armed retry itself used
    /// to run as a synchronous `.await` right inside the `select!` arm that
    /// fired it, blocking the loop for up to [`START_SESSION_TIMEOUT`] on a
    /// black-holed reconnect attempt — same class of bug, just moved from "the
    /// backoff sleep" to "the connect attempt after it". `in_flight` below is
    /// what fixes that: the attempt itself now runs as a detached
    /// `tokio::spawn`ed task (see [`attempt_reconnect`]), and this field is the
    /// handle the `select!` loop awaits *without* blocking any other branch
    /// while it does.
    struct ReconnectState {
        /// Consecutive failed reconnect attempts since the track last had a
        /// healthy session — feeds `reconnect_backoff`, and resets to 0 once a
        /// reconnect succeeds (or the disconnect turns out non-retryable, so
        /// there's nothing to back off from anymore).
        attempt: u32,
        /// `Some(instant)` while a retry is armed and pending; `None` when the
        /// track is healthy, or has given up until the recording ends (a
        /// non-retryable disconnect — see `SttError::is_retryable`).
        retry_at: Option<Instant>,
        /// When the track was first detected as disconnected in the *current*
        /// outage. Set once by the first `note_disconnect` and left unchanged
        /// across any failed retries in between (`note_disconnect` only fills
        /// this in if it's empty), so a reconnect that finally succeeds after
        /// several failed attempts sizes its timestamp-continuity correction
        /// (Remote only — see `RemoteGateState::apply_reconnect_offset`) off the
        /// whole outage, not just the time since the last failed attempt.
        disconnected_at: Option<Instant>,
        /// `Some(handle)` while a `tokio::spawn`ed [`attempt_reconnect`] call for
        /// this track is running (task #87) — mutually exclusive with `retry_at`
        /// being armed: `retry_at` is cleared the moment the backoff fires and a
        /// task is spawned, and only re-armed (by the `select!` loop, via
        /// `note_disconnect`) once that task's outcome comes back. Guards against
        /// spawning a second attempt for the same track while one is already in
        /// flight — see `note_disconnect_and_maybe_reconnect`'s guard.
        in_flight: Option<JoinHandle<Option<ReconnectSessionPair>>>,
        /// The `transcription_gaps` row id opened for the *current* outage
        /// (task #90), if any — `Some` from the moment `open_gap` first
        /// writes it until `close_open_gap` closes it (successful reconnect,
        /// or the recording ending with the track still down). Unlike every
        /// other field on this struct, deliberately **not** cleared by
        /// `reset()`: a non-retryable disconnect calls `reset()` to give up
        /// on reconnecting, but the gap it opened must stay open until the
        /// recording actually ends (see the `audio_rx.recv()` `None` arm in
        /// `run_live_transcription`, the only other place that closes it).
        open_gap_id: Option<i64>,
        /// `open_gap_id`'s row's `start_ms`, kept alongside it (rather than
        /// re-derived from `disconnected_at`, which *is* cleared by `reset()`)
        /// so `close_open_gap` can size the outage even after a give-up
        /// `reset()` has already cleared everything else.
        open_gap_start_ms: Option<u64>,
    }

    impl ReconnectState {
        fn new() -> Self {
            Self { attempt: 0, retry_at: None, disconnected_at: None, in_flight: None, open_gap_id: None, open_gap_start_ms: None }
        }

        /// Arms (or re-arms, after another failed attempt) a retry.
        fn note_disconnect(&mut self) {
            self.disconnected_at.get_or_insert_with(Instant::now);
            self.retry_at = Some(Instant::now() + reconnect_backoff(self.attempt));
            self.attempt = self.attempt.saturating_add(1);
        }

        /// A reconnect succeeded, or the disconnect was non-retryable so there's
        /// nothing left to retry: back to the healthy/gave-up baseline.
        /// `open_gap_id`/`open_gap_start_ms` (task #90) are the one exception —
        /// see their own doc comments for why a give-up `reset()` must not
        /// clear them.
        fn reset(&mut self) {
            let open_gap_id = self.open_gap_id;
            let open_gap_start_ms = self.open_gap_start_ms;
            *self = Self::new();
            self.open_gap_id = open_gap_id;
            self.open_gap_start_ms = open_gap_start_ms;
        }

        /// Aborts (rather than waits out) an in-flight reconnect task once the
        /// recording has ended (`audio_rx` closed) — called from the
        /// `audio_rx.recv()` arm's `None` branch in [`run_live_transcription`].
        /// A session that hasn't finished connecting yet has nothing left to
        /// receive once there's no more audio to send it, so letting it run out
        /// the rest of `START_SESSION_TIMEOUT` in the background would only
        /// delay this function's return past the audio-save/upload path it
        /// precedes for no benefit (see `START_SESSION_TIMEOUT`'s own doc
        /// comment for why an unbounded wait in that position is a problem) —
        /// and without this, the task would otherwise be silently detached
        /// (dropping a `JoinHandle` does not cancel the task it came from) and
        /// keep running past `run_live_transcription`'s return.
        fn abort_in_flight(&mut self) {
            if let Some(handle) = self.in_flight.take() {
                handle.abort();
            }
        }
    }

    /// Whether a disconnect signal (task #82: an `SttEvent::Error`, a
    /// `send_audio`/`keep_alive` call returning `Err`, or the track's events
    /// channel closing with no explicit error at all) should be retried with
    /// backoff, versus left in `Error` status for the rest of the recording.
    /// Delegates to [`SttError::is_retryable`] when an error is available;
    /// `None` (the events-channel-closed case) has no error to classify and is
    /// treated as retryable — a provider force-closing an otherwise healthy
    /// session with no `SttEvent::Error` at all is exactly what reconnecting is
    /// for (e.g. AssemblyAI's 3-hour hard cap, which closes the socket with no
    /// error event — see `stt_assemblyai`'s `AssemblyAISession::keep_alive` doc
    /// comment).
    fn is_disconnect_retryable(err: Option<&SttError>) -> bool {
        err.is_none_or(|err| err.is_retryable())
    }

    /// `recording_started.elapsed()` in whole milliseconds — the clock every
    /// `transcription_gaps` row's `start_ms`/`end_ms` (task #90) is measured
    /// against: wall-clock time since `run_live_transcription` began, not
    /// either track's own STT-provider-relative clock (which resets on every
    /// reconnect — see `RemoteGateState::apply_reconnect_offset`'s doc
    /// comment, and `note_self_disconnect`'s on why Self has no equivalent
    /// correction at all). Deliberately coarser than
    /// `TranscriptSegment.start_ms`/`end_ms` (sub-second, `TimestampMapper`-
    /// corrected on the Remote track): a gap only needs to point task #91's
    /// re-transcription pass at roughly the right stretch of `segments`, not
    /// pin an exact millisecond.
    fn ms_since_start(recording_started: Instant) -> u64 {
        recording_started.elapsed().as_millis() as u64
    }

    /// Opens a new `transcription_gaps` row for `track_kind` (task #90),
    /// called once per outage from the "genuinely new disconnect" branch of
    /// [`note_disconnect_and_maybe_reconnect`] — see that function's doc
    /// comment for the guard that keeps this from firing twice for the same
    /// outage. Written immediately, before it's known whether (or how soon)
    /// a reconnect will succeed, so a long outage still shows up even if the
    /// process crashes mid-outage; [`close_open_gap`] discards it later
    /// instead if it turns out shorter than [`MIN_RECORDED_GAP_MS`]. A
    /// failure to write the row is logged and simply means this outage won't
    /// be offered for later manual re-transcription — no different in spirit
    /// from any other best-effort persistence call in this module (e.g.
    /// `persist_event`'s `store.insert_transcript_segment` callers).
    fn open_gap(store: &SessionStore, session_id: SessionId, recording_started: Instant, track_kind: TrackKind, reconnect: &mut ReconnectState) {
        let start_ms = ms_since_start(recording_started);
        match store.record_gap_start(session_id, track_kind, start_ms) {
            Ok(id) => {
                reconnect.open_gap_id = Some(id);
                reconnect.open_gap_start_ms = Some(start_ms);
            }
            Err(err) => tracing::warn!(%err, ?track_kind, "live transcription: failed to record gap start"),
        }
    }

    /// Closes whichever gap `open_gap` opened on `reconnect` (task #90), if
    /// any — called once `track_kind`'s outage is over, whether because it
    /// reconnected successfully (the two reconnect-completion `select!` arms
    /// in `run_live_transcription`, and `RemoteGateState::apply_reconnect_offset`)
    /// or because the recording itself ended while the track was still down
    /// (the `audio_rx.recv()` `None` arm). A no-op if there's no open gap —
    /// the track was never disconnected, or `open_gap` failed to write one
    /// and already logged why. Discards the row instead of recording its
    /// `end_ms` if the outage turned out shorter than
    /// [`MIN_RECORDED_GAP_MS`] — see that constant's doc comment.
    fn close_open_gap(store: &SessionStore, session_id: SessionId, recording_started: Instant, track_kind: TrackKind, reconnect: &mut ReconnectState) {
        let (Some(gap_id), Some(start_ms)) = (reconnect.open_gap_id.take(), reconnect.open_gap_start_ms.take()) else {
            return;
        };
        let end_ms = ms_since_start(recording_started);
        let result = if end_ms.saturating_sub(start_ms) < MIN_RECORDED_GAP_MS {
            store.discard_gap(gap_id)
        } else {
            store.record_gap_end(gap_id, end_ms)
        };
        if let Err(err) = result {
            tracing::warn!(%err, ?track_kind, %session_id, "live transcription: failed to close transcription gap");
        }
    }

    /// Shared "what happens when a track disconnects" policy (task #82),
    /// mirroring `upload_worker::upload_pending_once`'s `retryable`
    /// classification pattern: reports `err` (if any) on `status_sink`, then
    /// arms a backoff-guarded reconnect if the disconnect is retryable
    /// ([`is_disconnect_retryable`]), or gives up until the recording ends
    /// otherwise. Does not touch the session slot itself — callers (the Self
    /// track's own disconnect sites, and `RemoteGateState::note_disconnect`)
    /// clear that immediately before calling this, since its shape differs
    /// between the two tracks.
    ///
    /// Task #90: also opens a `transcription_gaps` row via [`open_gap`] for
    /// a genuinely new outage (same guard as the reconnect-arming logic
    /// below) — a permanent (non-retryable) disconnect opens one too, even
    /// though `reconnect.reset()` immediately follows, since `reset()`
    /// deliberately leaves `open_gap_id`/`open_gap_start_ms` alone (see
    /// their own doc comments) and the recording-end arm in
    /// `run_live_transcription` is what eventually closes it.
    fn note_disconnect_and_maybe_reconnect(
        store: &SessionStore,
        session_id: SessionId,
        recording_started: Instant,
        track_kind: TrackKind,
        err: Option<&SttError>,
        status_sink: &Option<Arc<Mutex<TranscriptionStatus>>>,
        reconnect: &mut ReconnectState,
    ) {
        if let Some(err) = err {
            set_status(status_sink, track_kind, TrackTranscriptionStatus::Error(err.to_string()));
        }
        if reconnect.retry_at.is_some() || reconnect.in_flight.is_some() {
            // Already armed (or already reconnecting) for this outage: the
            // session slot was cleared by an earlier call to this function, so a
            // second disconnect signal here (e.g. a stale receiver delivering
            // another `SttEvent::Error` before it finally closes) reports the
            // same outage again, not a new one — restacking the backoff on top
            // of itself, or spawning a second concurrent reconnect attempt for
            // the same track (task #87), would just waste a connection for no
            // reason. A `transcription_gaps` row for this outage was already
            // opened (or attempted) by the call that armed it, so this must not
            // open a second one too (task #90).
            return;
        }
        open_gap(store, session_id, recording_started, track_kind, reconnect);
        if is_disconnect_retryable(err) {
            reconnect.note_disconnect();
        } else {
            reconnect.reset();
        }
    }

    /// Clears `self_session` and applies [`note_disconnect_and_maybe_reconnect`]
    /// for the Self track — the Self-track counterpart of
    /// `RemoteGateState::note_disconnect`, kept as a free function since the
    /// Self track's session isn't wrapped in a struct of its own.
    fn note_self_disconnect(
        self_session: &mut Option<Box<dyn SttSession>>,
        reconnect: &mut ReconnectState,
        status_sink: &Option<Arc<Mutex<TranscriptionStatus>>>,
        err: Option<&SttError>,
        store: &SessionStore,
        session_id: SessionId,
        recording_started: Instant,
    ) {
        *self_session = None;
        note_disconnect_and_maybe_reconnect(store, session_id, recording_started, TrackKind::SelfMic, err, status_sink, reconnect);
    }

    /// Attempts one reconnect for `track_kind` once its backoff timer fires (see
    /// the `select!` arms in [`run_live_transcription`]) — shared by Self and
    /// Remote, which otherwise differ only in where the resulting
    /// session/events pair gets stored and (Remote only) how the timestamp
    /// baseline is corrected afterward. Bounded by `start_session_with_timeout`
    /// exactly like an initial connect attempt, so a black-holed network during
    /// a reconnect is bounded the same way the first connect was.
    ///
    /// Task #87: run via `tokio::spawn` from the `select!` loop rather than
    /// awaited directly in the arm that fires it — a synchronous await here used
    /// to block the whole loop (so `audio_rx`, the other track's events, and the
    /// keepalive timer all went unpolled) for up to `START_SESSION_TIMEOUT`
    /// while one track reconnected, silently dropping audio queued behind the
    /// bounded `stt_tx` channel (task #86) in the meantime. Being spawned means
    /// this function must be entirely self-contained — no `&mut ReconnectState`
    /// parameter, unlike a prior version — since a spawned task can't borrow
    /// state that lives in `run_live_transcription`'s stack frame. `provider` is
    /// therefore `Arc<dyn SttProvider>` rather than `&dyn SttProvider`: the
    /// spawned task needs to own (or share ownership of) everything it touches,
    /// and an `Arc` clone is cheap enough to pay per reconnect attempt.
    ///
    /// **Does not touch `ReconnectState` at all**, on success or failure — the
    /// caller (the `select!` arm that awaits this task's `JoinHandle` once it
    /// completes) is entirely responsible for that, via `ReconnectState::note_disconnect`
    /// on failure or `ReconnectState::reset`/`RemoteGateState::apply_reconnect_offset`
    /// on success. This mirrors the prior version's "does not reset on success"
    /// contract (see its own history: `RemoteGateState::apply_reconnect_offset`
    /// needs `reconnect.disconnected_at` to still be the *original* disconnect
    /// time when it runs, right after this task's outcome is applied, to size
    /// its timestamp-continuity correction off the whole outage) — just taken
    /// one step further, since there is no `&mut ReconnectState` in here to
    /// touch even on failure now.
    async fn attempt_reconnect(
        provider: Arc<dyn SttProvider>,
        config: SttSessionConfig,
        track: &'static str,
        track_kind: TrackKind,
        status_sink: Option<Arc<Mutex<TranscriptionStatus>>>,
    ) -> Option<ReconnectSessionPair> {
        set_status(&status_sink, track_kind, TrackTranscriptionStatus::Connecting);
        start_session_with_timeout(provider.as_ref(), config, track, track_kind, &status_sink).await
    }

    /// Awaits the outcome of an in-flight [`attempt_reconnect`] task (task #87),
    /// or never resolves if there isn't one — the reconnect-completion
    /// counterpart of [`recv_track_event`] (see that function's doc comment for
    /// why a `select!` branch needs this shape rather than an `unwrap()` behind
    /// an `if` guard).
    async fn recv_reconnect_result(in_flight: &mut Option<JoinHandle<Option<ReconnectSessionPair>>>) -> Result<Option<ReconnectSessionPair>, JoinError> {
        match in_flight {
            Some(handle) => handle.await,
            None => std::future::pending().await,
        }
    }

    /// Applies a failed [`attempt_reconnect`] task's outcome to `reconnect`'s
    /// backoff bookkeeping (task #87) — shared by the two failure cases a
    /// `select!` reconnect-completion arm can see once `attempt_reconnect`'s
    /// `JoinHandle` resolves:
    /// - `Ok(None)`: `start_session_with_timeout` already reported `Error`
    ///   status itself (an `Err`/timeout from `start_session`), so this only
    ///   needs to arm the next backoff.
    /// - `Err(join_err)`: the task panicked or was cancelled before it could
    ///   report anything — unlike the above, nothing has set `Error` status yet,
    ///   so this does that too before arming the next backoff, the same way a
    ///   `start_session` failure already does.
    ///
    /// Does not handle the success case (`Ok(Some(pair))`) — callers install the
    /// new session/events themselves and call `reconnect.reset()` (Self) or
    /// `RemoteGateState::apply_reconnect_offset` (Remote), which differ enough
    /// between the two tracks that there's nothing to share there.
    fn note_reconnect_failure(track_kind: TrackKind, join_err: Option<&JoinError>, status_sink: &Option<Arc<Mutex<TranscriptionStatus>>>, reconnect: &mut ReconnectState) {
        if let Some(join_err) = join_err {
            tracing::warn!(%join_err, ?track_kind, "live transcription: reconnect task panicked or was cancelled");
            set_status(status_sink, track_kind, TrackTranscriptionStatus::Error(join_err.to_string()));
        }
        reconnect.note_disconnect();
    }

    /// Runs for the lifetime of one recording session, ending when `audio_rx` closes
    /// (i.e. `windows_session::run_capture_blocking`'s collector thread — and the
    /// `stt_tx` it owns — has finished, meaning capture is fully done). Failing to
    /// obtain a Deepgram session at all (no credential configured, auth failure,
    /// connect failure) is logged and simply means no live transcription for that
    /// track — same "failure doesn't take down the whole pipeline" spirit as
    /// `upload_worker`'s retry handling; the batch `run_pipeline` recording itself is
    /// unaffected either way. `status_sink`, if given, is kept up to date with each
    /// track's connection state for #52's UI visibility — `None` is fine, it's just
    /// a side channel, mirroring `level_sink`/`stt_sink` elsewhere in this crate.
    ///
    /// `silence_gate_enabled`, if `true`, applies a local RMS VAD ([`SilenceGate`])
    /// to the **Remote track only** (v1 scope — the Self track always streams
    /// unconditionally, regardless of this flag) so silent stretches aren't sent
    /// to the (metered) STT provider at all; see this crate's `silence_gate` and
    /// `timestamp_mapper` modules for the mechanism and the wall-clock timestamp
    /// correction it requires. `false` preserves this function's pre-gate
    /// behavior exactly (unconditional send on both tracks, no keepalive timer,
    /// no timestamp correction).
    ///
    /// A **mid-session disconnect** (task #82: an `SttEvent::Error`, a
    /// `send_audio`/`keep_alive` call returning `Err`, or a track's events
    /// channel closing unexpectedly — e.g. AssemblyAI's 3-hour hard cap, see
    /// `stt_assemblyai::AssemblyAISession::keep_alive`'s doc comment) is
    /// distinct from a failed *initial* connect above: the dead session is
    /// dropped immediately (so nothing keeps calling `send_audio`/`keep_alive`
    /// against it — that used to be this task's core bug), and if
    /// `SttError::is_retryable()` says the cause is transient, a fresh
    /// `provider.start_session()` is retried with backoff (see
    /// `reconnect_backoff`/`ReconnectState`) without blocking this function's
    /// main `select!` loop. A non-retryable cause (e.g.
    /// `AuthenticationFailed`) behaves like today: `Error` status, no more
    /// attempts for the rest of the recording. **Known caveat**: a successful
    /// Remote reconnect corrects persisted timestamps across the gap (see
    /// `RemoteGateState::apply_reconnect_offset`), but the Self track has no
    /// `TimestampMapper` (it never silence-gates, so it never needed one before
    /// this task) — a Self reconnect is logged, but that track's persisted
    /// timestamps after the reconnect point restart from the new session's own
    /// clock and are not corrected back to wall-clock. Self-track reconnects
    /// are expected to be rare (unlike Remote, nothing routinely disconnects
    /// it), so this is treated as an acceptable known limitation rather than
    /// justifying a second `TimestampMapper` instance for a track that has no
    /// other use for one.
    ///
    /// Task #87: the reconnect *attempt* itself (bounded by
    /// `START_SESSION_TIMEOUT`, same as an initial connect) also no longer
    /// blocks this `select!` loop — it runs as a `tokio::spawn`ed
    /// [`attempt_reconnect`] task, and a dedicated `select!` arm per track
    /// awaits its `JoinHandle` alongside every other branch. A prior version
    /// awaited it synchronously right inside the backoff-timer arm that fired
    /// it, which reintroduced the same class of bug task #82 otherwise fixed:
    /// while one track's reconnect attempt was in flight, `audio_rx`, the other
    /// track's events, and the keepalive timer all went unpolled for up to
    /// `START_SESSION_TIMEOUT`, and — concretely, with both tracks disconnected
    /// at once (a realistic scenario, e.g. a shared network blip) — the other
    /// track's own disconnect detection and reconnect were delayed by however
    /// long the first one's attempt took.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_live_transcription(
        session_id: SessionId,
        sample_rate_hz: u32,
        credential_store: Option<Arc<dyn CredentialStore + Send + Sync>>,
        mut audio_rx: Receiver<(TrackKind, Vec<f32>, u32)>,
        store: &SessionStore,
        status_sink: Option<Arc<Mutex<TranscriptionStatus>>>,
        silence_gate_enabled: bool,
        broker: Option<&LocalBroker>,
    ) {
        // Task #90: the clock every `transcription_gaps` row's `start_ms`/
        // `end_ms` is measured against (see `ms_since_start`) — captured as
        // close to this function's start as possible, since that's as close
        // as this module gets to "when the recording's audio timeline began"
        // (see this function's own doc comment: it runs for the recording's
        // full lifetime, fed by the same side channel capture is fed from).
        let recording_started = Instant::now();

        // Monotonic counters for generating stable segment_ids per track.
        // Incremented on each `is_final: true` segment; interim updates reuse
        // the same segment_id with an incremented revision.
        let mut self_segment_counter: u64 = 0;
        let mut remote_segment_counter: u64 = 0;

        let Some(credential_store) = credential_store else {
            tracing::debug!("live transcription: no credential store configured, skipping");
            set_both_status(&status_sink, TrackTranscriptionStatus::NotConfigured);
            drain(&mut audio_rx).await;
            return;
        };

        let selected_kind = credential_store
            .load(CREDENTIAL_SERVICE, SELECTED_STT_PROVIDER_ACCOUNT)
            .ok()
            .and_then(|value| SttProviderKind::from_account_value(&value))
            .unwrap_or_default();

        let provider = match build_stt_provider(selected_kind, credential_store.as_ref()) {
            Ok(provider) => provider,
            Err(err) => {
                tracing::info!(%err, ?selected_kind, "live transcription: no STT provider available, skipping");
                set_both_status(&status_sink, TrackTranscriptionStatus::NotConfigured);
                drain(&mut audio_rx).await;
                return;
            }
        };
        // `Arc` rather than `Box` (task #87): a mid-session reconnect attempt
        // now runs as a `tokio::spawn`ed task (see `attempt_reconnect`), which
        // needs to own the provider it connects through rather than borrow it
        // from this function's stack frame — an `Arc` clone per spawned attempt
        // is cheap, and `SttProvider: Send + Sync` (see `stt-api`) already makes
        // `dyn SttProvider` shareable across tasks like this.
        let provider: Arc<dyn SttProvider> = Arc::from(provider);
        // Phase 1A capture is a fixed format (design.md; see
        // `windows_frame_collector`'s "falls back to 48kHz mono" comment) — the
        // session is opened once, up front, at the target rate rather than
        // per-chunk, since a provider's session config (like every provider's) is
        // fixed for the connection's lifetime. `target_rate_hz` may differ from
        // `sample_rate_hz` (the capture pipeline's own rate) for providers that
        // require a fixed rate (see `target_sample_rate_hz`); each incoming chunk is
        // resampled to it below, right before `send_audio`.
        // Diarization is additional info within a single track (e.g. multiple people
        // on the Remote track in a group call) — the app's primary speaker split is
        // still Self/Remote (see this module's doc comment), not a provider's own
        // `speaker` index alone.
        let target_rate_hz = target_sample_rate_hz(selected_kind, sample_rate_hz);
        // Known side effect of `silence_gate_enabled` (Remote track only): once
        // silence is skipped locally, the provider itself sees almost no silence
        // in its input stream, so its own `vad_events(true)`-subscribed VAD
        // (`SttEvent::SpeechStarted`/`SpeechEnded`) becomes largely meaningless —
        // it will rarely, if ever, fire, since from the provider's perspective the
        // stream is "always speaking". This is especially notable for OpenAI:
        // that adapter's `vad_events(true)` maps directly to
        // `turn_detection: server_vad`, which the provider also uses to decide
        // where to *segment* transcripts — so silence removal can shift OpenAI's
        // segmentation behavior, not just suppress the VAD events themselves.
        let config = SttSessionConfig::new(target_rate_hz).with_interim_results(true).with_vad_events(true).with_diarization(true);

        set_both_status(&status_sink, TrackTranscriptionStatus::Connecting);

        // Self and Remote connect concurrently, for the same reason `finalize`
        // below does (task #81's doc comment): sequential awaits would stack up to
        // two `START_SESSION_TIMEOUT`s back to back on a slow/black-holed network,
        // doubling how long the `audio_rx.recv()` loop below is delayed from
        // starting — and every chunk that arrives on `stt_tx` before that loop
        // starts draining it just sits in the bounded (task #86) channel, which
        // means a slow-but-not-timed-out connection could burn through that
        // channel's buffering and start silently dropping audio before live
        // transcription even begins.
        // `config` is cloned for both, rather than moved into the second call,
        // since a mid-session reconnect (task #82) needs to open a fresh session
        // with this same config again, arbitrarily many times, for the rest of
        // this function's lifetime.
        let (self_sess, remote_sess) = tokio::join!(
            start_session_with_timeout(provider.as_ref(), config.clone(), "self", TrackKind::SelfMic, &status_sink),
            start_session_with_timeout(provider.as_ref(), config.clone(), "remote", TrackKind::RemoteAudio, &status_sink)
        );

        if self_sess.is_none() && remote_sess.is_none() {
            drain(&mut audio_rx).await;
            return;
        }

        let (mut self_session, mut self_events) = self_sess.unzip();
        let (remote_session, mut remote_events) = remote_sess.unzip();

        let mut self_samples_sent: u64 = 0;
        let mut self_reconnect = ReconnectState::new();
        let mut audio_open = true;

        // Bundles every piece of Remote-track-only state the VAD gate, keepalive
        // timer, timestamp correction, and reconnect backoff (task #82) share
        // (v1 scope, see this function's doc comment) — see `RemoteGateState`'s
        // own doc comment for why this replaced half a dozen loose `remote_*`
        // locals that always changed together.
        let mut remote = RemoteGateState::new(remote_session, silence_gate_enabled, target_rate_hz, status_sink.clone(), store, session_id, recording_started);
        let mut remote_keepalive_timer = tokio::time::interval(Duration::from_secs(1));

        // #56: the last still-unconfirmed `PartialTranscript` seen per track, cleared
        // every time a `FinalTranscript` supersedes it. If a track's event channel
        // closes (Deepgram disconnected) while one of these is still set, no final
        // ever arrived for that stretch of speech and it would otherwise vanish
        // entirely from `transcript::to_turns`' summary input (which only reads
        // `is_final` rows) — see this function's tail below.
        let mut self_last_interim: Option<PendingInterim> = None;
        let mut remote_last_interim: Option<PendingInterim> = None;

        while audio_open || self_events.is_some() || remote_events.is_some() {
            tokio::select! {
                maybe = audio_rx.recv(), if audio_open => {
                    match maybe {
                        Some((track, samples, chunk_rate_hz)) => {
                            match track {
                                TrackKind::SelfMic => {
                                    // Self is always sent unconditionally, regardless of
                                    // `silence_gate_enabled` — the gate is Remote-only
                                    // (v1 scope; see this function's doc comment).
                                    if let Some(session) = self_session.as_mut() {
                                        // `AudioChunk::start_sample` is documented as "at
                                        // `SttSessionConfig::sample_rate_hz`" — i.e. the
                                        // *target* rate's sample count, so resample happens
                                        // before the counter advances, not after.
                                        let resampled = resample(&samples, chunk_rate_hz, target_rate_hz);
                                        let chunk = AudioChunk { pcm: &resampled, start_sample: self_samples_sent };
                                        self_samples_sent += resampled.len() as u64;
                                        if let Err(err) = session.send_audio(chunk).await {
                                            tracing::warn!(%err, ?track, "live transcription: send_audio failed");
                                            note_self_disconnect(&mut self_session, &mut self_reconnect, &status_sink, Some(&err), store, session_id, recording_started);
                                        }
                                    }
                                }
                                TrackKind::RemoteAudio if !remote.is_active() => {
                                    // No Remote session at all (provider never connected, or
                                    // already finalized) — skip resample/gate/RMS work
                                    // entirely rather than paying for it on every chunk for
                                    // the rest of the recording with nowhere to send the
                                    // result (code-review finding: this used to run
                                    // unconditionally whenever the gate was enabled).
                                }
                                TrackKind::RemoteAudio => {
                                    let resampled = resample(&samples, chunk_rate_hz, target_rate_hz);
                                    remote.handle_chunk(&resampled).await;
                                }
                            }
                        }
                        None => {
                            audio_open = false;
                            // Any reconnect attempt still in flight has nothing
                            // left to serve now that there's no more audio to
                            // send it — abort it instead of leaking a detached
                            // background task past this function's return (task
                            // #87; see `ReconnectState::abort_in_flight`'s doc
                            // comment).
                            self_reconnect.abort_in_flight();
                            remote.reconnect.abort_in_flight();
                            // Task #90: the recording is ending, so any gap
                            // still open on either track (a disconnect that
                            // never reconnected, or a reconnect attempt just
                            // aborted above) closes here, with the recording's
                            // end time as `end_ms` — no later event will ever
                            // close it otherwise (see `close_open_gap`'s doc
                            // comment).
                            close_open_gap(store, session_id, recording_started, TrackKind::SelfMic, &mut self_reconnect);
                            close_open_gap(store, session_id, recording_started, TrackKind::RemoteAudio, &mut remote.reconnect);
                            // Self and Remote finalize concurrently (task #81):
                            // sequential awaits would stack up to two
                            // `FINALIZE_TIMEOUT`s back to back on a black-holed
                            // network, doubling how long recording stop blocks the
                            // audio-save/upload path behind this function (see
                            // `windows_session`'s `tokio::join!(capture_fut,
                            // live_transcription_fut)`).
                            let self_finalize = async {
                                if let Some(session) = self_session.take() {
                                    finalize_with_timeout(session, "self").await;
                                }
                            };
                            tokio::join!(self_finalize, remote.finalize());
                            // Publish UtteranceEnded(SessionEnd) so consumers
                            // (e.g. Summary Consumer) can react to session end.
                            if let Some(broker) = broker {
                                let event = TranscriptEvent::UtteranceEnded {
                                    session_id,
                                    segment_id: None,
                                    reason: UtteranceEndReason::SessionEnd,
                                };
                                let subject = transcript_event::subject_for(&event, session_id);
                                let envelope = EventEnvelope::new(session_id, event);
                                if let Err(err) = broker.publish(&subject, &envelope) {
                                    tracing::warn!(%err, "live transcription: failed to publish UtteranceEnded(SessionEnd)");
                                }
                            }
                        }
                    }
                }
                _ = remote_keepalive_timer.tick(), if remote.is_active() && remote.gate_enabled() => {
                    remote.keep_alive_if_idle().await;
                }
                // Reconnect timers (task #82): armed only while `audio_open` —
                // once recording has ended there's no more audio to send, so a
                // reconnect that fired after that point would open a session
                // this function would never finalize (the `audio_rx.recv()`
                // arm's `None` branch above, which drives `finalize`, only ever
                // runs once). `Instant::now()` as the disabled-branch fallback
                // is never actually awaited — see `recv_track_event`'s doc
                // comment on why a disabled `select!` branch's expression must
                // still evaluate without panicking. `in_flight.is_none()`
                // guards against arming a second attempt while one is already
                // running (task #87) — `retry_at`/`in_flight` are otherwise
                // mutually exclusive (see `ReconnectState::in_flight`'s doc
                // comment), so this is defensive rather than expected to ever
                // matter, but cheap to keep true regardless.
                //
                // Firing this arm only *spawns* the attempt (task #87) —
                // completion is awaited by the dedicated arms below, so this
                // returns to polling `audio_rx`/the other track/the keepalive
                // timer immediately rather than blocking on it.
                _ = tokio::time::sleep_until(self_reconnect.retry_at.unwrap_or_else(Instant::now)), if audio_open && self_reconnect.retry_at.is_some() && self_reconnect.in_flight.is_none() => {
                    self_reconnect.retry_at = None;
                    self_reconnect.in_flight = Some(tokio::spawn(attempt_reconnect(provider.clone(), config.clone(), "self", TrackKind::SelfMic, status_sink.clone())));
                }
                _ = tokio::time::sleep_until(remote.reconnect.retry_at.unwrap_or_else(Instant::now)), if audio_open && remote.reconnect.retry_at.is_some() && remote.reconnect.in_flight.is_none() => {
                    remote.reconnect.retry_at = None;
                    remote.reconnect.in_flight = Some(tokio::spawn(attempt_reconnect(provider.clone(), config.clone(), "remote", TrackKind::RemoteAudio, status_sink.clone())));
                }
                // Reconnect completions (task #87): awaits whichever spawned
                // `attempt_reconnect` task the arms above started, without
                // blocking any other branch of this loop while it's in flight.
                result = recv_reconnect_result(&mut self_reconnect.in_flight), if self_reconnect.in_flight.is_some() => {
                    self_reconnect.in_flight = None;
                    match result {
                        Ok(Some((session, events))) => {
                            self_session = Some(session);
                            self_events = Some(events);
                            self_samples_sent = 0;
                            // Task #90: the outage this track's `open_gap` opened
                            // is over now that a fresh session is in hand — close
                            // it before `reset()` below, which deliberately leaves
                            // `open_gap_id`/`open_gap_start_ms` untouched (see
                            // `ReconnectState::reset`'s doc comment).
                            close_open_gap(store, session_id, recording_started, TrackKind::SelfMic, &mut self_reconnect);
                            // `attempt_reconnect` no longer touches `reconnect`
                            // itself (see its doc comment) — the Self track has
                            // no `apply_reconnect_offset` counterpart that needs
                            // `disconnected_at` afterward, so it's safe to reset
                            // right here.
                            self_reconnect.reset();
                            tracing::warn!(track = "self", "live transcription: reconnected after disconnect; this track has no TimestampMapper, so persisted timestamps for it restart from the new session's clock (see run_live_transcription's doc comment)");
                        }
                        Ok(None) => note_reconnect_failure(TrackKind::SelfMic, None, &status_sink, &mut self_reconnect),
                        Err(join_err) => note_reconnect_failure(TrackKind::SelfMic, Some(&join_err), &status_sink, &mut self_reconnect),
                    }
                }
                result = recv_reconnect_result(&mut remote.reconnect.in_flight), if remote.reconnect.in_flight.is_some() => {
                    remote.reconnect.in_flight = None;
                    match result {
                        Ok(Some((session, events))) => {
                            remote.session = Some(session);
                            remote_events = Some(events);
                            remote.apply_reconnect_offset(target_rate_hz);
                        }
                        Ok(None) => note_reconnect_failure(TrackKind::RemoteAudio, None, &status_sink, &mut remote.reconnect),
                        Err(join_err) => note_reconnect_failure(TrackKind::RemoteAudio, Some(&join_err), &status_sink, &mut remote.reconnect),
                    }
                }
                maybe = recv_track_event(&mut self_events) => {
                    match maybe {
                        Some(event) => {
                            note_event(&mut self_last_interim, &event);
                            if let SttEvent::Error(err) = &event {
                                // Guarded by `self_session.is_some()` for the same
                                // reason the channel-close arm below is (task #90):
                                // once a disconnect has already been noted for this
                                // outage, `self_session` is `None` for its whole
                                // duration (including a permanent give-up, which
                                // leaves `self_events` pointed at the now-dead
                                // channel for the rest of the recording — see
                                // `is_disconnect_retryable`'s doc comment). Without
                                // this guard, a provider that emits more than one
                                // `SttEvent::Error` for the same underlying failure
                                // would re-enter `note_disconnect_and_maybe_reconnect`
                                // after `reset()` already cleared `retry_at`/
                                // `in_flight`, which reopens a second
                                // `transcription_gaps` row for an outage that was
                                // never actually two outages.
                                if self_session.is_some() {
                                    note_self_disconnect(&mut self_session, &mut self_reconnect, &status_sink, Some(err), store, session_id, recording_started);
                                }
                            }
                            persist_event(store, broker, session_id, TrackKind::SelfMic, event, None, &mut self_segment_counter);
                        }
                        None => {
                            self_events = None;
                            persist_pending_interim(store, session_id, TrackKind::SelfMic, self_last_interim.take(), None);
                            if self_session.is_some() {
                                // The channel closed with `self_session` still
                                // set, i.e. without us having cleared it via a
                                // prior `SttEvent::Error`/`send_audio` failure or
                                // `finalize` — an unexpected provider-side close
                                // (task #82; e.g. AssemblyAI's 3-hour hard cap,
                                // which has no `SttEvent::Error` at all).
                                tracing::warn!(track = "self", "live transcription: events channel closed unexpectedly");
                                note_self_disconnect(&mut self_session, &mut self_reconnect, &status_sink, None, store, session_id, recording_started);
                            }
                        }
                    }
                }
                maybe = recv_track_event(&mut remote_events) => {
                    match maybe {
                        Some(event) => {
                            note_event(&mut remote_last_interim, &event);
                            if let SttEvent::Error(err) = &event {
                                // See the Self-track arm above for why this guard matters (task #90).
                                if remote.is_active() {
                                    remote.note_disconnect(Some(err));
                                }
                            }
                            persist_event(store, broker, session_id, TrackKind::RemoteAudio, event, remote.timestamp_mapper(), &mut remote_segment_counter);
                        }
                        None => {
                            remote_events = None;
                            persist_pending_interim(store, session_id, TrackKind::RemoteAudio, remote_last_interim.take(), remote.timestamp_mapper());
                            if remote.is_active() {
                                // See the Self-track arm above for why this guard matters.
                                tracing::warn!(track = "remote", "live transcription: events channel closed unexpectedly");
                                remote.note_disconnect(None);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Bundles every piece of Remote-track-only state the VAD gate
    /// (`silence_gate_enabled`, v1 scope — see [`run_live_transcription`]'s doc
    /// comment), the idle-keepalive timer, and timestamp correction share, so a
    /// `select!` branch that needs any of it doesn't have to thread half a dozen
    /// loose `remote_*` locals through by hand. Consolidating them here is also what
    /// fixed a real bug caught in code review: `total_injected` used to be a
    /// separate argument callers could (and once did) forget to fold into a
    /// checkpoint's key.
    struct RemoteGateState<'a> {
        session: Option<Box<dyn SttSession>>,
        /// Only `Some` when `silence_gate_enabled` — see `new`.
        gate: Option<SilenceGate>,
        mapper: Option<TimestampMapper>,
        /// Samples actually sent to the provider via `send_audio`, i.e. what
        /// `AudioChunk::start_sample` needs — does *not* include keepalive-injected
        /// samples (see `total_injected`).
        samples_sent: u64,
        /// Cumulative samples sent to the provider via `keep_alive`'s
        /// `KeepAliveEffect::InjectedAudio` (Google/OpenAI), which bypass
        /// `samples_sent` entirely — that counter only tracks bytes sent through
        /// `send_audio`, but injected heartbeat audio reaches the provider through
        /// each adapter's own internal channel. The provider's *own* audio-duration
        /// clock (what its `audio_start_ms`/`audio_end_ms` are computed from)
        /// advances on both, so every `TimestampMapper` checkpoint is keyed on
        /// `samples_sent + total_injected`, not `samples_sent` alone — using the
        /// latter would under-count the provider's real position by however much
        /// has been injected so far, corrupting `TimestampMapper::to_wallclock_ms`'s
        /// binary search against later checkpoints.
        total_injected: u64,
        /// `captured_samples_dropped - artificial_samples_injected` so far (see
        /// `TimestampMapper`'s doc comment) — advances on every dropped span and
        /// every keepalive-injected heartbeat.
        net_offset: i64,
        /// Last time real audio (`Send`/`SendStitched`) or a keepalive reached the
        /// provider, for `keep_alive_if_idle`. Only consulted while `gate.is_some()`
        /// and `session.is_some()`, so its initial value is otherwise irrelevant.
        last_active: Instant,
        /// Reconnect backoff state (task #82) — see `ReconnectState`'s doc
        /// comment.
        reconnect: ReconnectState,
        /// Cloned once at construction so `note_disconnect` can report status
        /// without every caller threading it through — same shape as
        /// `RemoteGateState` bundling everything else this track's `select!`
        /// branches need (see this struct's doc comment).
        status_sink: Option<Arc<Mutex<TranscriptionStatus>>>,
        /// `SessionStore`/`SessionId`/recording-start clock needed to open and
        /// close `transcription_gaps` rows (task #90) from `note_disconnect`/
        /// `apply_reconnect_offset` — bundled here for the same reason as
        /// everything else on this struct (see its own doc comment): those
        /// two methods would otherwise need all three threaded through as
        /// parameters on every call.
        store: &'a SessionStore,
        session_id: SessionId,
        recording_started: Instant,
    }

    impl<'a> RemoteGateState<'a> {
        fn new(
            session: Option<Box<dyn SttSession>>,
            silence_gate_enabled: bool,
            target_rate_hz: u32,
            status_sink: Option<Arc<Mutex<TranscriptionStatus>>>,
            store: &'a SessionStore,
            session_id: SessionId,
            recording_started: Instant,
        ) -> Self {
            Self {
                session,
                gate: silence_gate_enabled.then(|| SilenceGate::new(GateConfig { sample_rate_hz: target_rate_hz, ..GateConfig::default() })),
                mapper: silence_gate_enabled.then(|| TimestampMapper::new(target_rate_hz)),
                samples_sent: 0,
                total_injected: 0,
                net_offset: 0,
                last_active: Instant::now(),
                reconnect: ReconnectState::new(),
                store,
                session_id,
                recording_started,
                status_sink,
            }
        }

        fn is_active(&self) -> bool {
            self.session.is_some()
        }

        /// Whether the VAD gate (and therefore the keepalive timer and timestamp
        /// correction) is enabled for this track — a stand-in for
        /// `silence_gate_enabled` that doesn't need that flag threaded separately,
        /// since `gate` is only ever constructed from it in `new` and never changes
        /// afterward.
        fn gate_enabled(&self) -> bool {
            self.gate.is_some()
        }

        fn timestamp_mapper(&self) -> Option<&TimestampMapper> {
            self.mapper.as_ref()
        }

        /// Routes one incoming chunk (already resampled to the provider's target
        /// rate) through the gate if enabled, or sends it unconditionally
        /// otherwise — matching this function's pre-gate behavior exactly when
        /// `gate` is `None`. A no-op if there's no session to send to; callers
        /// should check `is_active` first so a session-less track doesn't pay for
        /// `resample`ing input this would just discard (see the `select!` loop's
        /// `TrackKind::RemoteAudio if !remote.is_active()` arm).
        async fn handle_chunk(&mut self, resampled: &[f32]) {
            let Some(gate) = self.gate.as_mut() else {
                self.send_unconditional(resampled).await;
                return;
            };
            // Gate enabled: send only the spans `SilenceGate` judges worth paying
            // for, and record a `TimestampMapper` checkpoint for every action so
            // `persist_event`/`persist_pending_interim` can correct provider
            // timestamps back to wall-clock.
            for action in gate.process(resampled) {
                match action {
                    GateAction::Send(pcm) => self.send_gated(pcm).await,
                    GateAction::SendStitched(pcm) => self.send_gated(&pcm).await,
                    GateAction::Drop { sample_count } => self.record_drop(sample_count),
                }
            }
        }

        /// Sends `pcm` unconditionally (a no-op if there's no session) and advances
        /// `samples_sent` — the shared core of both the gate-disabled fallback path
        /// and `send_gated` below. Does not touch `mapper`/`net_offset`/
        /// `last_active`; callers that need those (i.e. `send_gated`) add them on
        /// top.
        async fn send_unconditional(&mut self, pcm: &[f32]) {
            let Some(session) = self.session.as_mut() else { return };
            let chunk = AudioChunk { pcm, start_sample: self.samples_sent };
            self.samples_sent += pcm.len() as u64;
            if let Err(err) = session.send_audio(chunk).await {
                tracing::warn!(%err, track = ?TrackKind::RemoteAudio, "live transcription: send_audio failed");
                self.note_disconnect(Some(&err));
            }
        }

        /// Sends one gated span (`GateAction::Send`/`SendStitched`), records a
        /// `TimestampMapper` checkpoint at the new provider-clock position (net
        /// offset unchanged by a send — only `record_drop` and keepalive
        /// heartbeats move it), and refreshes `last_active` so the idle-keepalive
        /// timer doesn't fire needlessly right after real audio went out.
        async fn send_gated(&mut self, pcm: &[f32]) {
            self.send_unconditional(pcm).await;
            if let Some(mapper) = self.mapper.as_mut() {
                mapper.record_checkpoint(self.samples_sent + self.total_injected, self.net_offset);
            }
            self.last_active = Instant::now();
        }

        /// Accounts for a `GateAction::Drop`: `sample_count` samples were judged
        /// silence and never sent, so `net_offset` grows by that much and a
        /// checkpoint is recorded at the (unchanged) provider-clock position.
        fn record_drop(&mut self, sample_count: u64) {
            self.net_offset += sample_count as i64;
            if let Some(mapper) = self.mapper.as_mut() {
                mapper.record_checkpoint(self.samples_sent + self.total_injected, self.net_offset);
            }
        }

        /// Calls `SttSession::keep_alive` if the Remote track has gone
        /// `REMOTE_KEEPALIVE_IDLE_THRESHOLD` without any real audio, so the
        /// provider's idle-connection timeout doesn't fire during a long
        /// silence-gated gap. A no-op unless the gate is enabled and there's a
        /// session to keep alive — callers should still guard the `select!`
        /// branch itself on `is_active() && gate_enabled()` to avoid polling this
        /// needlessly once neither holds.
        async fn keep_alive_if_idle(&mut self) {
            if !self.gate_enabled() || self.last_active.elapsed() < REMOTE_KEEPALIVE_IDLE_THRESHOLD {
                return;
            }
            let Some(session) = self.session.as_mut() else { return };
            match session.keep_alive().await {
                Ok(KeepAliveEffect::InjectedAudio { samples }) => {
                    self.net_offset -= samples as i64;
                    // Advance the provider-clock counter too (see `total_injected`'s
                    // doc comment) — this heartbeat reached the provider, so it
                    // counts toward the position later checkpoints and lookups must
                    // agree on.
                    self.total_injected += samples;
                    if let Some(mapper) = self.mapper.as_mut() {
                        mapper.record_checkpoint(self.samples_sent + self.total_injected, self.net_offset);
                    }
                }
                Ok(KeepAliveEffect::ControlMessage | KeepAliveEffect::Noop) => {}
                Err(err) => {
                    tracing::warn!(%err, track = "remote", "live transcription: keep_alive failed");
                    self.note_disconnect(Some(&err));
                    return;
                }
            }
            self.last_active = Instant::now();
        }

        async fn finalize(&mut self) {
            if let Some(session) = self.session.take() {
                finalize_with_timeout(session, "remote").await;
            }
        }

        /// Marks the Remote session as disconnected (task #82: any of
        /// `SttEvent::Error`, `send_audio`/`keep_alive` returning `Err`, or the
        /// events channel closing — see `is_disconnect_retryable`'s doc comment)
        /// — drops `session` immediately so nothing keeps calling
        /// `send_audio`/`keep_alive` against a dead session (that used to be
        /// this task's core bug: those calls kept failing with `SessionClosed`
        /// for the rest of the recording), and arms a reconnect via
        /// `note_disconnect_and_maybe_reconnect` if `err` is retryable (or
        /// `None`, the events-channel-closed case).
        fn note_disconnect(&mut self, err: Option<&SttError>) {
            self.session = None;
            note_disconnect_and_maybe_reconnect(self.store, self.session_id, self.recording_started, TrackKind::RemoteAudio, err, &self.status_sink, &mut self.reconnect);
        }

        /// Applied once a Remote reconnect succeeds, before any audio flows on
        /// the new session: preserves timestamp continuity across the outage by
        /// giving the fresh (empty) `TimestampMapper` a single checkpoint at
        /// local position 0 whose offset already accounts for (a) how far the
        /// old session's provider clock had reached
        /// (`samples_sent + total_injected`) plus whatever `net_offset` already
        /// applied there, and (b) the outage itself — every incoming Remote
        /// chunk was dropped outright while `session` was `None` (see the
        /// `select!` loop's `TrackKind::RemoteAudio if !remote.is_active()` arm),
        /// so that real time is *not* reflected in either of the old session's
        /// counters and has to be estimated from wall-clock elapsed since
        /// `reconnect.disconnected_at` instead.
        ///
        /// A *fresh* mapper (rather than continuing to append to the old one) is
        /// required, not just convenient: `TimestampMapper::record_checkpoint`
        /// requires a non-decreasing position across calls, but the new
        /// session's own clock (what live `SttEvent::audio_start_ms`/
        /// `audio_end_ms` are relative to, and therefore what future checkpoints
        /// must be keyed on — see `send_gated`/`record_drop`) restarts at 0,
        /// which is smaller than whatever position the old session's checkpoints
        /// ended at.
        ///
        /// Must run — and read `self.reconnect.disconnected_at` — before
        /// `self.reconnect` is reset back to its healthy baseline, which is why
        /// this is what resets it (via `ReconnectState::reset` at the end,
        /// rather than `attempt_reconnect` itself doing so on success): the
        /// caller only invokes this once it already has the new session in
        /// hand, right after `attempt_reconnect` returns, so `disconnected_at`
        /// is still whatever `note_disconnect` originally recorded for this
        /// outage.
        ///
        /// Task #90: also closes whichever `transcription_gaps` row
        /// `note_disconnect` opened for this outage, via `close_open_gap` —
        /// like the timestamp correction above, this must run before
        /// `self.reconnect.reset()` at the end (which does *not* clear
        /// `open_gap_id`/`open_gap_start_ms` itself — see `ReconnectState::reset`'s
        /// doc comment — so something has to).
        fn apply_reconnect_offset(&mut self, target_rate_hz: u32) {
            close_open_gap(self.store, self.session_id, self.recording_started, TrackKind::RemoteAudio, &mut self.reconnect);
            if self.mapper.is_some() {
                let outage_samples = self
                    .reconnect
                    .disconnected_at
                    .map(|at| (at.elapsed().as_secs_f64() * target_rate_hz as f64).round() as u64)
                    .unwrap_or(0);
                let carry_over_samples = (self.samples_sent + self.total_injected) as i64 + self.net_offset + outage_samples as i64;
                let mut fresh_mapper = TimestampMapper::new(target_rate_hz);
                fresh_mapper.record_checkpoint(0, carry_over_samples);
                self.mapper = Some(fresh_mapper);
                self.net_offset = carry_over_samples;
            }
            self.samples_sent = 0;
            self.total_injected = 0;
            self.last_active = Instant::now();
            self.reconnect.reset();
        }
    }

    /// Bounds `session.finalize()` by [`FINALIZE_TIMEOUT`] (task #81) so a
    /// black-holed connection's server-side drain can't hang recording stop
    /// indefinitely. A timeout is logged and otherwise treated like any other
    /// finalize failure — the audio-save/upload path this runs ahead of
    /// (`windows_session`'s `tokio::join!`) must not be blocked by it.
    async fn finalize_with_timeout(session: Box<dyn SttSession>, track: &'static str) {
        match tokio::time::timeout(FINALIZE_TIMEOUT, session.finalize()).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(%err, track, "live transcription: failed to finalize STT session"),
            Err(_) => tracing::warn!(track, timeout_secs = FINALIZE_TIMEOUT.as_secs(), "live transcription: finalize timed out"),
        }
    }

    /// Awaits the next event for a track whose events channel may already be
    /// `None` (that track's session never started, or already closed). Used
    /// instead of `events.as_mut().unwrap().recv()` behind a `select!` `if`
    /// guard: `tokio::select!` still evaluates a disabled branch's async
    /// expression (just doesn't poll it — see the `select!` macro docs), so an
    /// `unwrap()` there would panic as soon as the *other* track's branch stays
    /// enabled while this one is `None`. Awaiting `pending()` for the `None` case
    /// instead means the branch simply never becomes ready, with no unwrap.
    async fn recv_track_event(events: &mut Option<UnboundedReceiver<SttEvent>>) -> Option<SttEvent> {
        match events {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// The bits of a `PartialTranscript` needed to persist it as a fallback final
    /// (#56) — just what `TranscriptSegment` needs, not the full `SttEvent`.
    struct PendingInterim {
        text: String,
        audio_start_ms: Option<u64>,
        audio_end_ms: Option<u64>,
    }

    /// Tracks the most recent not-yet-finalized `PartialTranscript` per track:
    /// records it on every partial, clears it on every final (a final always
    /// supersedes the partials leading up to it).
    fn note_event(last_interim: &mut Option<PendingInterim>, event: &SttEvent) {
        match event {
            SttEvent::PartialTranscript { text, audio_start_ms, audio_end_ms, .. } => {
                *last_interim = Some(PendingInterim { text: text.clone(), audio_start_ms: *audio_start_ms, audio_end_ms: *audio_end_ms });
            }
            SttEvent::FinalTranscript { .. } => *last_interim = None,
            _ => {}
        }
    }

    /// #56's fallback: called once a track's event channel has closed, with
    /// whatever `PendingInterim` was still outstanding (`None` if the last partial
    /// was already superseded by a final, or there was none). Persists it as
    /// `is_final: true` so it isn't silently dropped by `transcript::to_turns`.
    /// Loses word-level diarization (`speaker` stays `None`) since
    /// `PartialTranscript` never carries `words` — better than losing the
    /// utterance outright.
    ///
    /// `timestamp_mapper`, if given, corrects `pending.audio_start_ms`/
    /// `audio_end_ms` to wall-clock milliseconds before persisting (see
    /// `TimestampMapper`'s doc comment) — `start`/`end` are corrected
    /// independently since a segment spanning a silence-gated gap needs each end
    /// mapped against its own applicable checkpoint. Pass `None` for the Self
    /// track, or whenever `silence_gate_enabled` is `false`, to persist the raw
    /// provider-relative values unchanged, exactly as before this parameter
    /// existed.
    fn persist_pending_interim(
        store: &SessionStore,
        session_id: SessionId,
        track: TrackKind,
        pending: Option<PendingInterim>,
        timestamp_mapper: Option<&TimestampMapper>,
    ) {
        let Some(pending) = pending else { return };
        if pending.text.trim().is_empty() {
            return;
        }
        tracing::info!(?track, "live transcription: finalizing last interim transcript after channel close");
        let segment = TranscriptSegment {
            session_id,
            track: Some(track),
            speaker: None,
            text: pending.text,
            start_ms: corrected_ms(pending.audio_start_ms, timestamp_mapper),
            end_ms: corrected_ms(pending.audio_end_ms, timestamp_mapper),
            is_final: true,
            is_retranscribed: false,
        };
        if let Err(err) = store.insert_transcript_segment(&segment) {
            tracing::warn!(%err, ?track, "live transcription: failed to persist fallback-final transcript");
        }
    }

    /// See `persist_pending_interim`'s doc comment for `timestamp_mapper`'s
    /// contract (`None` for Self / gate-disabled, applied independently to
    /// `start`/`end` otherwise).
    fn persist_event(
        store: &SessionStore,
        broker: Option<&LocalBroker>,
        session_id: SessionId,
        track: TrackKind,
        event: SttEvent,
        timestamp_mapper: Option<&TimestampMapper>,
        segment_counter: &mut u64,
    ) {
        match event {
            SttEvent::PartialTranscript { text, audio_start_ms, audio_end_ms, .. } => {
                let start_ms = corrected_ms(audio_start_ms, timestamp_mapper);
                let end_ms = corrected_ms(audio_end_ms, timestamp_mapper);
                let segment = TranscriptSegment { session_id, track: Some(track), speaker: None, text: text.clone(), start_ms, end_ms, is_final: false, is_retranscribed: false };
                if let Err(err) = store.insert_transcript_segment(&segment) {
                    tracing::warn!(%err, ?track, "live transcription: failed to persist partial transcript");
                }
                publish_transcript_event(broker, session_id, track, &segment, Finality::Interim, *segment_counter);
            }
            SttEvent::FinalTranscript { text, words, audio_start_ms, audio_end_ms, .. } => {
                let speaker = words.as_ref().and_then(|words| words.first()).and_then(|word| word.speaker);
                let start_ms = corrected_ms(audio_start_ms, timestamp_mapper);
                let end_ms = corrected_ms(audio_end_ms, timestamp_mapper);
                let segment = TranscriptSegment { session_id, track: Some(track), speaker, text: text.clone(), start_ms, end_ms, is_final: true, is_retranscribed: false };
                if let Err(err) = store.insert_transcript_segment(&segment) {
                    tracing::warn!(%err, ?track, "live transcription: failed to persist final transcript");
                }
                publish_transcript_event(broker, session_id, track, &segment, Finality::Final, *segment_counter);
                *segment_counter += 1;
            }
            SttEvent::SpeechStarted => tracing::debug!(?track, "live transcription: speech started"),
            SttEvent::SpeechEnded => tracing::debug!(?track, "live transcription: speech ended"),
            SttEvent::Error(err) => tracing::warn!(?track, %err, "live transcription: STT error"),
        }
    }

    fn publish_transcript_event(
        broker: Option<&LocalBroker>,
        session_id: SessionId,
        track: TrackKind,
        segment: &TranscriptSegment,
        finality: Finality,
        counter: u64,
    ) {
        let broker = match broker {
            Some(b) => b,
            None => return,
        };
        let segment_id = transcript_event::segment_id_for(session_id, track, counter);
        let speaker_label = super::speaker_label(segment.track, segment.speaker);
        let data = transcript_event::SegmentData {
            segment_id: segment_id.clone(),
            revision: 0,
            text: segment.text.clone(),
            speaker_label,
            track,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
        };
        let updated = TranscriptEvent::SegmentUpdated {
            session_id,
            data: data.clone(),
            finality,
        };
        let subject = transcript_event::subject_for(&updated, session_id);
        let envelope = EventEnvelope::new(session_id, updated);
        if let Err(err) = broker.publish(&subject, &envelope) {
            tracing::warn!(%err, ?track, "live transcription: failed to publish SegmentUpdated");
        }
        if matches!(finality, Finality::Final) {
            let finalized = TranscriptEvent::SegmentFinalized {
                session_id,
                data,
            };
            let subject = transcript_event::subject_for(&finalized, session_id);
            let envelope = EventEnvelope::new(session_id, finalized);
            if let Err(err) = broker.publish(&subject, &envelope) {
                tracing::warn!(%err, ?track, "live transcription: failed to publish SegmentFinalized");
            }
        }
    }

    /// Applies `timestamp_mapper.to_wallclock_ms` to `ms` if a mapper was given,
    /// otherwise returns `ms` unchanged. Shared by `persist_event` and
    /// `persist_pending_interim` so `start`/`end` are always corrected via the
    /// same one-liner rather than four near-identical `match`es.
    fn corrected_ms(ms: Option<u64>, timestamp_mapper: Option<&TimestampMapper>) -> Option<u64> {
        match timestamp_mapper {
            Some(mapper) => ms.map(|ms| mapper.to_wallclock_ms(ms)),
            None => ms,
        }
    }

    /// Drains `audio_rx` to completion without doing anything with it — used when
    /// live transcription can't start at all (no credential store, no key, both
    /// sessions failed to open), so the sender side
    /// (`windows_frame_collector::collect_frames`) never blocks on a full channel.
    /// `collect_frames`'s `try_send` never blocks regardless (see its doc comment,
    /// task #86), but without this the channel would just sit full and every chunk
    /// after the first `stt_tx`-capacity's worth would silently log as dropped —
    /// keeping it drained instead means no live transcription still means no
    /// spurious "channel full" warnings.
    async fn drain(audio_rx: &mut Receiver<(TrackKind, Vec<f32>, u32)>) {
        while audio_rx.recv().await.is_some() {}
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn set_status_updates_only_the_named_track() {
            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            set_status(&Some(sink.clone()), TrackKind::SelfMic, TrackTranscriptionStatus::Connected);
            let status = sink.lock().unwrap().clone();
            assert_eq!(status.self_status, TrackTranscriptionStatus::Connected);
            assert_eq!(status.remote_status, TrackTranscriptionStatus::NotConfigured);
        }

        #[test]
        fn set_both_status_updates_both_tracks() {
            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            set_both_status(&Some(sink.clone()), TrackTranscriptionStatus::Connecting);
            let status = sink.lock().unwrap().clone();
            assert_eq!(status.self_status, TrackTranscriptionStatus::Connecting);
            assert_eq!(status.remote_status, TrackTranscriptionStatus::Connecting);
        }

        #[test]
        fn note_event_tracks_the_latest_partial_and_clears_on_final() {
            let mut last_interim = None;
            note_event(&mut last_interim, &SttEvent::PartialTranscript { text: "hel".to_string(), audio_start_ms: Some(0), audio_end_ms: Some(100), extra: Default::default() });
            assert!(last_interim.is_some());
            note_event(&mut last_interim, &SttEvent::FinalTranscript { text: "hello".to_string(), words: None, audio_start_ms: Some(0), audio_end_ms: Some(200), extra: Default::default() });
            assert!(last_interim.is_none());
        }

        #[test]
        fn corrected_ms_passes_through_unchanged_without_a_mapper() {
            assert_eq!(corrected_ms(Some(300), None), Some(300));
            assert_eq!(corrected_ms(None, None), None);
        }

        #[test]
        fn corrected_ms_applies_the_mapper_when_given() {
            let mut mapper = TimestampMapper::new(100);
            mapper.record_checkpoint(0, 80); // 80 samples @ 100Hz = 800ms.
            assert_eq!(corrected_ms(Some(300), Some(&mapper)), Some(1_100));
            assert_eq!(corrected_ms(None, Some(&mapper)), None);
        }

        /// Minimal in-memory `SttSession` test double for exercising
        /// `send_gated_remote_chunk` without a real provider connection — records
        /// every `send_audio` call's `(start_sample, pcm)` so a test can assert
        /// what was actually sent. Relies on `SttSession::keep_alive`'s default
        /// no-op implementation — these tests only cover `RemoteGateState::send_gated`,
        /// not the keepalive timer branch itself.
        struct MockSttSession {
            sent: Vec<(u64, Vec<f32>)>,
        }

        impl MockSttSession {
            fn new() -> Self {
                Self { sent: Vec::new() }
            }
        }

        #[async_trait::async_trait]
        impl SttSession for MockSttSession {
            async fn send_audio(&mut self, chunk: AudioChunk<'_>) -> Result<(), stt_api::SttError> {
                self.sent.push((chunk.start_sample, chunk.pcm.to_vec()));
                Ok(())
            }

            async fn finalize(self: Box<Self>) -> Result<(), stt_api::SttError> {
                Ok(())
            }
        }

        /// `SttSession` test double whose `finalize` never resolves — stands in for
        /// a black-holed network connection so `finalize_with_timeout`'s timeout
        /// branch (task #81) can be exercised without an actual multi-second wait
        /// (see the `start_paused = true` tests below, which fast-forward virtual
        /// time instead).
        struct HangingFinalizeSession;

        #[async_trait::async_trait]
        impl SttSession for HangingFinalizeSession {
            async fn send_audio(&mut self, _chunk: AudioChunk<'_>) -> Result<(), stt_api::SttError> {
                Ok(())
            }

            async fn finalize(self: Box<Self>) -> Result<(), stt_api::SttError> {
                std::future::pending().await
            }
        }

        #[tokio::test(start_paused = true)]
        async fn finalize_with_timeout_gives_up_instead_of_hanging_forever() {
            // With time paused, tokio auto-advances the virtual clock once nothing
            // else can make progress (the only other pending future here is
            // `HangingFinalizeSession::finalize`'s `pending()`, which never wakes
            // anything) — so this resolves once `FINALIZE_TIMEOUT` elapses in
            // virtual time, not real time.
            finalize_with_timeout(Box::new(HangingFinalizeSession), "self").await;
        }

        /// `SttProvider` test double whose `start_session` never resolves — the
        /// provider-level analogue of `HangingFinalizeSession`, for exercising
        /// `start_session_with_timeout`'s timeout branch (task #85).
        struct HangingStartSessionProvider;

        #[async_trait::async_trait]
        impl SttProvider for HangingStartSessionProvider {
            async fn start_session(&self, _config: SttSessionConfig) -> Result<(Box<dyn SttSession>, UnboundedReceiver<SttEvent>), stt_api::SttError> {
                std::future::pending().await
            }
        }

        #[tokio::test(start_paused = true)]
        async fn start_session_with_timeout_reports_error_status_instead_of_hanging_forever() {
            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            let result = start_session_with_timeout(
                &HangingStartSessionProvider,
                SttSessionConfig::new(16_000),
                "self",
                TrackKind::SelfMic,
                &Some(sink.clone()),
            )
            .await;

            assert!(result.is_none());
            let status = sink.lock().unwrap().clone();
            assert!(
                matches!(status.self_status, TrackTranscriptionStatus::Error(_)),
                "a timed-out start_session must report Error, not leave the track stuck on Connecting: {:?}",
                status.self_status
            );
            assert_eq!(status.remote_status, TrackTranscriptionStatus::NotConfigured, "only the named track's status is touched");
        }

        /// A fresh in-memory store with one session already created — just
        /// enough plumbing for `test_remote_gate_state` (task #90's
        /// `open_gap`/`close_open_gap` need a real `SessionStore` and a
        /// `session_id` that satisfies `transcript_segments`'/`transcription_gaps`'
        /// foreign key) without every test hand-rolling the same three lines.
        fn test_store_and_session() -> (SessionStore, SessionId) {
            let store = SessionStore::open_in_memory().unwrap();
            let manifest = test_manifest();
            store.create_session(&manifest).unwrap();
            (store, manifest.session_id)
        }

        fn test_remote_gate_state(
            store: &SessionStore,
            session_id: SessionId,
            session: Option<Box<dyn SttSession>>,
            samples_sent: u64,
            total_injected: u64,
            net_offset: i64,
            last_active: Instant,
        ) -> RemoteGateState<'_> {
            RemoteGateState {
                session,
                gate: None,
                mapper: Some(TimestampMapper::new(100)),
                samples_sent,
                total_injected,
                net_offset,
                last_active,
                reconnect: ReconnectState::new(),
                status_sink: None,
                store,
                session_id,
                recording_started: Instant::now(),
            }
        }

        #[tokio::test]
        async fn remote_gate_state_send_gated_advances_and_checkpoints() {
            let (store, session_id) = test_store_and_session();
            let mut remote = test_remote_gate_state(&store, session_id, Some(Box::new(MockSttSession::new())), 10, 0, 0, Instant::now() - Duration::from_secs(10));
            let before_call = remote.last_active;

            let pcm = vec![0.5f32; 5];
            remote.send_gated(&pcm).await;

            assert_eq!(remote.samples_sent, 15, "samples_sent must advance by the chunk length");
            assert!(remote.last_active > before_call, "a real send must refresh the idle-keepalive clock");

            // The checkpoint recorded after this send should apply a net offset of
            // 0 (no drop/heartbeat yet) from this point on.
            let mapper = remote.mapper.unwrap();
            assert_eq!(mapper.to_wallclock_ms(150), 150);
        }

        #[tokio::test]
        async fn remote_gate_state_send_gated_checkpoints_at_the_provider_clock_including_injected_samples() {
            // Regression test for a Codex review finding: `send_gated` used to
            // checkpoint at `samples_sent` alone, ignoring how many samples
            // `keep_alive`'s `KeepAliveEffect::InjectedAudio` had already pushed
            // into the provider's own audio clock. That under-counts the
            // checkpoint's key, so a later timestamp lookup can wrongly treat a
            // checkpoint as already in effect before the provider's real position
            // ever reached it.
            //
            // 200 samples (2s) worth of heartbeat already injected before this send
            // — e.g. several keepalives fired during a long leading silence — plus
            // a non-zero net offset, so the bug's effect on `to_wallclock_ms` is
            // observable (an offset of 0 would look identical either way).
            let (store, session_id) = test_store_and_session();
            let mut remote = test_remote_gate_state(&store, session_id, Some(Box::new(MockSttSession::new())), 0, 200, -50, Instant::now());

            let pcm = vec![0.5f32; 10];
            remote.send_gated(&pcm).await;

            // `samples_sent` itself must still track only real `send_audio` bytes
            // (used for `AudioChunk::start_sample`), not the injected ones.
            assert_eq!(remote.samples_sent, 10);

            let mapper = remote.mapper.unwrap();
            // A provider timestamp of 1000ms (100 samples @ 100Hz) predates the
            // checkpoint's *correct* key of sent(10) + injected(200) = 210 samples,
            // so it must NOT pick up this checkpoint's -50-sample offset — the
            // provider hadn't actually reached this checkpoint's real position yet.
            // The old, buggy key of 10 alone would have wrongly satisfied
            // `10 <= 100` and returned 1000 + (-500ms) = 500 here instead.
            assert_eq!(
                mapper.to_wallclock_ms(1_000),
                1_000,
                "a timestamp before the checkpoint's true provider-clock position must be uncorrected"
            );
            // A provider timestamp at or past 2100ms (210 samples) does fall after
            // the checkpoint and must pick up its -500ms offset.
            assert_eq!(mapper.to_wallclock_ms(2_100), 2_100 - 500);
        }

        #[test]
        fn reconnect_backoff_doubles_then_caps_at_30s() {
            assert_eq!(reconnect_backoff(0), Duration::from_millis(500));
            assert_eq!(reconnect_backoff(1), Duration::from_millis(1_000));
            assert_eq!(reconnect_backoff(2), Duration::from_millis(2_000));
            // 500ms * 2^6 = 32s, already past the 30s cap.
            assert_eq!(reconnect_backoff(6), Duration::from_millis(30_000));
            assert_eq!(reconnect_backoff(20), Duration::from_millis(30_000), "later attempts stay capped, not overflow");
        }

        #[tokio::test(start_paused = true)]
        async fn reconnect_state_note_disconnect_grows_backoff_and_keeps_the_original_outage_start() {
            let mut reconnect = ReconnectState::new();
            assert!(reconnect.retry_at.is_none());

            reconnect.note_disconnect();
            let first_disconnect = reconnect.disconnected_at.expect("the first note_disconnect must record when the outage started");
            assert_eq!(reconnect.attempt, 1);
            let first_retry_at = reconnect.retry_at.expect("armed after a disconnect");

            // A failed retry attempt re-arms with a longer backoff, but must not
            // move the outage's original start — `RemoteGateState::apply_reconnect_offset`
            // needs the *whole* outage's duration once a later attempt finally
            // succeeds, not just the time since the last failed attempt.
            tokio::time::advance(Duration::from_millis(500)).await;
            reconnect.note_disconnect();
            assert_eq!(reconnect.attempt, 2);
            assert_eq!(reconnect.disconnected_at, Some(first_disconnect));
            assert!(reconnect.retry_at.unwrap() > first_retry_at, "backoff must grow after another failed attempt");

            reconnect.reset();
            assert!(reconnect.retry_at.is_none());
            assert!(reconnect.disconnected_at.is_none());
            assert_eq!(reconnect.attempt, 0);
        }

        #[test]
        fn is_disconnect_retryable_matches_stt_error_and_defaults_true_for_a_silent_channel_close() {
            assert!(is_disconnect_retryable(None), "a channel close with no explicit error (e.g. AssemblyAI's hard cap) must default to retryable");
            assert!(is_disconnect_retryable(Some(&SttError::Transport("boom".to_string()))));
            assert!(is_disconnect_retryable(Some(&SttError::Timeout)));
            assert!(is_disconnect_retryable(Some(&SttError::RateLimited)));
            assert!(!is_disconnect_retryable(Some(&SttError::AuthenticationFailed("bad key".to_string()))));
            assert!(!is_disconnect_retryable(Some(&SttError::PermanentError("rejected".to_string()))));
            assert!(!is_disconnect_retryable(Some(&SttError::SessionClosed)));
        }

        #[test]
        fn note_disconnect_and_maybe_reconnect_arms_for_retryable_and_gives_up_for_permanent_errors() {
            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            let (store, session_id) = test_store_and_session();

            let mut retryable = ReconnectState::new();
            note_disconnect_and_maybe_reconnect(&store, session_id, Instant::now(), TrackKind::SelfMic, Some(&SttError::Timeout), &Some(sink.clone()), &mut retryable);
            assert!(retryable.retry_at.is_some(), "a retryable error must arm a reconnect");
            assert!(matches!(sink.lock().unwrap().self_status, TrackTranscriptionStatus::Error(_)));
            assert!(retryable.open_gap_id.is_some(), "a genuinely new outage must open a transcription_gaps row");

            let mut permanent = ReconnectState::new();
            note_disconnect_and_maybe_reconnect(&store, session_id, Instant::now(), TrackKind::SelfMic, Some(&SttError::AuthenticationFailed("bad key".to_string())), &Some(sink.clone()), &mut permanent);
            assert!(permanent.retry_at.is_none(), "a non-retryable error must not arm a reconnect");
            assert!(permanent.open_gap_id.is_some(), "a non-retryable give-up must still open a gap — it's closed at recording end, not here");
        }

        #[test]
        fn note_self_disconnect_clears_the_session_and_classifies_retryability() {
            let (store, session_id) = test_store_and_session();
            let mut self_session: Option<Box<dyn SttSession>> = Some(Box::new(MockSttSession::new()));
            let mut reconnect = ReconnectState::new();
            note_self_disconnect(&mut self_session, &mut reconnect, &None, Some(&SttError::RateLimited), &store, session_id, Instant::now());
            assert!(self_session.is_none(), "the dead session must be cleared immediately");
            assert!(reconnect.retry_at.is_some());

            let mut self_session = Some(Box::new(MockSttSession::new()) as Box<dyn SttSession>);
            let mut reconnect = ReconnectState::new();
            note_self_disconnect(&mut self_session, &mut reconnect, &None, Some(&SttError::PermanentError("nope".to_string())), &store, session_id, Instant::now());
            assert!(self_session.is_none());
            assert!(reconnect.retry_at.is_none(), "a non-retryable error must not arm a reconnect");
        }

        #[tokio::test]
        async fn remote_gate_state_note_disconnect_clears_session_and_arms_reconnect_when_retryable() {
            let (store, session_id) = test_store_and_session();
            let mut remote = test_remote_gate_state(&store, session_id, Some(Box::new(MockSttSession::new())), 0, 0, 0, Instant::now());
            remote.note_disconnect(Some(&SttError::Transport("dropped".to_string())));
            assert!(remote.session.is_none(), "a dead session must be cleared immediately so send_audio/keep_alive stop being called against it");
            assert!(remote.reconnect.retry_at.is_some());
        }

        #[tokio::test]
        async fn remote_gate_state_note_disconnect_gives_up_on_a_permanent_error() {
            let (store, session_id) = test_store_and_session();
            let mut remote = test_remote_gate_state(&store, session_id, Some(Box::new(MockSttSession::new())), 0, 0, 0, Instant::now());
            remote.note_disconnect(Some(&SttError::AuthenticationFailed("bad key".to_string())));
            assert!(remote.session.is_none());
            assert!(remote.reconnect.retry_at.is_none(), "a non-retryable disconnect must not arm a reconnect");
        }

        #[tokio::test(start_paused = true)]
        async fn remote_gate_state_apply_reconnect_offset_preserves_timestamp_continuity_across_a_reconnect() {
            // Old session: 1_000 samples (10s @ 100Hz) sent with no drops/injects,
            // then disconnects.
            let (store, session_id) = test_store_and_session();
            let mut remote = test_remote_gate_state(&store, session_id, Some(Box::new(MockSttSession::new())), 1_000, 0, 0, Instant::now());
            remote.note_disconnect(Some(&SttError::Transport("dropped".to_string())));
            assert!(remote.session.is_none());

            // The outage (session gone, backoff, reconnect) lasts 5s of real time
            // before a fresh session opens.
            tokio::time::advance(Duration::from_secs(5)).await;
            remote.session = Some(Box::new(MockSttSession::new()));
            remote.apply_reconnect_offset(100);

            assert_eq!(remote.samples_sent, 0, "the new session's own send counter must restart at 0");
            assert_eq!(remote.total_injected, 0);

            // The new session's own clock restarts at 0, but its very first event
            // must still map back to roughly 10s (the old session's 1_000 samples)
            // + 5s (the outage) = 15s of true wall-clock-since-track-start —
            // otherwise persisted timestamps would jump backward on reconnect.
            let mapper = remote.mapper.as_ref().unwrap();
            assert_eq!(mapper.to_wallclock_ms(0), 15_000);
            // A later local event (1s further into the new session) keeps the same
            // baseline offset applied on top.
            assert_eq!(mapper.to_wallclock_ms(1_000), 16_000);
        }

        /// `SttProvider` test double whose `start_session` always succeeds
        /// immediately — the success-path counterpart of `HangingStartSessionProvider`,
        /// for exercising `attempt_reconnect`'s success branch.
        struct ImmediateSttProvider;

        #[async_trait::async_trait]
        impl SttProvider for ImmediateSttProvider {
            async fn start_session(&self, _config: SttSessionConfig) -> Result<(Box<dyn SttSession>, UnboundedReceiver<SttEvent>), stt_api::SttError> {
                let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
                Ok((Box::new(MockSttSession::new()), rx))
            }
        }

        #[tokio::test]
        async fn attempt_reconnect_reports_connected_status_and_returns_the_session_on_success() {
            // Task #87: `attempt_reconnect` no longer takes `&mut ReconnectState`
            // at all (it must be spawnable via `tokio::spawn`, which needs an
            // entirely owned/`'static` future) — the caller applies every
            // `ReconnectState` effect itself once the `JoinHandle` resolves (see
            // `run_live_transcription`'s reconnect-completion `select!` arms and
            // `note_reconnect_failure`). This just checks the function's own
            // direct contract: on success it returns the new session/events pair
            // and reports `Connected` status.
            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            let provider: Arc<dyn SttProvider> = Arc::new(ImmediateSttProvider);

            let result = attempt_reconnect(provider, SttSessionConfig::new(16_000), "self", TrackKind::SelfMic, Some(sink.clone())).await;

            assert!(result.is_some());
            assert_eq!(sink.lock().unwrap().self_status, TrackTranscriptionStatus::Connected);
        }

        #[tokio::test(start_paused = true)]
        async fn remote_reconnect_flow_uses_the_full_outage_duration_for_timestamp_continuity() {
            // Regression test for a real bug: `attempt_reconnect` used to call
            // `reconnect.reset()` on success, clearing `disconnected_at` before
            // `apply_reconnect_offset` (invoked right after, at the real
            // `select!` call site in `run_live_transcription`) could read it —
            // silently making every Remote reconnect estimate a zero-length
            // outage no matter how long the provider was actually unreachable.
            // Unlike `remote_gate_state_apply_reconnect_offset_preserves_timestamp_continuity_across_a_reconnect`
            // above (which sets `remote.session` directly, bypassing
            // `attempt_reconnect` entirely and so never exercised this), this
            // test drives the exact same two calls the `select!` arm makes.
            let (store, session_id) = test_store_and_session();
            let mut remote = test_remote_gate_state(&store, session_id, Some(Box::new(MockSttSession::new())), 1_000, 0, 0, Instant::now());
            remote.note_disconnect(Some(&SttError::Transport("dropped".to_string())));
            assert!(remote.session.is_none());

            tokio::time::advance(Duration::from_secs(5)).await;
            let provider: Arc<dyn SttProvider> = Arc::new(ImmediateSttProvider);
            let result = attempt_reconnect(provider, SttSessionConfig::new(100), "remote", TrackKind::RemoteAudio, None).await;
            let (session, _events) = result.expect("ImmediateSttProvider always succeeds");
            remote.session = Some(session);
            remote.apply_reconnect_offset(100);

            // 10s (the old session's 1_000 samples @ 100Hz) + 5s (the outage) =
            // 15s of true wall-clock-since-track-start. The pre-fix behavior
            // returned 10_000 here, having lost the outage entirely.
            let mapper = remote.mapper.as_ref().unwrap();
            assert_eq!(mapper.to_wallclock_ms(0), 15_000);
            assert!(remote.reconnect.retry_at.is_none(), "apply_reconnect_offset must reset reconnect back to a healthy baseline once it has used disconnected_at");
            assert!(remote.reconnect.disconnected_at.is_none());
        }

        #[tokio::test]
        async fn recv_reconnect_result_resolves_once_the_spawned_task_completes() {
            let mut in_flight: Option<JoinHandle<Option<ReconnectSessionPair>>> = Some(tokio::spawn(async { None }));
            let result = recv_reconnect_result(&mut in_flight).await;
            assert!(matches!(result, Ok(None)));
        }

        #[tokio::test(start_paused = true)]
        async fn recv_reconnect_result_never_resolves_when_nothing_is_in_flight() {
            // Mirrors `recv_track_event`'s `None` branch (see its doc comment):
            // a `select!` branch guarded by `in_flight.is_some()` must never see
            // this future spuriously ready when there is no task to await.
            let mut in_flight: Option<JoinHandle<Option<ReconnectSessionPair>>> = None;
            let timed_out = tokio::time::timeout(Duration::from_secs(60), recv_reconnect_result(&mut in_flight)).await;
            assert!(timed_out.is_err(), "recv_reconnect_result(&mut None) must never resolve");
        }

        #[test]
        fn note_reconnect_failure_from_an_already_reported_start_session_error_only_arms_backoff() {
            // The `Ok(None)` case (see `note_reconnect_failure`'s doc comment):
            // `start_session_with_timeout` already reported `Error` status
            // itself, so this must not overwrite it — just arm the next backoff.
            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            set_status(&Some(sink.clone()), TrackKind::SelfMic, TrackTranscriptionStatus::Error("original".to_string()));
            let mut reconnect = ReconnectState::new();

            note_reconnect_failure(TrackKind::SelfMic, None, &Some(sink.clone()), &mut reconnect);

            assert!(reconnect.retry_at.is_some(), "a failed attempt must arm another backoff");
            assert_eq!(sink.lock().unwrap().self_status, TrackTranscriptionStatus::Error("original".to_string()), "must not overwrite a status start_session_with_timeout already reported");
        }

        #[tokio::test]
        async fn note_reconnect_failure_from_a_panicked_task_reports_error_status_and_arms_backoff() {
            // The `Err(join_err)` case: unlike a plain `Ok(None)`, nothing has
            // reported `Error` status yet (the task never got to
            // `start_session_with_timeout`'s own reporting), so this must do so
            // itself before arming the backoff.
            let handle: JoinHandle<Option<ReconnectSessionPair>> = tokio::spawn(async { panic!("simulated reconnect task panic") });
            // `Result::expect_err` needs `T: Debug`, but `ReconnectSessionPair`
            // contains `Box<dyn SttSession>` which isn't — match instead.
            let join_err = match handle.await {
                Err(join_err) => join_err,
                Ok(_) => panic!("a panicking task must produce a JoinError"),
            };

            let sink = Arc::new(Mutex::new(TranscriptionStatus::default()));
            let mut reconnect = ReconnectState::new();
            note_reconnect_failure(TrackKind::RemoteAudio, Some(&join_err), &Some(sink.clone()), &mut reconnect);

            assert!(reconnect.retry_at.is_some());
            assert!(matches!(sink.lock().unwrap().remote_status, TrackTranscriptionStatus::Error(_)), "a panicked task has nothing else to report Error status, so this must");
        }

        #[tokio::test]
        async fn note_disconnect_and_maybe_reconnect_does_not_rearm_while_a_reconnect_is_already_in_flight() {
            // Task #87's multi-reconnect guard: a stray disconnect signal
            // arriving while a reconnect task is already running for this track
            // (see this function's doc comment) must not stack a second backoff
            // on top, mirroring the existing `retry_at.is_some()` guard.
            let (store, session_id) = test_store_and_session();
            let mut reconnect = ReconnectState::new();
            reconnect.in_flight = Some(tokio::spawn(std::future::pending::<Option<ReconnectSessionPair>>()));

            note_disconnect_and_maybe_reconnect(&store, session_id, Instant::now(), TrackKind::SelfMic, Some(&SttError::Timeout), &None, &mut reconnect);

            assert!(reconnect.retry_at.is_none(), "must not arm a new backoff while a reconnect task is already in flight");
            assert!(reconnect.open_gap_id.is_none(), "an already-armed outage must not open a second gap either");
            reconnect.abort_in_flight();
        }

        #[tokio::test]
        async fn abort_in_flight_clears_the_handle() {
            let mut reconnect = ReconnectState::new();
            reconnect.in_flight = Some(tokio::spawn(std::future::pending::<Option<ReconnectSessionPair>>()));

            reconnect.abort_in_flight();

            assert!(reconnect.in_flight.is_none());
        }

        #[test]
        fn abort_in_flight_is_a_no_op_when_nothing_is_in_flight() {
            let mut reconnect = ReconnectState::new();
            reconnect.abort_in_flight();
            assert!(reconnect.in_flight.is_none());
        }

        #[test]
        fn remote_silence_gate_wiring_skips_silence_and_corrects_timestamps() {
            // Small, easy-to-reason-about rates (mirrors `silence_gate`'s own test
            // config): 100 samples/sec, 10-sample (100ms) windows, 20-sample
            // (200ms) pre-roll, 30-sample (300ms) hangover.
            let config = GateConfig {
                sample_rate_hz: 100,
                vad_window_ms: 100,
                hangover_ms: 300,
                pre_roll_ms: 200,
                speaking_rms_threshold: 0.02,
                silent_rms_threshold: 0.01,
            };
            let mut gate = SilenceGate::new(config);
            let mut mapper = TimestampMapper::new(100);
            let mut net_offset: i64 = 0;
            let mut samples_sent: u64 = 0;

            // Mirrors exactly what `run_live_transcription`'s `select!` loop does
            // per `GateAction` for the Remote track (see that match arm),
            // reproduced here directly (rather than driving the full function,
            // which would require a live provider connection) so the gate ->
            // mapper wiring itself is under test without any network I/O.
            fn apply(gate: &mut SilenceGate, samples: &[f32], net_offset: &mut i64, samples_sent: &mut u64, mapper: &mut TimestampMapper) {
                for action in gate.process(samples) {
                    match action {
                        GateAction::Send(pcm) => {
                            *samples_sent += pcm.len() as u64;
                            mapper.record_checkpoint(*samples_sent, *net_offset);
                        }
                        GateAction::SendStitched(pcm) => {
                            *samples_sent += pcm.len() as u64;
                            mapper.record_checkpoint(*samples_sent, *net_offset);
                        }
                        GateAction::Drop { sample_count } => {
                            *net_offset += sample_count as i64;
                            mapper.record_checkpoint(*samples_sent, *net_offset);
                        }
                    }
                }
            }

            // 1s of leading silence: per `silence_gate`'s own
            // `starts_silent_and_drops_leading_silence` test, this drops 80 of the
            // 100 samples (the trailing 20 stay buffered as pre-roll).
            apply(&mut gate, &[0.0; 100], &mut net_offset, &mut samples_sent, &mut mapper);
            assert_eq!(net_offset, 80, "800ms of leading silence must be counted as dropped");
            assert_eq!(samples_sent, 0, "nothing was actually sent yet");

            // Then 500ms of speech: triggers Silent -> Speaking (stitching the
            // 20-sample pre-roll on) and keeps sending thereafter.
            apply(&mut gate, &[0.5; 50], &mut net_offset, &mut samples_sent, &mut mapper);
            assert_eq!(samples_sent, 70, "20 pre-roll samples + 50 speech samples were sent");
            assert_eq!(net_offset, 80, "no further silence was dropped once speech started");

            // A transcript segment reported by the provider at
            // [300ms, 700ms) provider-relative time must come back shifted by the
            // 800ms of silence dropped before any of it was sent.
            assert_eq!(mapper.to_wallclock_ms(300), 1_100);
            assert_eq!(mapper.to_wallclock_ms(700), 1_500);

            // Persisting a `FinalTranscript` for this track through `persist_event`
            // with this mapper must apply that same correction, while the same
            // event on a `None` mapper (Self track, or gate disabled) must not.
            let store = SessionStore::open_in_memory().unwrap();
            let manifest = test_manifest();
            store.create_session(&manifest).unwrap();
            let event = SttEvent::FinalTranscript {
                text: "hello".to_string(),
                words: None,
                audio_start_ms: Some(300),
                audio_end_ms: Some(700),
                extra: Default::default(),
            };
            persist_event(&store, None, manifest.session_id, TrackKind::RemoteAudio, event, Some(&mapper), &mut 0);
            let segments = store.list_transcript_segments(manifest.session_id).unwrap();
            assert_eq!(segments.len(), 1);
            assert_eq!(segments[0].start_ms, Some(1_100));
            assert_eq!(segments[0].end_ms, Some(1_500));
        }

        #[test]
        fn persist_pending_interim_inserts_a_final_segment() {
            let store = SessionStore::open_in_memory().unwrap();
            let manifest = test_manifest();
            store.create_session(&manifest).unwrap();

            let pending = PendingInterim { text: "こんにち".to_string(), audio_start_ms: Some(10), audio_end_ms: Some(500) };
            persist_pending_interim(&store, manifest.session_id, TrackKind::SelfMic, Some(pending), None);

            let segments = store.list_transcript_segments(manifest.session_id).unwrap();
            assert_eq!(segments.len(), 1);
            assert!(segments[0].is_final);
            assert_eq!(segments[0].text, "こんにち");
            assert_eq!(segments[0].track, Some(TrackKind::SelfMic));
        }

        #[test]
        fn persist_pending_interim_skips_when_none_or_blank() {
            let store = SessionStore::open_in_memory().unwrap();
            let manifest = test_manifest();
            store.create_session(&manifest).unwrap();

            persist_pending_interim(&store, manifest.session_id, TrackKind::SelfMic, None, None);
            persist_pending_interim(&store, manifest.session_id, TrackKind::SelfMic, Some(PendingInterim { text: "   ".to_string(), audio_start_ms: None, audio_end_ms: None }), None);

            assert!(store.list_transcript_segments(manifest.session_id).unwrap().is_empty());
        }

        fn test_manifest() -> recorder_domain::SessionManifest {
            recorder_domain::SessionManifest {
                schema_version: 1,
                session_id: SessionId::new(),
                started_at: chrono::Utc::now(),
                ended_at: None,
                platform: "test".to_string(),
                app_version: "0.0.0".to_string(),
                capture: recorder_domain::CaptureManifest {
                    microphone_device_id: "default".to_string(),
                    remote_source_id: "default".to_string(),
                    remote_source_kind: recorder_domain::RemoteSourceKind::EndpointLoopback,
                },
                audio: recorder_domain::AudioManifest { sample_rate: 48_000, segment_duration_ms: 30_000, tracks: vec![TrackKind::SelfMic, TrackKind::RemoteAudio] },
                consent: recorder_domain::ConsentManifest { confirmed_by_user: true, confirmed_at: chrono::Utc::now() },
            }
        }

        #[test]
        fn target_sample_rate_hz_pins_openai_and_assemblyai_leaves_others_at_capture_rate() {
            assert_eq!(target_sample_rate_hz(SttProviderKind::OpenAi, 48_000), 24_000);
            assert_eq!(target_sample_rate_hz(SttProviderKind::AssemblyAi, 48_000), 16_000);
            assert_eq!(target_sample_rate_hz(SttProviderKind::Deepgram, 48_000), 48_000);
            assert_eq!(target_sample_rate_hz(SttProviderKind::Google, 48_000), 48_000);
        }

        /// Minimal in-memory `CredentialStore` for exercising `build_stt_provider`
        /// without touching a real OS keyring — same "just a `HashMap`" shape as
        /// `credential_store::EncryptedFileStore`'s test doubles elsewhere in this
        /// workspace, kept local since no shared test-only crate exposes one.
        struct InMemoryCredentialStore(std::collections::HashMap<(String, String), String>);

        impl InMemoryCredentialStore {
            fn with(service: &str, account: &str, secret: &str) -> Self {
                let mut map = std::collections::HashMap::new();
                map.insert((service.to_string(), account.to_string()), secret.to_string());
                Self(map)
            }
        }

        impl CredentialStore for InMemoryCredentialStore {
            fn save(&self, _service: &str, _account: &str, _secret: &str) -> Result<(), credential_store::StoreError> {
                unimplemented!("not needed by these tests")
            }
            fn load(&self, service: &str, account: &str) -> Result<String, credential_store::StoreError> {
                self.0.get(&(service.to_string(), account.to_string())).cloned().ok_or_else(|| credential_store::StoreError::NotFound {
                    service: service.to_string(),
                    account: account.to_string(),
                })
            }
            fn delete(&self, _service: &str, _account: &str) -> Result<(), credential_store::StoreError> {
                unimplemented!("not needed by these tests")
            }
        }

        #[test]
        fn build_stt_provider_reports_credential_missing_for_every_kind_when_store_is_empty() {
            let store = InMemoryCredentialStore(std::collections::HashMap::new());
            for kind in [SttProviderKind::Deepgram, SttProviderKind::OpenAi, SttProviderKind::Google, SttProviderKind::AssemblyAi] {
                let Err(err) = build_stt_provider(kind, &store) else { panic!("expected CredentialMissing for {kind:?}") };
                assert!(matches!(err, SttProviderFactoryError::CredentialMissing { kind: k, .. } if k == kind));
            }
        }

        #[test]
        fn build_stt_provider_constructs_openai_and_assemblyai_from_a_bare_api_key() {
            let store = InMemoryCredentialStore::with(stt_openai::CREDENTIAL_SERVICE, stt_openai::OPENAI_STT_API_KEY_ACCOUNT, "sk-test");
            assert!(build_stt_provider(SttProviderKind::OpenAi, &store).is_ok());

            let store = InMemoryCredentialStore::with(stt_assemblyai::CREDENTIAL_SERVICE, stt_assemblyai::ASSEMBLYAI_API_KEY_ACCOUNT, "aai-test");
            assert!(build_stt_provider(SttProviderKind::AssemblyAi, &store).is_ok());
        }

        #[test]
        fn build_stt_provider_constructs_google_from_valid_credentials_json() {
            let credentials = GoogleSttCredentials::new("my-project", "global");
            let json = serde_json::to_string(&credentials).unwrap();
            let store = InMemoryCredentialStore::with(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT, &json);
            assert!(build_stt_provider(SttProviderKind::Google, &store).is_ok());
        }

        #[test]
        fn build_stt_provider_reports_invalid_credential_for_malformed_google_json() {
            let store = InMemoryCredentialStore::with(stt_google::CREDENTIAL_SERVICE, stt_google::GOOGLE_STT_CREDENTIALS_ACCOUNT, "not json");
            let Err(err) = build_stt_provider(SttProviderKind::Google, &store) else { panic!("expected InvalidCredential") };
            assert!(matches!(err, SttProviderFactoryError::InvalidCredential { kind: SttProviderKind::Google, .. }));
        }
    }
}

#[cfg(feature = "live-transcription")]
pub use stt_wiring::run_live_transcription;

/// `live-transcription` feature disabled: no STT provider types are even compiled in,
/// so this just drains and discards `audio_rx` — keeping
/// `windows_session::run_windows_capture_session`'s call site identical regardless of
/// the feature (see this module's doc comment). Reports `Unavailable` on
/// `status_sink` for both tracks immediately, same as the non-Windows desktop path
/// (see `apps/desktop/src/transcription_status.rs`).
#[cfg(not(feature = "live-transcription"))]
pub async fn run_live_transcription(
    _session_id: SessionId,
    _sample_rate_hz: u32,
    _credential_store: Option<Arc<dyn CredentialStore + Send + Sync>>,
    mut audio_rx: Receiver<(TrackKind, Vec<f32>, u32)>,
    _store: &SessionStore,
    status_sink: Option<Arc<Mutex<TranscriptionStatus>>>,
    _silence_gate_enabled: bool,
    _broker: Option<&LocalBroker>,
) {
    set_both_status(&status_sink, TrackTranscriptionStatus::Unavailable);
    while audio_rx.recv().await.is_some() {}
}
