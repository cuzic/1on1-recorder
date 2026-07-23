//! Local-socket control server for `1on1ctl` (`apps/desktop-cli`) — lets
//! recording be started/stopped/checked from outside the GUI regardless of
//! which screen is showing, without duplicating any logic: every command just
//! calls the same `actions::*` functions `ui.rs`'s buttons already call.
//!
//! One connection = one request line + one response line (newline-delimited
//! JSON via `control_protocol`), then the connection closes.

use std::sync::Arc;
use std::time::Duration;

use control_protocol::{Command, Response, StatusDto, TrackStatusDto, TranscriptionStatusDto};
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::{ListenerOptions, Name};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::actions;
use crate::app_state::AppState;

/// Called once from `main.rs`, right after `AppState` is built and before
/// `LaunchBuilder::desktop()...launch()` takes over the main thread — the
/// accept loop below runs on the already-`enter()`ed multi-thread tokio
/// runtime's worker threads (see `main.rs`'s doc comment on why that runtime
/// is entered before `AppState` exists), independent of the webview event
/// loop that `.launch()` blocks on.
///
/// Ancillary feature: on any failure to bind, logs a warning and returns
/// without spawning an accept loop, rather than touching `AppState` or
/// panicking — CLI control being unavailable must never crash the GUI.
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let name = match control_protocol::control_socket_name() {
            Ok(name) => name,
            Err(err) => {
                tracing::warn!(%err, "could not resolve control socket name; CLI control unavailable this run");
                return;
            }
        };
        match bind(name).await {
            Ok(listener) => {
                tracing::info!("control server listening");
                accept_loop(listener, state).await;
            }
            Err(err) => tracing::warn!(%err, "control server failed to start; CLI control unavailable this run"),
        }
    });
}

/// Binds the control socket, recovering from a stale socket *file* left
/// behind by a prior run — only possible on the platforms where
/// `control_protocol::control_socket_name()` falls back to a real filesystem
/// path (macOS today; see that function's doc comment), since named pipes
/// (Windows) and the Linux abstract namespace are kernel-reclaimed on process
/// exit regardless of how the process exited. This matters here because the
/// tray "Quit" handler (`ui.rs`) calls `std::process::exit(0)`, which skips
/// `Listener`'s `Drop` impl (the thing that would otherwise unlink the socket
/// file on a clean exit) — so a stale `control.sock` is the *expected* case
/// on macOS after a normal quit, not just after a crash.
///
/// Deliberately does not use `ListenerOptions::try_overwrite(true)`: its own
/// docs note it can hijack a still-live listener via a TOCTOU race. Instead,
/// on `AddrInUse`, this probes liveness with a short connect attempt first —
/// if something answers, a real server is already running (most likely a
/// second GUI instance) and this bind backs off rather than displacing it.
async fn bind(name: Name<'static>) -> std::io::Result<Listener> {
    match ListenerOptions::new().name(name.clone()).create_tokio() {
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && control_protocol::uses_filesystem_path() => {
            if is_alive(name.clone()).await {
                Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "another instance's control server is already running"))
            } else {
                let _ = std::fs::remove_file(control_protocol::control_socket_path());
                ListenerOptions::new().name(name).create_tokio()
            }
        }
        other => other,
    }
}

async fn is_alive(name: Name<'static>) -> bool {
    tokio::time::timeout(Duration::from_millis(200), Stream::connect(name)).await.map(|r| r.is_ok()).unwrap_or(false)
}

async fn accept_loop(listener: Listener, state: Arc<AppState>) {
    loop {
        match listener.accept().await {
            Ok(conn) => {
                tokio::spawn(handle_conn(conn, state.clone()));
            }
            Err(err) => tracing::warn!(%err, "control server accept error"),
        }
    }
}

async fn handle_conn(conn: Stream, state: Arc<AppState>) {
    let mut reader = BufReader::new(&conn);
    let mut line = String::new();
    if matches!(reader.read_line(&mut line).await, Err(_) | Ok(0)) {
        return;
    }

    let response = match serde_json::from_str::<Command>(line.trim_end()) {
        Ok(cmd) => dispatch(cmd, &state).await,
        Err(err) => Response::Err { message: format!("invalid request: {err}"), status: status_dto(&state) },
    };

    let mut payload = match serde_json::to_string(&response) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(%err, "failed to serialize control server response");
            return;
        }
    };
    payload.push('\n');

    let mut writer = &conn;
    if let Err(err) = writer.write_all(payload.as_bytes()).await {
        tracing::warn!(%err, "failed to write control server response");
    }
}

async fn dispatch(cmd: Command, state: &AppState) -> Response {
    match cmd {
        Command::Status => Response::Ok(status_dto(state)),
        Command::Start => {
            // Product decision: a CLI-triggered start bypasses the GUI's
            // one-time "I consent to this meeting being recorded and
            // uploaded." checkbox — this is a single-user personal tool.
            // `confirm_consent` sets `state.consent_confirmed` for the rest
            // of the app run, so a GUI window open at the same time will
            // also stop showing its consent prompt from this point on.
            actions::confirm_consent(state);
            match actions::start_recording(state) {
                Ok(status) => Response::Ok(status.into()),
                Err(message) => Response::Err { message, status: status_dto(state) },
            }
        }
        Command::Stop => match actions::stop_recording(state).await {
            Ok(status) => Response::Ok(status.into()),
            Err(message) => Response::Err { message, status: status_dto(state) },
        },
    }
}

fn status_dto(state: &AppState) -> StatusDto {
    actions::get_status(state).into()
}

impl From<crate::status::Status> for StatusDto {
    fn from(s: crate::status::Status) -> Self {
        StatusDto {
            recording: s.recording,
            elapsed_ms: s.elapsed_ms,
            self_rms: s.self_rms,
            self_peak: s.self_peak,
            remote_rms: s.remote_rms,
            remote_peak: s.remote_peak,
            consent_confirmed: s.consent_confirmed,
            uploaded_segments: s.uploaded_segments,
            pending_segments: s.pending_segments,
            last_error: s.last_error,
            last_session_id: s.last_session_id,
            last_total_duration_ms: s.last_total_duration_ms,
            transcription_status: TranscriptionStatusDto {
                self_status: s.transcription_status.self_status.into(),
                remote_status: s.transcription_status.remote_status.into(),
            },
        }
    }
}

impl From<crate::transcription_status::TrackTranscriptionStatus> for TrackStatusDto {
    fn from(s: crate::transcription_status::TrackTranscriptionStatus) -> Self {
        use crate::transcription_status::TrackTranscriptionStatus as T;
        match s {
            T::NotConfigured => Self::NotConfigured,
            T::Connecting => Self::Connecting,
            T::Connected => Self::Connected,
            T::Error(m) => Self::Error(m),
            T::Unavailable => Self::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::sync::Mutex;

    use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions as SyncListenerOptions, Stream as SyncStream};

    use super::*;
    use crate::app_settings::AppSettings;
    use crate::config::Config;
    use crate::summary_consumer::SummaryConsumer;

    /// Builds a real, tempdir-backed `AppState` — every constructor it needs
    /// (`SessionStore::open`, `FallbackCredentialStore::new`, `RhaiEngine::new`,
    /// `LocalBroker::new`) takes only paths/args, none of them touch a
    /// Dioxus context, so this exercises the exact same code path
    /// `control_server::dispatch` runs against in the real app.
    fn test_state(dir: &std::path::Path) -> Arc<AppState> {
        let store = Arc::new(session_store::SessionStore::open(&dir.join("sessions.db")).expect("open session-store"));
        let credential_store = Arc::new(credential_store::FallbackCredentialStore::new(dir.join("credentials")).expect("open credential-store"));
        let adapter = Arc::new(upload_client::HttpUploadClient::new(
            "http://127.0.0.1:0".to_string(),
            Duration::from_secs(1),
            Arc::new(credential_store::CredentialStoreTokenProvider::new(credential_store.clone(), "1on1-recorder-test".to_string(), "upload-token".to_string())),
        ));
        let app_settings = Arc::new(Mutex::new(AppSettings::load(dir)));
        let broker = local_broker::LocalBroker::new();
        let summary_consumer = SummaryConsumer::new(broker.clone(), store.clone(), credential_store.clone(), app_settings.clone());
        let settings_provider = Arc::new(crate::AppSettingsProvider { app_settings: app_settings.clone(), credential_store: credential_store.clone() });
        let rhai_engine = rhai_engine::RhaiEngine::new(broker.clone(), store.clone(), credential_store.clone(), settings_provider);

        Arc::new(AppState {
            store,
            adapter,
            config: Config::load(dir.to_path_buf()),
            broker,
            summary_consumer,
            rhai_engine,
            credential_store,
            app_data_dir: dir.to_path_buf(),
            app_settings,
            consent_confirmed: Mutex::new(false),
            current: Mutex::new(None),
            last_error: Mutex::new(None),
            last_summary: Mutex::new(None),
        })
    }

    fn test_socket_name(tag: &str) -> Name<'static> {
        format!("1on1-recorder-control-test-{tag}-{}.sock", std::process::id())
            .to_ns_name::<GenericNamespaced>()
            .expect("namespaced name should resolve on Linux")
    }

    fn send(name: Name<'static>, cmd: &Command) -> Response {
        let mut conn = SyncStream::connect(name).expect("connect to test control server");
        let mut payload = serde_json::to_string(cmd).unwrap();
        payload.push('\n');
        conn.write_all(payload.as_bytes()).unwrap();

        let mut reader = StdBufReader::new(conn);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim_end()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_start_status_stop_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let name = test_socket_name("roundtrip");

        let listener = SyncListenerOptions::new().name(name.clone()).create_tokio().expect("bind test control socket");
        tokio::spawn(accept_loop(listener, state));
        // Give the accept loop a moment to start `listener.accept().await`.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let idle = send(name.clone(), &Command::Status);
        match idle {
            Response::Ok(status) => assert!(!status.recording),
            Response::Err { message, .. } => panic!("unexpected error: {message}"),
        }

        let started = send(name.clone(), &Command::Start);
        let status = match started {
            Response::Ok(status) => status,
            Response::Err { message, .. } => panic!("start failed: {message}"),
        };
        assert!(status.recording);
        assert!(status.consent_confirmed, "Start must auto-confirm consent");

        let while_recording = send(name.clone(), &Command::Status);
        match while_recording {
            Response::Ok(status) => assert!(status.recording),
            Response::Err { message, .. } => panic!("unexpected error: {message}"),
        }

        let double_start = send(name.clone(), &Command::Start);
        match double_start {
            Response::Err { message, status } => {
                assert_eq!(message, "a recording is already in progress");
                assert!(status.recording);
            }
            Response::Ok(_) => panic!("expected an error for a double start"),
        }

        let stopped = send(name.clone(), &Command::Stop);
        match stopped {
            Response::Ok(status) => assert!(!status.recording),
            Response::Err { message, .. } => panic!("stop failed: {message}"),
        }

        let double_stop = send(name, &Command::Stop);
        match double_stop {
            Response::Err { message, .. } => assert_eq!(message, "not recording"),
            Response::Ok(_) => panic!("expected an error for a double stop"),
        }
    }
}
