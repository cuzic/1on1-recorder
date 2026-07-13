use capture_api::rebinding::BindingKind;

#[derive(Debug, Clone)]
pub struct CapturedFrameRecord {
    pub stream: BindingKind,
    /// Per-buffer sequence, monotonically increasing within a stream starting from 0.
    pub packet_seq: u64,
    /// Host-clock-domain timestamp in nanoseconds, converted from a `CMSampleBuffer`'s
    /// presentation timestamp (`CMTime`) via `mach_timebase_info` — see
    /// `timestamp.rs`. Both `Microphone` and `EndpointLoopback`/`ProcessLoopback`
    /// frames from the same `SCStream` share this one host clock domain, unlike
    /// `capture-windows` where mic and loopback are two independent WASAPI streams
    /// whose QPC readings are only correlated because QPC itself is machine-global.
    pub capture_time_ns: u64,
    /// Cumulative sample count since the start of the stream, accumulated locally
    /// from `CMSampleBufferGetNumSamples`. Unlike `capture-windows`'s
    /// `device_position_frames` (read from `IAudioCaptureClient::GetBuffer`'s
    /// `pu64DevicePosition`, a hardware-reported value), ScreenCaptureKit exposes no
    /// raw device sample counter — this is a *derived* diagnostic value, not a
    /// device-reported one.
    pub device_position_frames: u64,
    pub frame_count: u32,
    pub discontinuity: bool,
    pub silent: bool,
    /// Always `false`: ScreenCaptureKit exposes no raw "timestamp error" flag the
    /// way WASAPI's `AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR` does. Kept for API-shape
    /// parity with `capture_windows::CapturedFrameRecord` so
    /// `app-service`'s frame-collector conversion logic can stay structurally
    /// parallel between the two backends.
    pub timestamp_error: bool,
    /// Capture generation number, incremented every time this stream is rebound
    /// (see `capture_api::rebinding::StreamEpoch`).
    pub capture_epoch: u64,
    /// Only set for process loopback (`None` for endpoint loopback/microphone).
    pub target_pid: Option<u32>,
}

impl CapturedFrameRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        stream: BindingKind,
        packet_seq: u64,
        capture_time_ns: u64,
        device_position_frames: u64,
        frame_count: u32,
        discontinuity: bool,
        silent: bool,
        capture_epoch: u64,
        target_pid: Option<u32>,
    ) -> Self {
        Self {
            stream,
            packet_seq,
            capture_time_ns,
            device_position_frames,
            frame_count,
            discontinuity,
            silent,
            timestamp_error: false,
            capture_epoch,
            target_pid,
        }
    }
}
