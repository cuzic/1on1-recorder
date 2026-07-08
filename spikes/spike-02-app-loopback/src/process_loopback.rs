// spike-windows-01-02-detail-design.md §5.4/§5.6
//
// これが本スパイクの最大の不確実点(spike-plan.md記載どおり)。
// この関数群はcapture MTAスレッド内(ProcessLoopbackStream::run)から
// 呼ばれ、IAudioClientをこの関数の外へ持ち出した後もスレッドをまたがない
// (P0-3)。

use crate::completion_handler::CompletionHandler;
use spike_common::frame_record::StreamId;
use spike_common::{AudioFormatInfo, CaptureEvent, CaptureExit, SpikeError, StopSignal};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
// cargo checkで実際に検出: IUnknown::cast()はwindows_core::Interfaceトレイトの
// メソッドであり、トレイトをスコープに入れないと呼び出せない。
use windows::core::{IUnknown, Interface};
use windows::Win32::Media::Audio::{
    IAudioClient, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProcessLoopbackMode {
    Include,
    Exclude,
}

fn build_fixed_format_48k_stereo_f32() -> windows::Win32::Media::Audio::WAVEFORMATEX {
    todo!("§5.6: 48kHz/2ch/float32のWAVEFORMATEX(EXTENSIBLE)を構築する")
}

/// 診断用オプション経路(P0-4)のための「保留中アクティベーション置き場」。
/// hard_timeout: None(既定)を使う限りここへは何も積まれない。
fn park_pending_activation(
    _operation: Option<windows::Win32::Media::Audio::IActivateAudioInterfaceAsyncOperation>,
    _handler: windows::Win32::Media::Audio::IActivateAudioInterfaceCompletionHandler,
    _expired: Arc<AtomicBool>,
) {
    todo!("§5.4: 完了ハンドラが実際に呼ばれるまでoperation/handlerを生存させる置き場を実装する")
}

/// タイムアウトの扱い: 既定ではハードタイムアウトを設けず、recv()で完了まで
/// 無条件にブロックする。診断目的でハードタイムアウトを使いたい場合のみ
/// hard_timeoutにSomeを渡す(§5.4参照)。
pub fn activate_process_loopback_client(
    target_pid: u32,
    mode: ProcessLoopbackMode,
    hard_timeout: Option<std::time::Duration>,
) -> Result<IAudioClient, SpikeError> {
    // 1. AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS を構築
    // TODO(§5.4/§7-1,2): windows crateの実際のAPIパスを確認してから実装する。
    //   AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: target_pid, ProcessLoopbackMode: ... }
    //   AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, ... }
    //   PROPVARIANT(VT_BLOB)へ詰める

    let expired = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<windows::core::Result<IUnknown>>();
    let _handler_impl = CompletionHandler::new(tx, expired.clone());
    // let handler: IActivateAudioInterfaceCompletionHandler = _handler_impl.into();

    // 4. 呼び出し
    // TODO(§5.4): unsafe { ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&prop), &handler, &mut operation)?; }

    match hard_timeout {
        None => {
            let unknown = rx.recv().map_err(|_| SpikeError::ActivationChannelClosed)??;
            let audio_client: IAudioClient = unknown.cast()?;
            Ok(audio_client)
        }
        Some(timeout) => match rx.recv_timeout(timeout) {
            Ok(result) => Ok(result?.cast()?),
            Err(_) => {
                expired.store(true, Ordering::SeqCst);
                // park_pending_activation(operation, handler, expired.clone());
                Err(SpikeError::ActivationTimeout(timeout))
            }
        },
    }
}

/// 「Activate→Initialize」を1つの再試行可能な単位として扱う。GetMixFormat経路が
/// 失敗した場合、**同じクライアントを使い回さず**、新しくActivateし直した
/// クライアントで固定形式+AUTOCONVERTPCMを試す(P0-2)。
fn activate_and_initialize_with_retry(
    target_pid: u32,
    mode: ProcessLoopbackMode,
    activation_hard_timeout: Option<std::time::Duration>,
) -> Result<(IAudioClient, AudioFormatInfo), SpikeError> {
    // 試行1: GetMixFormat経路
    let client_a = activate_process_loopback_client(target_pid, mode, activation_hard_timeout)?;
    let mix_format_result = unsafe { client_a.GetMixFormat() };
    if let Ok(fmt) = mix_format_result {
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let init_ok = unsafe {
            client_a
                .Initialize(
                    windows::Win32::Media::Audio::AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    0,
                    0,
                    fmt,
                    None,
                )
                .is_ok()
        };
        if init_ok {
            let format_info = unsafe { AudioFormatInfo::from_waveformatex(&*fmt) };
            return Ok((client_a, format_info));
        }
    }
    // client_aはInitialize未実施または失敗済み。ここで破棄し、二度とこの
    // オブジェクトへInitializeを呼ばない。
    drop(client_a);

    // 試行2: 新しくActivateし直し、固定形式+AUTOCONVERTPCMを試す。
    let client_b = activate_process_loopback_client(target_pid, mode, activation_hard_timeout)?;
    let fixed_format = build_fixed_format_48k_stereo_f32();
    let fixed_flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe {
        client_b.Initialize(
            windows::Win32::Media::Audio::AUDCLNT_SHAREMODE_SHARED,
            fixed_flags,
            0,
            0,
            &fixed_format,
            None,
        )?;
    }
    let format_info = AudioFormatInfo::from_waveformatex(&fixed_format);
    Ok((client_b, format_info))
}

pub struct ProcessLoopbackStream {
    pub target_pid: u32,
    pub mode: ProcessLoopbackMode,
    pub capture_epoch: u64,
    /// §5.4参照。既定はNone(タイムアウトなし)。
    pub activation_hard_timeout: Option<std::time::Duration>,
    pub pipeline_drop_counter: Arc<AtomicU64>,
}

impl spike_common::CaptureStream for ProcessLoopbackStream {
    fn stream_id(&self) -> StreamId {
        StreamId::ProcessLoopback
    }

    fn run(
        self: Box<Self>,
        tx: &crossbeam_channel::Sender<CaptureEvent>,
        stop: &StopSignal,
    ) -> Result<CaptureExit, SpikeError> {
        let _com = spike_common::com_guard::ComApartment::new_mta()?;

        let (audio_client, format_info) =
            activate_and_initialize_with_retry(self.target_pid, self.mode, self.activation_hard_timeout)?;

        let capture_client: windows::Win32::Media::Audio::IAudioCaptureClient =
            unsafe { audio_client.GetService()? };

        // SPIKE-01の共通ループ本体(wasapi_common::run_capture_loop相当)と
        // 同一のロジックをここで使う。spike-01/02間でのコード共有は§10の
        // 推奨実装順序に従い、両方が動いた後にspike-commonへ抽出する。
        todo!("§5.6: run_capture_loop(audio_client, capture_client, StreamId::ProcessLoopback, Some(self.target_pid), self.capture_epoch, format_info, self.pipeline_drop_counter, tx, stop)")
    }
}
