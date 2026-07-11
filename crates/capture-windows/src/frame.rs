use capture_api::rebinding::BindingKind;

#[derive(Debug, Clone)]
pub struct CapturedFrameRecord {
    pub stream: BindingKind,
    /// Incremented once per `WaitForMultipleObjects` wake. Multiple packets drained
    /// from the same wake share this value.
    pub wake_seq: u64,
    /// Per-packet sequence (one `GetBuffer` call), monotonically increasing within a
    /// stream starting from 0.
    pub packet_seq: u64,
    /// `IAudioCaptureClient::GetBuffer`'s `pu64QPCPosition`, converted to 100ns units.
    /// Shares a common QPC clock domain across processes and streams. Not trustworthy
    /// when `timestamp_error` is true.
    pub capture_qpc_100ns: u64,
    /// A separately-sampled QPC value (100ns units) taken when `WaitForMultipleObjects`
    /// returned. Not used for timeline placement.
    pub wake_qpc_100ns: u64,
    /// `pu64DevicePosition`: cumulative audio frames since the start of the stream.
    pub device_position_frames: u64,
    pub frame_count: u32,
    /// The raw flags returned by `IAudioCaptureClient::GetBuffer`.
    pub raw_flags: u32,
    pub discontinuity: bool,
    pub silent: bool,
    pub timestamp_error: bool,
    /// Capture generation number, incremented every time this stream is rebound
    /// (see `capture_api::rebinding::StreamEpoch`).
    pub capture_epoch: u64,
    /// Only set for process loopback (`None` for endpoint loopback/microphone).
    pub target_pid: Option<u32>,
}

impl CapturedFrameRecord {
    pub const FLAG_DATA_DISCONTINUITY: u32 = 0x1; // AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY
    pub const FLAG_SILENT: u32 = 0x2; // AUDCLNT_BUFFERFLAGS_SILENT
    pub const FLAG_TIMESTAMP_ERROR: u32 = 0x4; // AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR

    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        stream: BindingKind,
        wake_seq: u64,
        packet_seq: u64,
        wake_qpc_100ns: u64,
        device_position_frames: u64,
        capture_qpc_100ns: u64,
        frame_count: u32,
        raw_flags: u32,
        capture_epoch: u64,
        target_pid: Option<u32>,
    ) -> Self {
        Self {
            stream,
            wake_seq,
            packet_seq,
            capture_qpc_100ns,
            wake_qpc_100ns,
            device_position_frames,
            frame_count,
            raw_flags,
            discontinuity: raw_flags & Self::FLAG_DATA_DISCONTINUITY != 0,
            silent: raw_flags & Self::FLAG_SILENT != 0,
            timestamp_error: raw_flags & Self::FLAG_TIMESTAMP_ERROR != 0,
            capture_epoch,
            target_pid,
        }
    }
}
