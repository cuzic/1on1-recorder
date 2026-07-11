/// Self/Remote level meter values, `[0.0, 1.0]`-ish range (no dB conversion) — the
/// same shape as `app_service::LevelSnapshot` (task #10/#11's real-capture level
/// tracking), duplicated here as a plain, always-available type so this crate's
/// status DTO doesn't need to depend on the `windows-supervisor`-gated
/// `app_service::LevelSnapshot` on non-Windows builds.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct LevelSnapshot {
    pub self_rms: f32,
    pub self_peak: f32,
    pub remote_rms: f32,
    pub remote_peak: f32,
}

#[cfg(windows)]
impl From<app_service::LevelSnapshot> for LevelSnapshot {
    fn from(s: app_service::LevelSnapshot) -> Self {
        Self { self_rms: s.self_rms, self_peak: s.self_peak, remote_rms: s.remote_rms, remote_peak: s.remote_peak }
    }
}

/// Dev-mode (non-Windows) placeholder level, deterministic from elapsed time only
/// (no randomness/system clock, so it's reproducible) — ported from
/// `spikes/spike-05-tauri-tray`'s `synthesize_level`. Real capture only exists on
/// Windows (`capture-windows`), so this is what lets the UI be exercised locally;
/// it is never used when `cfg(windows)` (see `recording.rs`).
#[cfg(not(windows))]
pub fn dev_placeholder_level(elapsed: std::time::Duration) -> LevelSnapshot {
    let t = elapsed.as_secs_f32();
    let envelope = 0.5 + 0.5 * (t * 0.3).sin();
    let jitter = 0.05 * (t * 13.7).sin() * (t * 3.1).cos();
    let self_rms = (envelope * 0.6 + jitter).clamp(0.0, 1.0);
    let self_peak = (self_rms + 0.15 * (t * 13.7).sin().abs()).clamp(0.0, 1.0);
    // A different phase for Remote, purely so the two meters visibly differ in
    // this placeholder — not a claim about what real Self/Remote audio would look
    // like relative to each other.
    let remote_envelope = 0.5 + 0.5 * ((t + 1.5) * 0.25).sin();
    let remote_rms = (remote_envelope * 0.5).clamp(0.0, 1.0);
    let remote_peak = (remote_rms + 0.1).clamp(0.0, 1.0);
    LevelSnapshot { self_rms, self_peak, remote_rms, remote_peak }
}
