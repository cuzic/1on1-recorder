use crate::error::CaptureError;
use crate::wasapi_common::{init_and_capture, DeviceSelector, WasapiInitParams};
use crate::{CaptureEvent, CaptureExit, CaptureStream, StopSignal};
use capture_api::rebinding::{BindingKind, DeviceRole};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_LOOPBACK;

pub struct EndpointLoopbackStream {
    /// `"default"` activates the current default render (playback) endpoint.
    pub device_id_or_default: String,
    pub role: DeviceRole,
    pub pipeline_drop_counter: Arc<AtomicU64>,
    pub callback_timeout_ms: u32,
    /// Incremented by the caller each time this stream is rebound after a device loss.
    pub capture_epoch: u64,
}

impl CaptureStream for EndpointLoopbackStream {
    fn stream_id(&self) -> BindingKind {
        BindingKind::EndpointLoopback
    }

    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, CaptureError> {
        init_and_capture(
            WasapiInitParams {
                device: DeviceSelector::Render {
                    id_or_default: self.device_id_or_default,
                    role: self.role,
                },
                extra_stream_flags: AUDCLNT_STREAMFLAGS_LOOPBACK,
                stream_id: BindingKind::EndpointLoopback,
                callback_timeout_ms: self.callback_timeout_ms,
                pipeline_drop_counter: self.pipeline_drop_counter,
            },
            tx,
            stop,
            self.capture_epoch,
        )
    }
}
