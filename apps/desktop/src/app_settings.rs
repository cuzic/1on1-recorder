//! Non-secret app settings, persisted as a single JSON file under `app_data_dir`
//! (see `main.rs::app_data_dir`), parallel to the `credentials/` directory
//! `credential-store::FallbackCredentialStore` owns.
//!
//! `settings.rs` currently saves a few non-secret selection values (e.g.
//! `SELECTED_PROVIDER_ACCOUNT`/`SELECTED_MODEL_ACCOUNT`) through
//! `credential_store` anyway, because that was the only persistence mechanism
//! available at the time. That's a mismatch: `credential-store` is meant for
//! actual secrets, and on Windows each entry is capped at
//! `CRED_MAX_CREDENTIAL_BLOB_SIZE` (2,560 bytes) by the OS credential manager.
//! Non-secret settings that are expected to grow past that — a summary prompt
//! template, an Ollama base URL, a whisper model file path — need a home that
//! doesn't share that limit.
//!
//! This module is that home. It does *not* migrate the existing
//! credential-store-backed selections above (out of scope, larger blast
//! radius — a separate task); it only gives future settings a place to live.
//!
//! Modeled after `config::Config::load`'s `app_data_dir: PathBuf` convention,
//! but as an explicit load/save pair (rather than load-only) since, unlike
//! `Config` (env-var-derived, read-only at runtime), these values are meant to
//! be edited from the settings UI and written back.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename for the settings file, placed directly under `app_data_dir` —
/// sibling to the `credentials/` directory and `session-store.sqlite3` file
/// `main.rs`/`config::Config` already put there.
const SETTINGS_FILE_NAME: &str = "settings.json";

/// Non-secret, app-scoped settings. Every field is `Option` and the whole
/// struct derives `Default` plus `#[serde(default)]`, so:
/// - a freshly created store (no file yet) is just `AppSettings::default()`;
/// - a file written by an older build (missing newer fields) still
///   deserializes, defaulting the missing fields to `None`;
/// - a file written by a newer build (extra fields this build doesn't know
///   about) still deserializes here — `serde`'s default behavior already
///   ignores unrecognized fields on read; this build's own `save` will drop
///   them on the next overwrite, which is fine for this single-user,
///   single-process app.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Base URL of a local/self-hosted Ollama server, for a future summarize
    /// backend that talks to it directly instead of a hosted LLM API.
    pub ollama_base_url: Option<String>,
    /// Freeform summary prompt template, overriding `summarize`'s built-in
    /// default when set. Long-form text — the original reason this store
    /// exists instead of reusing `credential_store`.
    pub summary_template: Option<String>,
    /// Local filesystem path to a whisper.cpp (or similar) model file, for a
    /// future local/offline STT backend.
    pub whisper_model_path: Option<PathBuf>,
    /// Root directory session exports are written under, overriding whatever
    /// default the export feature would otherwise pick.
    pub exports_root: Option<PathBuf>,
    /// Enables `app-service`'s silence gate (see `silence_gate::SilenceGate`)
    /// for the Remote track only during live transcription — silent stretches
    /// of remote audio are skipped rather than streamed to the STT provider,
    /// cutting STT usage/cost. `None` (the default, same as `Some(false)`)
    /// keeps the pre-existing behavior of streaming all remote audio
    /// unconditionally; no settings-UI toggle exists yet for this field, so it
    /// currently only takes effect via manual `settings.json` edits.
    pub silence_gate_enabled: Option<bool>,
    /// `capture_windows`/`capture_macos::device_select::DeviceInfo::id` of the
    /// microphone to record the Self track from, as chosen in the settings
    /// screen's "録音デバイス" section. `None` (the default, same as before this
    /// field existed) means "whatever the OS reports as its current default
    /// capture device" — see `recording.rs::start`, which resolves this into
    /// `run_windows_capture_session`/`run_macos_capture_session`'s
    /// `mic_device_id` parameter.
    pub microphone_device_id: Option<String>,
    /// Same as `microphone_device_id`, but for the render/loopback device the
    /// Remote track is captured from (a speaker/output device — WASAPI loopback
    /// on Windows, ScreenCaptureKit's system-audio capture on macOS).
    pub render_device_id: Option<String>,
}

impl AppSettings {
    /// Absolute path to the settings file under `app_data_dir`.
    pub fn path(app_data_dir: &Path) -> PathBuf {
        app_data_dir.join(SETTINGS_FILE_NAME)
    }

    /// Loads settings from `app_data_dir`'s settings file. Returns
    /// `AppSettings::default()` if the file doesn't exist yet (first run), and
    /// also falls back to defaults (logging a warning) on a read or parse
    /// error rather than propagating it — this is convenience state, not
    /// something a corrupt/foreign file should be able to block startup over.
    pub fn load(app_data_dir: &Path) -> Self {
        let path = Self::path(app_data_dir);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                tracing::warn!(%err, ?path, "failed to read app settings file, falling back to defaults");
                return Self::default();
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(settings) => settings,
            Err(err) => {
                tracing::warn!(%err, ?path, "failed to parse app settings file, falling back to defaults");
                Self::default()
            }
        }
    }

    /// Overwrites the settings file under `app_data_dir` with this value's
    /// current contents. Always a whole-file rewrite — no partial/diff update
    /// or file locking, since this app is single-user/single-process (see this
    /// module's doc comment).
    ///
    /// Called from `settings.rs`'s "要約プロンプトテンプレート" section
    /// (`save_summary_template`), the first settings-UI writer of any
    /// `AppSettings` field — see `summary_template.rs` for the preset
    /// definitions that section resolves into `summary_template` before
    /// calling this.
    pub fn save(&self, app_data_dir: &Path) -> std::io::Result<()> {
        let path = Self::path(app_data_dir);
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_without_existing_file_returns_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = AppSettings::load(dir.path());
        assert_eq!(loaded, AppSettings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = AppSettings {
            ollama_base_url: Some("http://localhost:11434".to_string()),
            summary_template: Some("要約テンプレート\n複数行もOK".to_string()),
            whisper_model_path: Some(PathBuf::from("/models/ggml-large-v3.bin")),
            exports_root: Some(PathBuf::from("/exports")),
            silence_gate_enabled: Some(true),
            microphone_device_id: Some("{0.0.1.00000000}.{aaaa}".to_string()),
            render_device_id: Some("{0.0.0.00000000}.{bbbb}".to_string()),
        };

        settings.save(dir.path()).expect("save");
        let loaded = AppSettings::load(dir.path());

        assert_eq!(loaded, settings);
    }

    #[test]
    fn save_overwrites_previous_contents_entirely() {
        let dir = tempfile::tempdir().expect("tempdir");
        AppSettings { ollama_base_url: Some("http://old:11434".to_string()), ..Default::default() }.save(dir.path()).expect("save 1");

        let second = AppSettings { summary_template: Some("new template".to_string()), ..Default::default() };
        second.save(dir.path()).expect("save 2");

        let loaded = AppSettings::load(dir.path());
        assert_eq!(loaded, second);
        assert_eq!(loaded.ollama_base_url, None);
    }

    #[test]
    fn load_reads_partial_existing_file_with_missing_fields_defaulted() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(AppSettings::path(dir.path()), r#"{"ollama_base_url":"http://localhost:11434"}"#).expect("write partial file");

        let loaded = AppSettings::load(dir.path());

        assert_eq!(loaded.ollama_base_url, Some("http://localhost:11434".to_string()));
        assert_eq!(loaded.summary_template, None);
        assert_eq!(loaded.whisper_model_path, None);
        assert_eq!(loaded.exports_root, None);
        assert_eq!(loaded.silence_gate_enabled, None);
        assert_eq!(loaded.microphone_device_id, None);
        assert_eq!(loaded.render_device_id, None);
    }

    #[test]
    fn load_falls_back_to_defaults_on_corrupt_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(AppSettings::path(dir.path()), "not valid json").expect("write corrupt file");

        let loaded = AppSettings::load(dir.path());

        assert_eq!(loaded, AppSettings::default());
    }

    #[test]
    fn path_is_settings_json_under_app_data_dir() {
        let dir = PathBuf::from("/tmp/example-app-data-dir");
        assert_eq!(AppSettings::path(&dir), dir.join("settings.json"));
    }
}
