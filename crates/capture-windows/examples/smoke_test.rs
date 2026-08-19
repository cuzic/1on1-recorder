//! Minimal standalone smoke test used by `.github/workflows/windows-app-build.yml`'s
//! `e2e-best-effort` job: starts a mic-only WASAPI capture stream for a fixed
//! duration, writes every received `f32` PCM sample to a raw output file, then
//! exits. Mirrors `crates/capture-macos/examples/smoke_test.rs`'s scope and
//! reasoning exactly (see that file's doc comment) — mic-only, not the dual
//! mic+loopback capture `app-service`'s `WindowsSupervisor` drives, since this only
//! needs to prove "does WASAPI actually deliver capture frames to this process at
//! all," a strictly lower bar than the full pipeline.
//!
//! Deliberately bypasses `app-service`'s `WindowsSupervisor` (no rebinding FSM, no
//! device-change handling) and constructs `MicCaptureStream` directly.
//!
//! Usage: `smoke_test --duration-secs 10 --out /tmp/smoke.raw`

use std::io::Write;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use capture_api::rebinding::DeviceRole;
use capture_windows::mic_stream::MicCaptureStream;
use capture_windows::{spawn_capture_thread, CaptureEvent, StopSignal};

// Matches apps/desktop/src/recording.rs's CALLBACK_TIMEOUT_MS (production value).
const CALLBACK_TIMEOUT_MS: u32 = 500;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let duration_secs = parse_arg(&args, "--duration-secs").unwrap_or(10);
    let out_path = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "smoke_test.raw".to_string());

    println!("capture-windows smoke test: duration={duration_secs}s out={out_path}");

    let stream = MicCaptureStream {
        device_id_or_default: "default".to_string(),
        role: DeviceRole::Console,
        pipeline_drop_counter: Arc::new(AtomicU64::new(0)),
        callback_timeout_ms: CALLBACK_TIMEOUT_MS,
        capture_epoch: 0,
    };

    let (tx, rx) = crossbeam_channel::unbounded::<CaptureEvent>();
    let stop = match StopSignal::new() {
        Ok(stop) => Arc::new(stop),
        Err(err) => {
            eprintln!("FAIL: could not create stop signal: {err}");
            std::process::exit(1);
        }
    };
    let join_handle = spawn_capture_thread(Box::new(stream), tx, stop.clone());

    let mut out_file = match std::fs::File::create(&out_path) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("FAIL: could not create {out_path}: {err}");
            std::process::exit(1);
        }
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(duration_secs);
    let mut total_samples: u64 = 0;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(CaptureEvent::Frame { samples, .. }) => {
                total_samples += samples.len() as u64;
                let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_ne_bytes()).collect();
                if let Err(err) = out_file.write_all(&bytes) {
                    eprintln!("FAIL: write error: {err}");
                    std::process::exit(1);
                }
            }
            Ok(CaptureEvent::StreamError { error, .. }) => {
                eprintln!("FAIL: stream reported an error: {error}");
                std::process::exit(1);
            }
            Ok(CaptureEvent::StreamStalled { error, .. }) => {
                // Non-fatal (see `CaptureEvent::StreamStalled`'s doc comment — the
                // capture thread is still running) but worth surfacing here: this
                // smoke test previously treated *any* callback timeout as a hard
                // `StreamError` failure before `StreamStalled` existed as a
                // separate, non-fatal variant. Print it instead of silently
                // dropping it into the wildcard arm below, so a stall is still
                // visible in this test's output even though it no longer aborts.
                eprintln!("WARN: stream stalled (worker still running): {error}");
            }
            Ok(CaptureEvent::StreamStarted { device_friendly_name, .. }) => {
                println!("stream started on device: {device_friendly_name}");
            }
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = stop.signal();
    let _ = join_handle.join();

    println!("captured {total_samples} samples total, wrote to {out_path}");
    if total_samples == 0 {
        eprintln!("FAIL: no samples were captured");
        std::process::exit(1);
    }
}

fn parse_arg(args: &[String], name: &str) -> Option<u64> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}
