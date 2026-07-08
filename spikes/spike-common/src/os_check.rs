// spike-windows-01-02-detail-design.md §3.10

use crate::error::SpikeError;

#[derive(Debug, Clone, serde::Serialize)]
pub struct OsVersionInfo {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

/// RtlGetVersion(公開APIのGetVersionExはマニフェストの互換シムで嘘の値を
/// 返しうるため使わない)でOSバージョンを取得する。
pub fn query_os_version() -> windows::core::Result<OsVersionInfo> {
    todo!("RtlGetVersionでOSVERSIONINFOEXWを取得しOsVersionInfoへ変換する")
}

/// SPIKE-02(Process Loopback)専用の下限チェック。
/// 「未満なら即NO-GO相当として扱う」閾値(20348)と、
/// 「これ未満だと一部情報源で非対応とされる」閾値(20438)の両方を記録し、
/// どちらの基準で判定したかをRESULT.mdへ残す(§7の既知の不確実性#6)。
pub const PROCESS_LOOPBACK_MIN_BUILD_CONSERVATIVE: u32 = 20348;
pub const PROCESS_LOOPBACK_MIN_BUILD_STRICT: u32 = 20438;

pub fn check_process_loopback_support(info: &OsVersionInfo) -> Result<(), SpikeError> {
    if info.build < PROCESS_LOOPBACK_MIN_BUILD_CONSERVATIVE {
        return Err(SpikeError::UnsupportedOsBuild { build: info.build });
    }
    Ok(())
}
