//! `1on1ctl`: start/stop/check recording on the 1on1 Recorder desktop app
//! (`apps/desktop`) from outside the GUI, regardless of which screen (if any)
//! is showing. Talks to `apps/desktop/src/control_server.rs` over the local
//! socket described by `control_protocol`. If the GUI isn't running at all,
//! this launches it and waits for the control server to come up before
//! sending the command — every invocation blocks until it has the app's real
//! resulting state to print, never fire-and-forget.

use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use control_protocol::{Command, Response, StatusDto};
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::Stream;

const LAUNCH_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(300);

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (command, json) = match parse_args(&mut args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("usage: 1on1ctl [--json] <start|stop|status>");
            return ExitCode::from(2);
        }
    };

    match run(&command) {
        Ok(Response::Ok(status)) => {
            print_status(&status, json);
            ExitCode::SUCCESS
        }
        Ok(Response::Err { message, status }) => {
            eprintln!("error: {message}");
            print_status(&status, json);
            ExitCode::from(1)
        }
        Err(err) => {
            eprintln!("could not reach 1on1 Recorder: {err}");
            ExitCode::from(2)
        }
    }
}

fn parse_args(args: &mut dyn Iterator<Item = String>) -> Result<(Command, bool), String> {
    let mut json = false;
    let mut command = None;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "start" if command.is_none() => command = Some(Command::Start),
            "stop" if command.is_none() => command = Some(Command::Stop),
            "status" if command.is_none() => command = Some(Command::Status),
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    let command = command.ok_or_else(|| "missing subcommand".to_string())?;
    Ok((command, json))
}

fn run(command: &Command) -> std::io::Result<Response> {
    let conn = connect_or_launch()?;
    send_and_receive(conn, command)
}

/// Tries the control socket immediately; if nothing answers, assumes the GUI
/// isn't running, spawns it, and polls the socket until it comes up (or
/// `LAUNCH_WAIT_TIMEOUT` elapses). A stale-but-unremoved control socket file
/// (the macOS case `control_server::bind` otherwise handles) would make the
/// very first `Stream::connect` here fail exactly like "not running" does —
/// that's fine, since spawning the GUI just makes it perform the same
/// stale-socket recovery on its own next `bind()` call.
fn connect_or_launch() -> std::io::Result<Stream> {
    let name = control_protocol::control_socket_name()?;
    if let Ok(conn) = Stream::connect(name.clone()) {
        return Ok(conn);
    }

    let gui_path = locate_gui_binary()?;
    eprintln!("1on1 Recorder is not running; starting {}...", gui_path.display());
    std::process::Command::new(&gui_path).spawn()?;

    let deadline = Instant::now() + LAUNCH_WAIT_TIMEOUT;
    loop {
        match Stream::connect(name.clone()) {
            Ok(conn) => return Ok(conn),
            Err(err) if Instant::now() >= deadline => return Err(err),
            Err(_) => std::thread::sleep(LAUNCH_POLL_INTERVAL),
        }
    }
}

/// The GUI binary is expected to sit next to this CLI binary — true for a
/// Windows portable install (`desktop.exe` alongside `1on1ctl.exe`) and for a
/// macOS `.app`'s `Contents/MacOS/` (where `desktop` and `1on1ctl` would be
/// siblings, if packaged that way — see the project plan for the current
/// packaging status). Not resolved via `PATH`: the GUI isn't meant to be a
/// general-purpose CLI tool someone would put there.
fn locate_gui_binary() -> std::io::Result<std::path::PathBuf> {
    let exe_dir = std::env::current_exe()?.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let name = if cfg!(windows) { "desktop.exe" } else { "desktop" };
    let candidate = exe_dir.join(name);
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("1on1 Recorder is not running and its GUI binary was not found next to 1on1ctl (looked for {})", candidate.display()),
        ))
    }
}

fn send_and_receive(conn: Stream, command: &Command) -> std::io::Result<Response> {
    let mut writer = BufReader::new(conn);
    let mut payload = serde_json::to_string(command)?;
    payload.push('\n');
    writer.get_mut().write_all(payload.as_bytes())?;

    let mut line = String::new();
    writer.read_line(&mut line)?;
    serde_json::from_str(line.trim_end()).map_err(std::io::Error::from)
}

fn print_status(status: &StatusDto, json: bool) {
    if json {
        if let Ok(payload) = serde_json::to_string(status) {
            println!("{payload}");
        }
        return;
    }

    if status.recording {
        println!("recording: {}経過", format_duration(status.elapsed_ms));
        if let Some(id) = &status.last_session_id {
            println!("session: {id}");
        }
        println!("segments: 送信済み {} / 保留 {}", status.uploaded_segments, status.pending_segments);
    } else {
        println!("recording: 停止中");
        if let (Some(id), Some(total_ms)) = (&status.last_session_id, status.last_total_duration_ms) {
            println!("last session: {id} ({})", format_duration(total_ms));
        }
    }
    if let Some(err) = &status.last_error {
        println!("last_error: {err}");
    }
}

fn format_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{:02}:{:02}:{:02}", total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_subcommand() {
        let (cmd, json) = parse_args(&mut vec!["start".to_string()].into_iter()).unwrap();
        assert!(matches!(cmd, Command::Start));
        assert!(!json);

        let (cmd, _) = parse_args(&mut vec!["stop".to_string()].into_iter()).unwrap();
        assert!(matches!(cmd, Command::Stop));

        let (cmd, _) = parse_args(&mut vec!["status".to_string()].into_iter()).unwrap();
        assert!(matches!(cmd, Command::Status));
    }

    #[test]
    fn json_flag_can_appear_before_or_after_subcommand() {
        let (_, json) = parse_args(&mut vec!["--json".to_string(), "status".to_string()].into_iter()).unwrap();
        assert!(json);
        let (_, json) = parse_args(&mut vec!["status".to_string(), "--json".to_string()].into_iter()).unwrap();
        assert!(json);
    }

    #[test]
    fn rejects_missing_or_unknown_subcommand() {
        assert!(parse_args(&mut std::iter::empty()).is_err());
        assert!(parse_args(&mut vec!["bogus".to_string()].into_iter()).is_err());
        // A second subcommand-shaped token is rejected too, not silently ignored.
        assert!(parse_args(&mut vec!["start".to_string(), "stop".to_string()].into_iter()).is_err());
    }

    #[test]
    fn formats_duration_as_hh_mm_ss() {
        assert_eq!(format_duration(0), "00:00:00");
        assert_eq!(format_duration(61_000), "00:01:01");
        assert_eq!(format_duration(3_661_000), "01:01:01");
    }

    #[test]
    fn locate_gui_binary_reports_a_clear_error_when_missing() {
        // `current_exe()` in a `cargo test` run is the test harness binary, which
        // has no `desktop`/`desktop.exe` sibling — exercises the real "not found"
        // path this function takes whenever 1on1ctl is run standalone (e.g. `cargo
        // run -p desktop-cli`) without the GUI binary built alongside it.
        let err = locate_gui_binary().expect_err("no desktop binary sits next to the test harness");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
