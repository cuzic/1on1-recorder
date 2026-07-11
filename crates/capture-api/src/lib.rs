//! OS-independent building blocks for a resilient audio capture engine.
//!
//! The first piece here is [`rebinding`]: a pure state machine that decides how to
//! respond when a capture stream's underlying device disappears, the system default
//! device changes, or a captured process exits/restarts — without ever calling into
//! WASAPI, PipeWire, CoreAudio, or any other OS API itself. Real backends (Windows,
//! Linux, macOS) execute the [`rebinding::Effect`]s this produces and report back
//! [`rebinding::Observation`]s; the policy is entirely OS-agnostic and can be tested
//! without any real hardware, as the scenario tests demonstrate.
//!
//! A `CaptureAdapter` trait connecting this policy layer to real OS backends is
//! intentionally not defined yet — it will be added once the Windows backend
//! (`capture-windows`) is implemented, so its shape is grounded in an actual
//! implementation rather than designed speculatively.

pub mod rebinding;
