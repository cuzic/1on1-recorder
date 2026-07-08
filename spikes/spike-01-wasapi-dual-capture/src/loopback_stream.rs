// spike-windows-01-02-detail-design.md §4.5

use crate::device_select::DeviceRole;
use crate::wasapi_common::{init_and_capture, DeviceSelector, WasapiInitParams};
use spike_common::frame_record::StreamId;
use spike_common::{CaptureEvent, CaptureExit, CaptureStream, SpikeError, StopSignal};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_LOOPBACK;

pub struct EndpointLoopbackStream {
    /// CLIの--render-deviceそのまま。"render"(既定の再生エンドポイント)をActivateする
    pub device_id_or_default: String,
    pub role: DeviceRole,
    pub pipeline_drop_counter: Arc<AtomicU64>,
}

impl CaptureStream for EndpointLoopbackStream {
    fn stream_id(&self) -> StreamId {
        StreamId::EndpointLoopback
    }

    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, SpikeError> {
        init_and_capture(
            WasapiInitParams {
                device: DeviceSelector::Render {
                    id_or_default: self.device_id_or_default,
                    role: self.role,
                },
                extra_stream_flags: AUDCLNT_STREAMFLAGS_LOOPBACK,
                stream_id: StreamId::EndpointLoopback,
                callback_timeout_ms: 2000,
                pipeline_drop_counter: self.pipeline_drop_counter,
            },
            tx,
            stop,
            /* capture_epoch */ 0,
        )
    }
}
