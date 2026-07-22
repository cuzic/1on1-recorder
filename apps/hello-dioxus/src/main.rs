//! Minimal dioxus-desktop smoke test. No app-specific state, no tray icon, no
//! SessionStore/CredentialStore — if this doesn't show a window either, the
//! problem is in the dioxus-desktop/wry/WebView2 stack itself, not in the main
//! app's startup code. Logs to a file next to the exe (not stdout — a
//! GUI-subsystem exe launched by double-click has no console to write to, and
//! even launched from a terminal its stdio may not be what you expect) so a
//! panic before any window appears is still visible after the fact.

use std::io::Write;

use dioxus::prelude::*;

fn log_path() -> std::path::PathBuf {
    std::env::current_exe()
        .map(|p| p.with_file_name("hello-dioxus.log"))
        .unwrap_or_else(|_| std::path::PathBuf::from("hello-dioxus.log"))
}

fn log_line(msg: &str) {
    let path = log_path();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {msg}", chrono_like_timestamp());
    }
}

/// Avoids pulling in `chrono` just for a log timestamp in this throwaway smoke test.
fn chrono_like_timestamp() -> String {
    format!("{:?}", std::time::SystemTime::now())
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        log_line(&format!("PANIC: {info}"));
    }));

    log_line("main() start");
    log_line(&format!("log file path: {}", log_path().display()));

    log_line("calling dioxus::launch...");
    dioxus::launch(app);
    // If dioxus::launch ever returns instead of running the event loop forever,
    // that's itself informative — normally the process just runs until the
    // window is closed.
    log_line("dioxus::launch returned (unexpected — the app usually runs until closed)");
}

fn app() -> Element {
    log_line("app() component rendering");
    rsx! {
        div {
            style: "display: flex; align-items: center; justify-content: center; height: 100vh; font-family: sans-serif; font-size: 2rem;",
            "Hello, world! (dioxus-desktop smoke test)"
        }
    }
}
