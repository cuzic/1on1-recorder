/// Create one per thread. Calls `CoUninitialize` on drop.
/// Audio capture threads are initialized into the MTA (`COINIT_MULTITHREADED`).
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
