// spike-windows-01-02-detail-design.md §3.3

/// QueryPerformanceFrequencyで取得したカウンタ周波数。
/// システム起動時に固定されるが、環境ごとに値が異なりうるため、
/// 定数(10MHz)扱いにせず必ず実行時に取得する。
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
        // QueryPerformanceCounterは(ドキュメント上)失敗しないとされているが、
        // Result<()>を返すシグネチャのため、失敗時は0として扱い呼び出し側の
        // panicを避ける(このメソッド自体はResultを返さない設計のため)。
        if unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut count) }.is_err() {
            return 0;
        }
        ((count as u128 * 10_000_000u128) / self.freq_hz as u128) as u64
    }

    pub fn hundred_ns_to_ns(v: u64) -> u64 {
        v.saturating_mul(100)
    }
}

/// 単調性チェック用。逆行を検出したら呼び出し側へ知らせる。
/// `timestamp_error == true`のレコードはチェック対象から除外する。
#[derive(Default)]
pub struct MonotonicGuard {
    last: Option<u64>,
}

impl MonotonicGuard {
    pub fn check(&mut self, value_100ns: u64) -> bool {
        let ok = self.last.map_or(true, |last| value_100ns >= last);
        self.last = Some(value_100ns);
        ok
    }
}
