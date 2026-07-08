// spike-windows-01-02-detail-design.md §3.9
//
// AvSetMmThreadCharacteristicsW(L"Pro Audio", ...) でスレッドをマルチメディア
// クラスケジューラへ登録し、コールバックのスケジューリング遅延を最小化する。
// 戻り値は (mmcss_applied: このスレッドでMMCSS登録が成功したか, f()の戻り値) のタプル。
// 適用に失敗した場合も録音自体は継続し、失敗した事実を記録するに留める
// (MMCSS登録失敗を致命的エラーにはしない)。

// 訂正: HANDLEはwindows::Win32::Media::Multimediaではなくwindows::Win32::Foundationに
// 定義されている(AvSetMmThreadCharacteristicsWの戻り値の型としても同じ)。
struct MmcssGuard(windows::Win32::Foundation::HANDLE);

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        // TODO(§3.9): AvRevertMmThreadCharacteristics(self.0)
    }
}

pub fn with_pro_audio_priority<F: FnOnce() -> R, R>(f: F) -> (bool, R) {
    // TODO(§3.9): AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index)
    // 成功時: let _guard = MmcssGuard(handle); (true, f())
    // 失敗時: tracing::warn!(...); (false, f())
    tracing::warn!("mmcss::with_pro_audio_priority is not yet implemented; running without MMCSS");
    (false, f())
}
