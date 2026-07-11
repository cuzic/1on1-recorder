//! Registers the calling thread with the Multimedia Class Scheduler Service via
//! `AvSetMmThreadCharacteristicsW(L"Pro Audio", ...)`, which minimizes scheduling
//! latency for the audio callback. Failing to register is not treated as fatal —
//! capture continues either way; only the fact of the failure is reported back.

struct MmcssGuard(windows::Win32::Foundation::HANDLE);

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// Runs `f` with the "Pro Audio" MMCSS characteristic applied to the current thread
/// for its duration. Returns `(mmcss_applied, f()'s return value)`.
pub fn with_pro_audio_priority<F: FnOnce() -> R, R>(f: F) -> (bool, R) {
    let mut task_index: u32 = 0;
    let task_name = windows::core::HSTRING::from("Pro Audio");
    match unsafe {
        windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(&task_name, &mut task_index)
    } {
        Ok(handle) => {
            let _guard = MmcssGuard(handle);
            (true, f())
        }
        Err(e) => {
            tracing::warn!(error = %e, "MMCSS registration failed; running without Pro Audio priority");
            (false, f())
        }
    }
}
