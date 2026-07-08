// spike-windows-01-02-detail-design.md §3.6

/// スレッドごとに1つ生成する。Drop時にCoUninitializeする。
/// オーディオキャプチャスレッドは MTA (COINIT_MULTITHREADED) で初期化する。
pub struct ComApartment;

impl ComApartment {
    pub fn new_mta() -> windows::core::Result<Self> {
        // TODO(§3.6): windows::Win32::System::Com::CoInitializeEx(
        //     None, windows::Win32::System::Com::COINIT_MULTITHREADED,
        // )
        todo!("CoInitializeEx(None, COINIT_MULTITHREADED)")
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // TODO(§3.6): windows::Win32::System::Com::CoUninitialize()
    }
}
