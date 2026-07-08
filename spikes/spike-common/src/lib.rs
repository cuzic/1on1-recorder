// spike-windows-01-02-detail-design.md §3

pub mod analyze;
pub mod com_guard;
pub mod csv_log;
pub mod error;
pub mod frame_record;
pub mod mmcss;
pub mod os_check;
pub mod timestamp;
pub mod wav_writer;

pub use error::SpikeError;
pub use frame_record::{CapturedFrameRecord, StreamId};

use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::Foundation::HANDLE;

pub enum CaptureEvent {
    Frame {
        record: CapturedFrameRecord,
        samples: Vec<f32>,
    },
    StreamStarted {
        stream: StreamId,
        format: AudioFormatInfo,
        qpc_freq_hz: u64,
    },
    StreamError {
        stream: StreamId,
        error: String,
    },
    /// mmcss_applied: このストリームのキャプチャスレッドでMMCSS登録が
    /// 成功したか(§3.9)。Aggregatorはこの値をStreamStatsへ記録し、
    /// summary.jsonのmmcss_appliedへ反映する。
    StreamStopped {
        stream: StreamId,
        exit: CaptureExit,
        mmcss_applied: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureExit {
    StoppedByRequest,
    DeviceLost,
}

/// WAVEFORMATEXTENSIBLEを安全に解釈するための情報。
#[derive(Debug, Clone)]
pub struct AudioFormatInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub is_float: bool,
    /// WAVEFORMATEX::wFormatTag(例: WAVE_FORMAT_PCM, WAVE_FORMAT_IEEE_FLOAT,
    /// WAVE_FORMAT_EXTENSIBLE)
    pub format_tag: u16,
    /// wFormatTag == WAVE_FORMAT_EXTENSIBLEの場合のSubFormat GUID。
    pub sub_format: Option<windows::core::GUID>,
    pub block_align: u16,
    /// WAVEFORMATEXTENSIBLE::Samples.wValidBitsPerSample。
    pub valid_bits_per_sample: u16,
    /// WAVEFORMATEXTENSIBLE::dwChannelMask。
    pub channel_mask: u32,
    pub bytes_per_sample: u16,
}

impl AudioFormatInfo {
    /// GetMixFormat/固定フォーマット双方から生成する。wFormatTagが
    /// WAVE_FORMAT_EXTENSIBLEかどうかでWAVEFORMATEXTENSIBLEとして
    /// 再解釈するかを分岐する。
    pub fn from_waveformatex(wfx: &WAVEFORMATEX) -> Self {
        todo!("§3.8: WAVEFORMATEX/WAVEFORMATEXTENSIBLEからAudioFormatInfoを構築する")
    }
}

/// IAudioClient::GetMixFormatが返す*mut WAVEFORMATEXをラップするRAII型。
/// 呼び出し側がCoTaskMemFreeで解放する責務を負う。
pub struct WaveFormatBox {
    ptr: *mut WAVEFORMATEX,
}

impl WaveFormatBox {
    pub fn from_raw(ptr: *mut WAVEFORMATEX) -> Self {
        Self { ptr }
    }

    pub fn as_ref(&self) -> &WAVEFORMATEX {
        unsafe { &*self.ptr }
    }
}

impl Drop for WaveFormatBox {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.ptr as *const _)) };
    }
}

/// 停止通知は手動リセットのWin32イベントオブジェクトで行う。キャプチャループは
/// WaitForMultipleObjectsで[audio_ready_event, stop_event]を同時に待つため、
/// stopがシグナルされた時点で即座にループを抜けられる(§3.8)。
pub struct StopSignal {
    event: HANDLE,
}

// HANDLE(*mut c_void)はデフォルトでSend/Syncを持たないため、Arc<StopSignal>を
// スレッド間で共有しようとするとコンパイルエラーになる(cargo checkで実際に
// 検出した)。Win32のイベントオブジェクトへの参照は、SetEvent/CloseHandle/
// WaitForMultipleObjectsのいずれもスレッドを問わず安全に呼べるため、
// unsafe impl Send/Syncで明示的に許可する。
unsafe impl Send for StopSignal {}
unsafe impl Sync for StopSignal {}

impl StopSignal {
    pub fn new() -> windows::core::Result<Self> {
        todo!("§3.8: CreateEventW(None, manual_reset=true, initial=false, None)")
    }

    /// SetEvent。以後handle()を待っているすべてのスレッドが即座に解除される。
    pub fn signal(&self) -> windows::core::Result<()> {
        todo!("§3.8: SetEvent(self.event)")
    }

    pub fn handle(&self) -> HANDLE {
        self.event
    }
}

impl Drop for StopSignal {
    fn drop(&mut self) {
        // TODO(§3.8): windows::Win32::Foundation::CloseHandle(self.event)
    }
}

pub trait CaptureStream: Send {
    fn stream_id(&self) -> StreamId;

    /// 呼び出しスレッド内でブロッキングし、stopがシグナルされるか回復不能な
    /// エラーが起きるまでキャプチャを継続する。
    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, SpikeError>;
}

/// spawn_capture_threadが返すJoinHandleの戻り値。main.rs(§5.7)は共有チャネル
/// 経由のCaptureEvent::StreamStoppedではなく、このJoinHandle::join()の戻り値を
/// 再アタッチ制御の正とする。
pub enum CaptureThreadOutcome {
    Stopped { exit: CaptureExit, mmcss_applied: bool },
    Errored { error: SpikeError, mmcss_applied: bool },
}

pub fn spawn_capture_thread(
    stream: Box<dyn CaptureStream>,
    tx: crossbeam_channel::Sender<CaptureEvent>,
    stop: std::sync::Arc<StopSignal>,
) -> std::thread::JoinHandle<CaptureThreadOutcome> {
    let stream_id = stream.stream_id();
    std::thread::Builder::new()
        .name(format!("capture-{:?}", stream_id))
        .spawn(move || {
            let (mmcss_applied, run_result) =
                mmcss::with_pro_audio_priority(|| stream.run(&tx, &stop));

            match run_result {
                Ok(exit) => {
                    let _ = tx.send(CaptureEvent::StreamStopped {
                        stream: stream_id,
                        exit,
                        mmcss_applied,
                    });
                    CaptureThreadOutcome::Stopped { exit, mmcss_applied }
                }
                Err(e) => {
                    let _ = tx.send(CaptureEvent::StreamError {
                        stream: stream_id,
                        error: e.to_string(),
                    });
                    CaptureThreadOutcome::Errored {
                        error: e,
                        mmcss_applied,
                    }
                }
            }
        })
        .expect("failed to spawn capture thread")
}
