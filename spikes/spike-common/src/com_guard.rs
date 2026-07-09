// spike-windows-01-02-detail-design.md §3.6

/// スレッドごとに1つ生成する。Drop時にCoUninitializeする。
/// オーディオキャプチャスレッドは MTA (COINIT_MULTITHREADED) で初期化する。
pub struct ComApartment;

impl ComApartment {
    pub fn new_mta() -> windows::core::Result<Self> {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        }
        .ok()?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}
