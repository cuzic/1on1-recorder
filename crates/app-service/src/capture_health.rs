//! Cross-platform view of each capture track's health, threaded from
//! `capture_api::rebinding`'s per-binding state up through
//! `WindowsSupervisor`/`MacosSupervisor` to `apps/desktop`'s recording screen — so a
//! mic/system-audio binding stuck `Waiting`/`Failed` mid-session shows up as
//! something other than a silently flatlined level meter.
//!
//! Unlike `LevelSnapshot`/`TranscriptionStatus` (duplicated per platform because
//! each is *produced* by a separate platform module gated behind its own
//! supervisor feature), track health carries no platform-specific content — just a
//! retry count or a failure string — so one type covers both platforms with no
//! root-export collision to avoid, and stays available even in the default
//! (no-features) build.

/// See `capture_api::rebinding::BindingHealth`, which this mirrors — kept as a
/// separate type (rather than re-exporting that one) so this crate's public API
/// doesn't require the `windows-supervisor`/`macos-supervisor` feature just to name
/// it, matching `CaptureHealth` below.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TrackHealth {
    #[default]
    Ok,
    Unavailable,
    Retrying {
        attempt: u32,
    },
    Failed {
        reason: String,
    },
}

#[cfg(any(feature = "windows-supervisor", feature = "macos-supervisor"))]
impl From<capture_api::rebinding::BindingHealth> for TrackHealth {
    fn from(health: capture_api::rebinding::BindingHealth) -> Self {
        match health {
            capture_api::rebinding::BindingHealth::Ok => TrackHealth::Ok,
            capture_api::rebinding::BindingHealth::Unavailable => TrackHealth::Unavailable,
            capture_api::rebinding::BindingHealth::Retrying { attempt } => TrackHealth::Retrying { attempt },
            capture_api::rebinding::BindingHealth::Failed { reason } => TrackHealth::Failed { reason },
        }
    }
}

/// Both tracks of one recording session. `self_health` is the microphone binding
/// (`BindingKind::Microphone`), `remote_health` is the system-audio/loopback
/// binding (`BindingKind::EndpointLoopback`) — same Self/Remote naming
/// `LevelSnapshot` already uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureHealth {
    pub self_health: TrackHealth,
    pub remote_health: TrackHealth,
}
