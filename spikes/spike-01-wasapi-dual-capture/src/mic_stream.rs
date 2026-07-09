// spike-windows-01-02-detail-design.md §4.4

use crate::device_select::DeviceRole;
use crate::wasapi_common::{init_and_capture, DeviceSelector, WasapiInitParams};
use spike_common::frame_record::StreamId;
use spike_common::{CaptureEvent, CaptureExit, CaptureStream, SpikeError, StopSignal};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

pub struct MicCaptureStream {
    pub device_id_or_default: String, // CLIの--mic-deviceそのまま
    pub role: DeviceRole,
    pub pipeline_drop_counter: Arc<AtomicU64>,
    pub callback_timeout_ms: u32,
    /// spike-plan.md SPIKE-09: デバイス消失検出後の再アタッチのたびに
    /// 呼び出し側(main.rs)がインクリメントする。
    pub capture_epoch: u64,
}

impl CaptureStream for MicCaptureStream {
    fn stream_id(&self) -> StreamId {
        StreamId::Mic
    }

    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, SpikeError> {
        init_and_capture(
            WasapiInitParams {
                device: DeviceSelector::Capture {
                    id_or_default: self.device_id_or_default,
                    role: self.role,
                },
                extra_stream_flags: 0,
                stream_id: StreamId::Mic,
                callback_timeout_ms: self.callback_timeout_ms,
                pipeline_drop_counter: self.pipeline_drop_counter,
            },
            tx,
            stop,
            self.capture_epoch,
        )
    }
}
