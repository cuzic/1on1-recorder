use crate::error::CaptureError;
use crate::wasapi_common::{init_and_capture, DeviceSelector, WasapiInitParams};
use crate::{CaptureEvent, CaptureExit, CaptureStream, StopSignal};
use capture_api::rebinding::{BindingKind, DeviceRole};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

pub struct MicCaptureStream {
    pub device_id_or_default: String,
    pub role: DeviceRole,
    pub pipeline_drop_counter: Arc<AtomicU64>,
    pub callback_timeout_ms: u32,
    /// Incremented by the caller each time this stream is rebound after a device loss.
    pub capture_epoch: u64,
}

impl CaptureStream for MicCaptureStream {
    fn stream_id(&self) -> BindingKind {
        BindingKind::Microphone
    }

    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, CaptureError> {
        init_and_capture(
            WasapiInitParams {
                device: DeviceSelector::Capture {
                    id_or_default: self.device_id_or_default,
                    role: self.role,
                },
                extra_stream_flags: 0,
                stream_id: BindingKind::Microphone,
                callback_timeout_ms: self.callback_timeout_ms,
                pipeline_drop_counter: self.pipeline_drop_counter,
            },
            tx,
            stop,
            self.capture_epoch,
        )
    }
}
