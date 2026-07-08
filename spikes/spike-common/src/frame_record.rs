// spike-windows-01-02-detail-design.md §3.2

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamId {
    Mic,
    EndpointLoopback,
    ProcessLoopback,
}

#[derive(Debug, Clone)]
pub struct CapturedFrameRecord {
    pub stream: StreamId,
    /// `WaitForMultipleObjects`が1回戻るごとにインクリメントされる連番。
    /// 同一wakeで複数パケットを排出した場合、それらのレコードは同じ値を持つ。
    pub wake_seq: u64,
    /// パケット(`GetBuffer`呼び出し1回)ごとの連番(0始まり、ストリーム内で単調増加)
    pub packet_seq: u64,
    /// `IAudioCaptureClient::GetBuffer`が返す`pu64QPCPosition`を100ns単位に
    /// 変換した値。プロセス・ストリームをまたいで共通のQPCクロックドメインに属する。
    /// `timestamp_error`がtrueの場合はこの値を信頼しない。
    pub capture_qpc_100ns: u64,
    /// `WaitForMultipleObjects`が戻った時点で別途取得したQPC値(100ns単位)。
    /// 共通タイムライン変換には使わない(§3.2参照)。
    pub wake_qpc_100ns: u64,
    /// `pu64DevicePosition`。ストリーム先頭からの累積オーディオフレーム数。
    pub device_position_frames: u64,
    /// このパケットのフレーム数
    pub frame_count: u32,
    /// `IAudioCaptureClient::GetBuffer`が返す生フラグ
    pub raw_flags: u32,
    pub discontinuity: bool,
    pub silent: bool,
    pub timestamp_error: bool,
    /// キャプチャの世代番号。SPIKE-02でプロセス再アタッチが発生するたびにインクリメントする。
    pub capture_epoch: u64,
    /// SPIKE-02でのみ使用。取得元プロセスのPID(Endpoint Loopback/Micでは`None`)
    pub target_pid: Option<u32>,
}

impl CapturedFrameRecord {
    pub const FLAG_DATA_DISCONTINUITY: u32 = 0x1; // AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY
    pub const FLAG_SILENT: u32 = 0x2; // AUDCLNT_BUFFERFLAGS_SILENT
    pub const FLAG_TIMESTAMP_ERROR: u32 = 0x4; // AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR

    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        stream: StreamId,
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
