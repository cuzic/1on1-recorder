//! Builds `SCContentFilter`s (design.md §5.2 steps 2/3) — the natural home for
//! `BindingKind::ProcessLoopback`, which ScreenCaptureKit's per-app filtering fits
//! more naturally than Windows's approach (WASAPI process-loopback was never ported
//! to `capture-windows`; see `capture-api`'s `BindingKind` doc comment). Task 3
//! decides whether app-filtered system audio defaults to `ProcessLoopback` or
//! `EndpointLoopback` stays the default with filtering as an opt-in.

use screencapturekit::shareable_content::{SCDisplay, SCRunningApplication};
use screencapturekit::stream::content_filter::SCContentFilter;

/// An unfiltered content filter over the given display — all system audio,
/// unfiltered by application. Used for `BindingKind::EndpointLoopback`.
pub fn unfiltered(display: &SCDisplay) -> SCContentFilter {
    SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build()
}

/// A content filter scoped to one running application's audio only. Used for
/// `BindingKind::ProcessLoopback` — the identity carried in
/// `capture_api::rebinding::BindingSelection::Process` should be this
/// application's bundle identifier (see `device_select::RunningApplicationInfo`).
pub fn scoped_to_application(
    display: &SCDisplay,
    application: &SCRunningApplication,
) -> SCContentFilter {
    SCContentFilter::create()
        .with_display(display)
        .with_including_applications(std::slice::from_ref(application), &[])
        .build()
}
