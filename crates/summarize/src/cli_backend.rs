//! Summarizing via the `claude`/`codex` CLIs as subprocesses (#59), instead of an
//! API key through `genai`. Both CLIs authenticate with the user's own OAuth/
//! subscription login (`claude login` / `codex login`), so this path needs no
//! `credential-store` entry at all — the caller only needs to check
//! [`CliBackend::is_available`] before offering it.
//!
//! Both CLIs are non-interactive ("headless") invocations verified by hand against
//! the actually-installed binaries:
//! - `claude --print --output-format json --no-session-persistence --disallowedTools
//!   <tools...> -- <prompt>`, which prints a single JSON object on stdout whose
//!   `"result"` field is the response text. `--bare` must never be added — it
//!   disables OAuth/Keychain auth in favor of requiring an API key, defeating the
//!   point of this module. The `--` separator is required: `--disallowedTools` is a
//!   variadic flag, so without it the prompt string would be swallowed as an
//!   additional (bogus) tool name.
//! - `codex exec -s read-only --skip-git-repo-check --ephemeral -C <dir> -o <file>
//!   -- <prompt>`, which writes only the final response text to `<file>` (`-o`);
//!   stdout is a mixed conversation log not worth parsing.
//!
//! Both are run with their working directory pinned to a fresh [`tempfile::TempDir`]
//! so neither CLI picks up this project's own `CLAUDE.md`/`AGENTS.md`.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use crate::{build_cli_prompt, SummarizeOptions, TranscriptTurn};

/// Tool names `claude --print` is not allowed to invoke while summarizing — this is
/// a text-in/text-out call, so every tool that could read/write files or the
/// network is denied.
const CLAUDE_DISALLOWED_TOOLS: &str = "Bash,Edit,Write,Read,WebFetch,WebSearch,Task,Glob,Grep,NotebookEdit";

/// Which CLI-based summarization backend to use. Distinct from `genai`'s
/// provider/model string dispatch — these two go through a subprocess, not HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliBackend {
    ClaudeCode,
    Codex,
}

impl CliBackend {
    /// The executable name looked up on `PATH`.
    pub fn binary(self) -> &'static str {
        match self {
            CliBackend::ClaudeCode => "claude",
            CliBackend::Codex => "codex",
        }
    }

    /// Whether `binary()` is on `PATH` and runnable, checked via `<binary>
    /// --version`. Doesn't verify the CLI is logged in — a login-required failure
    /// only surfaces once [`summarize_via_cli`] actually runs.
    pub async fn is_available(self) -> bool {
        Command::new(self.binary())
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

impl std::fmt::Display for CliBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.binary())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CliSummarizeError {
    #[error("{0} CLI not found on PATH")]
    NotFound(CliBackend),
    #[error("failed to create a temporary working directory: {0}")]
    TempDir(#[source] std::io::Error),
    #[error("failed to launch {0} CLI: {1}")]
    Spawn(CliBackend, #[source] std::io::Error),
    #[error("{0} CLI exited with {1}: {2}")]
    NonZeroExit(CliBackend, std::process::ExitStatus, String),
    #[error("failed to read {0} CLI output: {1}")]
    ReadOutput(CliBackend, #[source] std::io::Error),
    #[error("failed to parse {0} CLI output: {1}")]
    ParseOutput(CliBackend, String),
    #[error("{0} CLI returned an empty summary")]
    EmptyResponse(CliBackend),
}

/// Summarizes `turns` by shelling out to `backend`'s CLI rather than calling a
/// provider API through `genai`. Builds the same prompt [`crate::summarize`] would
/// (system prompt + rendered transcript, via [`build_cli_prompt`]) but passes it as
/// a single CLI argument, since neither CLI takes separate system/user messages.
pub async fn summarize_via_cli(
    backend: CliBackend,
    turns: &[TranscriptTurn],
    options: &SummarizeOptions,
) -> Result<String, CliSummarizeError> {
    if !backend.is_available().await {
        return Err(CliSummarizeError::NotFound(backend));
    }

    let prompt = build_cli_prompt(turns, options);
    let work_dir = tempfile::TempDir::new().map_err(CliSummarizeError::TempDir)?;
    // `options.model` doubles as the model string for the genai path and the CLI's
    // own `--model`/`-m` alias here — empty means "let the CLI use its own
    // configured default" rather than passing an empty `--model ""`.
    let model = (!options.model.trim().is_empty()).then_some(options.model.as_str());

    match backend {
        CliBackend::ClaudeCode => run_claude(work_dir.path(), &prompt, model).await,
        CliBackend::Codex => run_codex(work_dir.path(), &prompt, model).await,
    }
}

async fn run_claude(work_dir: &Path, prompt: &str, model: Option<&str>) -> Result<String, CliSummarizeError> {
    let mut command = Command::new(CliBackend::ClaudeCode.binary());
    // Without this, dropping the `.output()` future (e.g. the caller's `spawn`ed
    // task getting cancelled) leaves the `claude` subprocess running in the
    // background instead of killing it — Tokio's `Child` doesn't kill on drop by
    // default.
    command.kill_on_drop(true);
    command.current_dir(work_dir).args([
        "--print",
        "--output-format",
        "json",
        "--no-session-persistence",
        "--disallowedTools",
        CLAUDE_DISALLOWED_TOOLS,
    ]);
    if let Some(model) = model {
        command.args(["--model", model]);
    }
    let output = command
        .arg("--")
        .arg(prompt)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| CliSummarizeError::Spawn(CliBackend::ClaudeCode, e))?;

    if !output.status.success() {
        return Err(CliSummarizeError::NonZeroExit(
            CliBackend::ClaudeCode,
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| CliSummarizeError::ParseOutput(CliBackend::ClaudeCode, e.to_string()))?;
    let text = value
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CliSummarizeError::ParseOutput(CliBackend::ClaudeCode, "missing \"result\" field".to_string()))?;

    if text.trim().is_empty() {
        return Err(CliSummarizeError::EmptyResponse(CliBackend::ClaudeCode));
    }
    Ok(text.to_string())
}

async fn run_codex(work_dir: &Path, prompt: &str, model: Option<&str>) -> Result<String, CliSummarizeError> {
    let output_path = work_dir.join("codex-output.txt");

    let mut command = Command::new(CliBackend::Codex.binary());
    // See `run_claude`'s identical `kill_on_drop` for why this is needed.
    command.kill_on_drop(true);
    command
        .current_dir(work_dir)
        .args(["exec", "-s", "read-only", "--skip-git-repo-check", "--ephemeral", "-C"])
        .arg(work_dir)
        .arg("-o")
        .arg(&output_path);
    if let Some(model) = model {
        command.args(["-m", model]);
    }
    let output = command
        .arg("--")
        .arg(prompt)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| CliSummarizeError::Spawn(CliBackend::Codex, e))?;

    if !output.status.success() {
        return Err(CliSummarizeError::NonZeroExit(
            CliBackend::Codex,
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    let text = tokio::fs::read_to_string(&output_path)
        .await
        .map_err(|e| CliSummarizeError::ReadOutput(CliBackend::Codex, e))?;

    if text.trim().is_empty() {
        return Err(CliSummarizeError::EmptyResponse(CliBackend::Codex));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_backend_binary_names() {
        assert_eq!(CliBackend::ClaudeCode.binary(), "claude");
        assert_eq!(CliBackend::Codex.binary(), "codex");
    }

    #[test]
    fn cli_backend_display_matches_binary_name() {
        assert_eq!(CliBackend::ClaudeCode.to_string(), "claude");
        assert_eq!(CliBackend::Codex.to_string(), "codex");
    }

    #[tokio::test]
    async fn is_available_is_false_for_a_nonexistent_binary_by_construction() {
        // Neither `claude` nor `codex` is guaranteed present in every environment
        // this crate builds in (CI, other contributors' machines); the only thing
        // assertable unconditionally is that `is_available` doesn't panic and
        // returns a bool without an installed binary needing to exist.
        let _ = CliBackend::ClaudeCode.is_available().await;
        let _ = CliBackend::Codex.is_available().await;
    }

    // Real subprocess calls against the actually-installed, logged-in `claude`/
    // `codex` CLIs — costs real tokens/time, so `#[ignore]`d. Run explicitly with
    // `cargo test -p summarize -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn summarize_via_cli_claude_code_smoke_test() {
        let turns = vec![TranscriptTurn {
            speaker: Some("Alice".to_string()),
            text: "Let's ship the feature by Friday.".to_string(),
        }];
        let options = SummarizeOptions::new("sonnet");
        let result = summarize_via_cli(CliBackend::ClaudeCode, &turns, &options).await;
        let text = result.expect("claude CLI summarize should succeed");
        assert!(!text.trim().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn summarize_via_cli_codex_smoke_test() {
        let turns = vec![TranscriptTurn {
            speaker: Some("Alice".to_string()),
            text: "Let's ship the feature by Friday.".to_string(),
        }];
        let options = SummarizeOptions::new("gpt-5.5");
        let result = summarize_via_cli(CliBackend::Codex, &turns, &options).await;
        let text = result.expect("codex CLI summarize should succeed");
        assert!(!text.trim().is_empty());
    }
}
