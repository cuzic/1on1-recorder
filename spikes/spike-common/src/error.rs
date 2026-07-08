// spike-windows-01-02-detail-design.md §3.7

#[derive(thiserror::Error, Debug)]
pub enum SpikeError {
    #[error("COM/WASAPI呼び出し失敗: {0}")]
    Com(#[from] windows::core::Error),

    #[error("対象デバイスが見つかりません: {0}")]
    DeviceNotFound(String),

    #[error("対象プロセスが見つかりません: {0}")]
    ProcessNotFound(String),

    #[error("ActivateAudioInterfaceAsync がタイムアウトしました({0:?})。オプションのハードタイムアウトモード使用時のみ発生する")]
    ActivationTimeout(std::time::Duration),

    #[error("ActivateAudioInterfaceAsync の完了通知チャネルが送信側切断のまま閉じられました")]
    ActivationChannelClosed,

    #[error("ActivateAudioInterfaceAsync がエラーを返しました: hresult=0x{0:08X}")]
    ActivationFailed(u32),

    #[error("未対応のフォーマットです: {0}")]
    UnsupportedFormat(String),

    #[error("Process Loopback未対応の可能性があるOSビルドです: build={build}")]
    UnsupportedOsBuild { build: u32 },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
