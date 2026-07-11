/// The counter frequency from `QueryPerformanceFrequency`. Fixed at boot but can
/// differ across machines, so it's always queried at runtime rather than assumed to
/// be a fixed 10MHz.
pub struct QpcClock {
    freq_hz: u64,
}

impl QpcClock {
    pub fn query() -> windows::core::Result<Self> {
        let mut freq: i64 = 0;
        unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq)? };
        Ok(Self { freq_hz: freq as u64 })
    }

    pub fn freq_hz(&self) -> u64 {
        self.freq_hz
    }

    pub fn now_100ns(&self) -> u64 {
        let mut count: i64 = 0;
        // QueryPerformanceCounter is documented to never fail in practice, but its
        // signature returns a Result; treat failure as 0 rather than panicking, since
        // this method's own signature doesn't return a Result.
        if unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut count) }.is_err() {
            return 0;
        }
        ((count as u128 * 10_000_000u128) / self.freq_hz as u128) as u64
    }
}
