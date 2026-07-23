//! Minimal standalone smoke test used by `.github/workflows/macos-build.yml`'s
//! `e2e-best-effort` job: starts a mic-only `SCStream` for a fixed duration,
//! writes every received `f32` PCM sample to a raw output file, then exits.
//!
//! Deliberately bypasses `app-service`'s `MacosSupervisor` (no rebinding FSM, no
//! device pinning) — this only needs to prove "does ScreenCaptureKit actually
//! deliver audio frames to this process at all," which is a strictly lower bar
//! than the full pipeline `app-service` builds on top of it.
//!
//! Usage: `smoke_test --duration-secs 10 --out /tmp/smoke.raw`

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use capture_api::rebinding::BindingKind;
use capture_macos::sc_stream::{ScreenCaptureKitStream, StreamOutputs};
use capture_macos::{spawn_capture_thread, CaptureEvent, StopSignal};

const SAMPLE_RATE_HZ: u32 = 16_000;
const CHANNELS: u16 = 1;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let duration_secs = parse_arg(&args, "--duration-secs").unwrap_or(10);
    let out_path = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "/tmp/capture-macos-smoke-test.raw".to_string());

    println!("capture-macos smoke test: duration={duration_secs}s out={out_path}");

    let filter = match unfiltered_display_filter() {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!("FAIL: could not build content filter: {err}");
            std::process::exit(1);
        }
    };

    let stream = ScreenCaptureKitStream::new(
        filter,
        SAMPLE_RATE_HZ,
        CHANNELS,
        StreamOutputs {
            microphone: true,
            system_audio: false,
        },
        BindingKind::EndpointLoopback, // unused: system_audio is disabled above
        0,
        None,
    );

    let (tx, rx) = crossbeam_channel::unbounded::<CaptureEvent>();
    let stop = Arc::new(StopSignal::new());
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
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    stop.signal();
    let _ = join_handle.join();

    println!(
        "captured {total_samples} samples ({} bytes) to {out_path}",
        total_samples * 4
    );
    if total_samples == 0 {
        eprintln!(
            "FAIL: zero samples captured — likely a TCC permission denial or no input device."
        );
        std::process::exit(1);
    }
}

fn unfiltered_display_filter(
) -> Result<screencapturekit::stream::content_filter::SCContentFilter, capture_macos::CaptureError>
{
    let content = screencapturekit::shareable_content::SCShareableContent::get()
        .map_err(|err| capture_macos::CaptureError::ScreenCaptureKit(err.to_string()))?;
    let display = content.displays().into_iter().next().ok_or_else(|| {
        capture_macos::CaptureError::DeviceNotFound("no display available".to_string())
    })?;
    Ok(capture_macos::app_filter::unfiltered(&display))
}

fn parse_arg(args: &[String], name: &str) -> Option<u64> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}
