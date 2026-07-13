//! TCC (Transparency, Consent, and Control) permission handling.
//!
//! design.md §5.2 requires two *separate* grants: Screen & System Audio Recording
//! (covers `SCStreamOutputType::Audio`, and ScreenCaptureKit access in general) and
//! Microphone (covers `SCStreamOutputType::Microphone`). ScreenCaptureKit itself
//! exposes no microphone-specific preflight API (that's an AVFoundation/`AVCaptureDevice`
//! concern) — rather than add a second Objective-C binding dependency just for a
//! preflight check, microphone permission is discovered lazily: attempt to start the
//! stream with microphone capture enabled, and translate the resulting
//! `SCStreamErrorCode` into `CaptureError::PermissionDenied { service: Microphone }`
//! if it indicates a TCC denial (see `sc_stream.rs`). This mirrors how
//! `capture-windows` doesn't preflight WASAPI permissions either — Windows'
//! Microphone privacy setting is discovered the same way, via a failed `Initialize`
//! call.

use crate::error::{CaptureError, TccService};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    Granted,
    Denied,
    /// The user hasn't been asked yet — on macOS this typically means the first
    /// capture attempt will trigger the OS's own permission prompt.
    NotDetermined,
}

/// Preflights Screen & System Audio Recording access without prompting, via
/// `SCShareableContent::get()` — per design.md §5.2 and the research backing this
/// crate's design doc, this call fails (rather than silently returning empty
/// content) when the permission hasn't been granted, so its success/failure is a
/// reasonable proxy for a dedicated preflight API. **Not yet verified against a real
/// build** — the exact error variant `screencapturekit` surfaces for a TCC denial
/// here needs confirming on first real macOS run; the classification below is
/// best-effort from documentation.
pub fn check_screen_recording_access() -> PermissionStatus {
    match screencapturekit::shareable_content::SCShareableContent::get() {
        Ok(_) => PermissionStatus::Granted,
        // TODO(verify on real build): distinguish "denied" from "not determined"
        // once the crate's actual error shape for this call is known — for now,
        // any failure is treated as Denied since that's the safer default (it
        // surfaces a "grant permission" prompt rather than silently proceeding).
        Err(_) => PermissionStatus::Denied,
    }
}

/// Translates a capture-start failure into a `CaptureError::PermissionDenied` when
/// it looks like a TCC denial for `service`, or passes the original error through
/// otherwise. Centralizes the "was this a permission problem?" judgment call in one
/// place rather than duplicating it at every `sc_stream.rs` call site.
pub fn classify_stream_start_error(
    service: TccService,
    raw_error: impl std::fmt::Display,
) -> CaptureError {
    let message = raw_error.to_string();
    // TODO(verify on real build): match on the real SCStreamErrorCode variant name
    // once confirmed, instead of a substring heuristic. Kept as a heuristic for now
    // so this compiles against best-effort documentation research rather than a
    // guessed-at enum variant that might not exist.
    if message.to_lowercase().contains("permission") || message.to_lowercase().contains("denied") {
        CaptureError::PermissionDenied { service }
    } else {
        CaptureError::ScreenCaptureKit(message)
    }
}
