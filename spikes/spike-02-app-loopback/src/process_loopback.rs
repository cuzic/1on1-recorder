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
use std::sync::{Arc, Mutex, OnceLock};
// cargo checkで実際に検出: IUnknown::cast()はwindows_core::Interfaceトレイトの
// メソッドであり、トレイトをスコープに入れないと呼び出せない。
use windows::core::{IUnknown, Interface};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IAudioClient, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProcessLoopbackMode {
    Include,
    Exclude,
}

/// AUTOCONVERTPCM経路(試行2)向けの固定フォーマット。プレーンな`WAVEFORMATEX`
/// (`WAVE_FORMAT_IEEE_FLOAT`)で十分で、`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`が
/// エンジン側のリサンプル/チャンネル変換を担うため、`WAVEFORMATEXTENSIBLE`まで
/// 組み立てる必要はない(診断用フォールバック経路のための簡略化)。
fn build_fixed_format_48k_stereo_f32() -> windows::Win32::Media::Audio::WAVEFORMATEX {
    use windows::Win32::Media::Audio::WAVEFORMATEX;
    use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;

    let channels: u16 = 2;
    let bits_per_sample: u16 = 32;
    let sample_rate: u32 = 48_000;
    let block_align: u16 = channels * (bits_per_sample / 8);
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: sample_rate * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: bits_per_sample,
        cbSize: 0,
    }
}

/// `AUDIOCLIENT_ACTIVATION_PARAMS`をVT_BLOBのPROPVARIANTへ詰める。
/// `ActivateAudioInterfaceAsync`は呼び出し中に同期的にblobの内容を読み取る
/// (完了ハンドラ経由の非同期部分はインターフェース取得のみ)ため、
/// `activation_params`はこの関数の外側(呼び出し元のスタックフレーム)で
/// 生存していれば十分で、ヒープへコピーする必要はない。
///
/// 安全性の注意: `windows_core::PROPVARIANT`のDrop実装は`PropVariantClear`を
/// 呼び、VT_BLOBの場合`blob.pBlobData`を`CoTaskMemFree`しようとする。しかし
/// ここで指すのはスタック上の値であり、CoTaskMemAllocされたメモリではない
/// ため、Dropを走らせると未定義動作になる。呼び出し側(`activate_process_loopback_client`)
/// は`ActivateAudioInterfaceAsync`呼び出し直後に`std::mem::forget`でDropを
/// 抑止すること。
fn make_blob_propvariant(
    activation_params: &AUDIOCLIENT_ACTIVATION_PARAMS,
) -> windows::core::PROPVARIANT {
    use windows::core::imp::{
        BLOB, PROPVARIANT as RawPropVariant, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
    };

    let raw = RawPropVariant {
        Anonymous: PROPVARIANT_0 {
            Anonymous: PROPVARIANT_0_0 {
                vt: windows::Win32::System::Variant::VT_BLOB.0,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: activation_params as *const AUDIOCLIENT_ACTIVATION_PARAMS
                            as *mut u8,
                    },
                },
            },
        },
    };
    unsafe { windows::core::PROPVARIANT::from_raw(raw) }
}

/// 診断用オプション経路(P0-4)のための「保留中アクティベーション置き場」。
/// hard_timeout: None(既定)を使う限りここへは何も積まれない。
///
/// `operation`/`handler`はwindows-rsの生成型で、生ポインタ(AddRef/Release管理
/// のCOM参照)の保持のみを行う。ここでは呼び出し元スレッドから登録されたあと
/// 二度とこのスレッドから触らない(いずれかの時点でOS側のコールバックスレッドが
/// `ActivateCompleted`を呼び、その中で処理が完結する。completion_handler.rsが
/// `IAgileObject`を実装しているため、任意のスレッド/アパートメントから直接
/// 呼ばれても安全)ため、`PendingActivation`をSendとして扱ってよい。
struct PendingActivation {
    #[allow(dead_code)] // 完了コールバックが実際に呼ばれるまで生存させるためだけに保持する
    operation: Option<IActivateAudioInterfaceAsyncOperation>,
    #[allow(dead_code)]
    handler: IActivateAudioInterfaceCompletionHandler,
    #[allow(dead_code)]
    expired: Arc<AtomicBool>,
}

unsafe impl Send for PendingActivation {}

static PENDING_ACTIVATIONS: OnceLock<Mutex<Vec<PendingActivation>>> = OnceLock::new();

/// 診断用ハードタイムアウト経路でのみ呼ばれる。プロセスの生存期間中、
/// エントリを個別に取り除く仕組みはあえて持たない(スパイクの実行時間内で
/// 発生するタイムアウト回数は高々数件と見込まれるため、「取り除かない」
/// という単純化が許容できる。本番実装ではこの経路自体を採用しない想定)。
fn park_pending_activation(
    operation: Option<IActivateAudioInterfaceAsyncOperation>,
    handler: IActivateAudioInterfaceCompletionHandler,
    expired: Arc<AtomicBool>,
) {
    let registry = PENDING_ACTIVATIONS.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = registry.lock().unwrap();
    guard.push(PendingActivation {
        operation,
        handler,
        expired,
    });
    tracing::warn!(
        parked_count = guard.len(),
        "activation timed out; operation/handler parked until the late completion callback \
         fires (diagnostic --activation-hard-timeout-ms path only, see §5.4)"
    );
}

/// タイムアウトの扱い: 既定ではハードタイムアウトを設けず、recv()で完了まで
/// 無条件にブロックする。診断目的でハードタイムアウトを使いたい場合のみ
/// hard_timeoutにSomeを渡す(§5.4参照)。
pub fn activate_process_loopback_client(
    target_pid: u32,
    mode: ProcessLoopbackMode,
    hard_timeout: Option<std::time::Duration>,
) -> Result<IAudioClient, SpikeError> {
    let activation_params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: target_pid,
                ProcessLoopbackMode: match mode {
                    ProcessLoopbackMode::Include => {
                        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                    }
                    ProcessLoopbackMode::Exclude => {
                        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                    }
                },
            },
        },
    };
    let prop = make_blob_propvariant(&activation_params);

    let expired = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<windows::core::Result<IUnknown>>();
    let handler: IActivateAudioInterfaceCompletionHandler =
        CompletionHandler::new(tx, expired.clone()).into();

    let operation_result = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&prop),
            &handler,
        )
    };
    // make_blob_propvariantのコメント参照: pBlobDataはactivation_paramsという
    // スタック値を指しており、PROPVARIANT::Drop(PropVariantClear)にCoTaskMemFree
    // させてはいけない。呼び出しは既に完了しているので、ここでforgetする。
    std::mem::forget(prop);
    let operation = operation_result?;

    match hard_timeout {
        None => {
            // 既定経路: 完了まで無条件にブロックする。
            let unknown = rx.recv().map_err(|_| SpikeError::ActivationChannelClosed)??;
            let audio_client: IAudioClient = unknown.cast()?;
            Ok(audio_client)
        }
        Some(timeout) => match rx.recv_timeout(timeout) {
            Ok(result) => Ok(result?.cast()?),
            Err(_) => {
                expired.store(true, Ordering::SeqCst);
                park_pending_activation(Some(operation), handler, expired.clone());
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
    if let Ok(raw_fmt) = mix_format_result {
        // GetMixFormatが返すメモリはCoTaskMemFreeで解放する責務を呼び出し側が
        // 負う。WaveFormatBoxがRAIIで解放する。
        let mix_format = spike_common::WaveFormatBox::from_raw(raw_fmt);
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let init_ok = unsafe {
            client_a
                .Initialize(
                    windows::Win32::Media::Audio::AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    0,
                    0,
                    mix_format.as_ref(),
                    None,
                )
                .is_ok()
        };
        if init_ok {
            let format_info = AudioFormatInfo::from_waveformatex(mix_format.as_ref());
            return Ok((client_a, format_info));
        }
    }
    // client_aはInitialize未実施または失敗済み。ここで破棄し、二度とこの
    // オブジェクトへInitializeを呼ばない(P0-2)。
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
    pub callback_timeout_ms: u32,
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

        let (audio_client, format_info) = activate_and_initialize_with_retry(
            self.target_pid,
            self.mode,
            self.activation_hard_timeout,
        )?;

        let capture_client: windows::Win32::Media::Audio::IAudioCaptureClient =
            unsafe { audio_client.GetService()? };

        // Process Loopbackには物理endpointがないため、summary.jsonのdevices
        // ブロック相当の識別情報として、対象PIDとモードを文字列化して渡す
        // (spike_common::run_capture_loopのdevice_id/device_friendly_name引数、§4.8)。
        spike_common::run_capture_loop(
            audio_client,
            capture_client,
            StreamId::ProcessLoopback,
            Some(self.target_pid),
            self.capture_epoch,
            format_info,
            format!("pid:{}", self.target_pid),
            format!("{:?}", self.mode),
            self.pipeline_drop_counter,
            self.callback_timeout_ms,
            tx,
            stop,
        )
    }
}
