// spike-windows-01-02-detail-design.md §3

pub mod aggregator;
pub mod analyze;
pub mod capture_loop;
pub mod com_guard;
pub mod csv_log;
pub mod device_watch;
pub mod error;
pub mod frame_record;
pub mod jsonl_log;
pub mod mmcss;
pub mod os_check;
pub mod report;
pub mod timestamp;
pub mod wav_writer;

pub use capture_loop::run_capture_loop;

pub use error::SpikeError;
pub use frame_record::{CapturedFrameRecord, StreamId};

use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::Foundation::HANDLE;

/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT。windowsクレートの生成コードには
/// (KSDATAFORMAT_SUBTYPE_PCMと異なり)このGUIDが含まれていないため、
/// ks.h/mmreg.hで定義された固定値をそのまま定数化する。
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

pub enum CaptureEvent {
    Frame {
        record: CapturedFrameRecord,
        samples: Vec<f32>,
    },
    StreamStarted {
        stream: StreamId,
        format: AudioFormatInfo,
        qpc_freq_hz: u64,
        /// 実際に解決されたIMMDevice::GetId()。summary.json(§4.8)のdevicesブロックへ
        /// 記録し、"default"解決時にどのデバイスが実際に使われたかを後から検証できる
        /// ようにする(§4.3)。
        device_id: String,
        device_friendly_name: String,
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
    /// Process Loopbackで対象アプリが無音の間、通知が来ずタイムアウトを
    /// 繰り返した回数(§4.4/§5.9)。エラーではない。capture_loop::run_capture_loop
    /// がループを抜ける直前に一度だけ送る。
    IdleTimeoutObserved {
        stream: StreamId,
        idle_timeout_count: u64,
    },
    /// SPIKE-09: `IAudioSessionEvents::OnSessionDisconnected`を観測した。
    /// `reason_raw`は`AudioSessionDisconnectReason`の生値(DeviceRemoval=0,
    /// ServerShutdown=1, FormatChanged=2, SessionLogoff=3,
    /// SessionDisconnected=4, ExclusiveModeOverride=5)。COM型を直接
    /// スレッド間で持ち回さないため整数のまま送る。
    SessionDisconnected {
        stream: StreamId,
        reason_raw: i32,
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
        let block_align = wfx.nBlockAlign;
        let bytes_per_sample = if wfx.nChannels > 0 {
            block_align / wfx.nChannels
        } else {
            wfx.wBitsPerSample / 8
        };

        // WAVE_FORMAT_EXTENSIBLEの場合のみ、cbSizeがWAVEFORMATEXTENSIBLE分の
        // 追加フィールドを含んでいることを確認したうえで再解釈する。cbSizeが
        // 不足している(壊れたフォーマット記述)場合は非EXTENSIBLE扱いにフォール
        // バックし、パニックを避ける。
        let extensible_extra_size =
            std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>();
        if wfx.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE
            && wfx.cbSize as usize >= extensible_extra_size
        {
            // WAVEFORMATEXは`WAVEFORMATEXTENSIBLE`の先頭フィールド(Format)と
            // レイアウトが一致するため、cbSizeで安全性を確認したうえで
            // 同じメモリを`WAVEFORMATEXTENSIBLE`として再解釈してよい。ただし
            // 元のWAVEFORMATEXが4byteアライメントを保証しないため、参照を
            // 取らずread_unalignedでスタックへコピーしてから読む
            // (`&*ptr`でGUIDフィールドへの参照を作るとE0793で弾かれる)。
            let ext = unsafe {
                std::ptr::read_unaligned(wfx as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE)
            };
            let valid_bits_per_sample = unsafe { ext.Samples.wValidBitsPerSample };
            // WAVEFORMATEXTENSIBLEはpacked structのため、フィールドへの参照は
            // (コピーであっても)作れない。値をローカル変数へコピーしてから比較する。
            let sub_format = ext.SubFormat;
            Self {
                sample_rate: wfx.nSamplesPerSec,
                channels: wfx.nChannels,
                bits_per_sample: wfx.wBitsPerSample,
                is_float: sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
                format_tag: wfx.wFormatTag,
                sub_format: Some(sub_format),
                block_align,
                valid_bits_per_sample,
                channel_mask: ext.dwChannelMask,
                bytes_per_sample,
            }
        } else {
            Self {
                sample_rate: wfx.nSamplesPerSec,
                channels: wfx.nChannels,
                bits_per_sample: wfx.wBitsPerSample,
                is_float: wfx.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT,
                format_tag: wfx.wFormatTag,
                sub_format: None,
                block_align,
                valid_bits_per_sample: wfx.wBitsPerSample,
                channel_mask: 0,
                bytes_per_sample,
            }
        }
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
        // manual_reset=true, initial_state=false, 無名イベント。
        let event = unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None)? };
        Ok(Self { event })
    }

    /// SetEvent。以後handle()を待っているすべてのスレッドが即座に解除される。
    pub fn signal(&self) -> windows::core::Result<()> {
        unsafe { windows::Win32::System::Threading::SetEvent(self.event) }
    }

    pub fn handle(&self) -> HANDLE {
        self.event
    }
}

impl Drop for StopSignal {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.event) };
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
