// spike-windows-01-02-detail-design.md §3.10

use crate::error::SpikeError;
use windows::Win32::System::SystemInformation::OSVERSIONINFOEXW;

#[derive(Debug, Clone, serde::Serialize)]
pub struct OsVersionInfo {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

// windowsクレートはRtlGetVersionを公開していない(ntdll.dllのエクスポートで、
// 公式のWin32メタデータに含まれないため)。ntdll.dllへ直接リンクして宣言する。
// mingw-w64のクロスコンパイル環境にはlibntdll.aが同梱されておりリンクできる
// ことを確認済み(windows-build-verification.md)。将来MSVCネイティブビルドへ
// 移行する場合は、公式SDKにntdll.libが含まれないためGetProcAddress経由へ
// 切り替える必要がある。
#[link(name = "ntdll")]
extern "system" {
    fn RtlGetVersion(version_information: *mut OSVERSIONINFOEXW) -> i32;
}

/// RtlGetVersion(公開APIのGetVersionExはマニフェストの互換シムで嘘の値を
/// 返しうるため使わない)でOSバージョンを取得する。
pub fn query_os_version() -> windows::core::Result<OsVersionInfo> {
    let mut info = OSVERSIONINFOEXW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOEXW>() as u32,
        ..Default::default()
    };
    // RtlGetVersionはNTSTATUSを返す。実質的に失敗しないとされているが、
    // 呼び出し規約どおりStatusを確認したうえでwindows::core::Errorへ変換する。
    let status = unsafe { RtlGetVersion(&mut info) };
    if status < 0 {
        return Err(windows::core::Error::from(windows::core::HRESULT::from_nt(status)));
    }
    Ok(OsVersionInfo {
        major: info.dwMajorVersion,
        minor: info.dwMinorVersion,
        build: info.dwBuildNumber,
    })
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
