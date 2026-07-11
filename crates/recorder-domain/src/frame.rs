use crate::track::TrackKind;

/// design.md §9.2. `host_time_ns` must already be converted to the OS's monotonic
/// clock domain — never use wall-clock time directly for alignment (see
/// `audio-timeline`, which consumes this via a per-track adapter into its own
/// `AudioPacket`).
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub track: TrackKind,
    pub host_time_ns: u64,
    pub source_time_ns: Option<u64>,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
    pub discontinuity: bool,
}
