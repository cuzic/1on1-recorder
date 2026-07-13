//! Converts ScreenCaptureKit's `CMSampleBuffer` presentation timestamps into the
//! `u64` nanosecond values `CapturedFrameRecord::capture_time_ns` needs — the macOS
//! analogue of `capture-windows::timestamp::QpcClock`.
//!
//! **Design note (refines the original plan)**: a `CMTime` is a rational time value
//! by definition — `seconds = value / timescale` — so converting a host-clock-
//! anchored `CMSampleBuffer` presentation timestamp to nanoseconds is a direct
//! rational-to-integer conversion; it does **not** need a separate
//! `mach_timebase_info` tick-to-ns conversion step (that step is only needed when
//! starting from a raw `mach_absolute_time()` tick count, which has no timescale of
//! its own — `CMTime` already carries one). This crate therefore has no `mach2`
//! dependency.
//!
//! The tests below need no TCC grant, no real ScreenCaptureKit/CoreAudio access, and
//! no real audio hardware — they exercise pure arithmetic against fixture values.
//! They still need this *crate* to compile, though, which — like
//! `capture-windows` needing a Windows target — needs a macOS host (or a macOS CI
//! runner with a Swift toolchain, see `Cargo.toml`'s `screencapturekit`/`objc2-*`
//! dependencies); `cargo test -p capture-macos` will not run on this project's
//! Linux dev environment.

/// A `CMTime`-shaped value, kept independent of the `screencapturekit` crate's own
/// `CMTime` type so this conversion's arithmetic can be unit-tested without any
/// ScreenCaptureKit/macOS dependency (see the tests below, which run on any OS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CMTimeLike {
    pub value: i64,
    pub timescale: i32,
}

/// Converts a `CMTime`-shaped rational time value into nanoseconds since whatever
/// epoch the clock it's anchored to uses. Returns `None` for an invalid
/// (non-positive) timescale rather than panicking or dividing by zero — a
/// `CMSampleBuffer` reporting a malformed timestamp shouldn't crash the capture
/// pipeline.
pub fn cmtime_to_ns(time: CMTimeLike) -> Option<u64> {
    if time.timescale <= 0 {
        return None;
    }
    // i128 intermediate avoids overflow for value * 1_000_000_000 well past any
    // value CMTime would realistically carry.
    let ns = (time.value as i128 * 1_000_000_000) / time.timescale as i128;
    u64::try_from(ns).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_whole_seconds() {
        let t = CMTimeLike {
            value: 5,
            timescale: 1,
        };
        assert_eq!(cmtime_to_ns(t), Some(5_000_000_000));
    }

    #[test]
    fn converts_fractional_seconds_at_sample_rate_timescale() {
        // 16000 samples into a 16kHz-timescale clock = exactly 1 second.
        let t = CMTimeLike {
            value: 16_000,
            timescale: 16_000,
        };
        assert_eq!(cmtime_to_ns(t), Some(1_000_000_000));
    }

    #[test]
    fn converts_sub_second_value() {
        // 500ms at a 1000Hz timescale.
        let t = CMTimeLike {
            value: 500,
            timescale: 1_000,
        };
        assert_eq!(cmtime_to_ns(t), Some(500_000_000));
    }

    #[test]
    fn zero_timescale_is_none_not_a_panic() {
        let t = CMTimeLike {
            value: 5,
            timescale: 0,
        };
        assert_eq!(cmtime_to_ns(t), None);
    }

    #[test]
    fn negative_timescale_is_none() {
        let t = CMTimeLike {
            value: 5,
            timescale: -1,
        };
        assert_eq!(cmtime_to_ns(t), None);
    }

    #[test]
    fn negative_value_does_not_panic_and_fails_conversion() {
        // A negative presentation timestamp is malformed for this crate's purposes
        // (host-clock-anchored timestamps should never be negative); make sure it's
        // rejected rather than silently wrapping into a huge u64.
        let t = CMTimeLike {
            value: -1,
            timescale: 1,
        };
        assert_eq!(cmtime_to_ns(t), None);
    }

    #[test]
    fn large_value_does_not_overflow() {
        // ~106 days worth of nanoseconds at a 1e9 timescale — well within i128
        // headroom, exercises the "large but valid" path.
        let t = CMTimeLike {
            value: 9_000_000,
            timescale: 1,
        };
        assert_eq!(cmtime_to_ns(t), Some(9_000_000_000_000_000));
    }
}
