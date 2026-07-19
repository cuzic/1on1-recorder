//! Turns a stream of arbitrarily-sized PCM pushes into whisper.cpp-sized chunks using
//! a crude energy-threshold VAD, deliberately kept independent of `whisper-rs` so it
//! can be unit tested without a `.bin` model file (see crate root docs for why that
//! separation matters for this spike). [`ChunkBuffer::push`] is the mid-stream path,
//! [`ChunkBuffer::flush`] is what `finalize()` calls for the trailing remainder.

use audio_timeline::rms;

/// Tunables for [`ChunkBuffer`]. All `_ms` fields are milliseconds; `sample_rate_hz`
/// must match the audio actually pushed in (this crate always uses 16000 — see the
/// crate root docs — but the buffering logic itself doesn't hardcode that, so it can
/// be exercised with easier-to-read sample counts in tests).
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    pub sample_rate_hz: u32,
    /// Once buffered audio reaches this length *and* trailing silence has lasted
    /// `silence_hold_ms`, the buffer is cut into a chunk. This is the lower bound of
    /// the "3〜5秒" window from the design brief.
    pub target_chunk_ms: u32,
    /// Hard cap: buffered audio is cut into a chunk at this length regardless of
    /// silence, so a long run of continuous speech (e.g. someone monologuing with no
    /// detectable pause) still gets transcribed incrementally instead of growing the
    /// buffer — and inference latency — unboundedly.
    pub max_chunk_ms: u32,
    /// A silence-triggered cut is never emitted for less than this much buffered
    /// audio (`flush` is exempt — see its doc comment), to avoid feeding whisper.cpp
    /// fragments so short it tends to hallucinate text (a documented whisper.cpp
    /// quirk on near-silent input, not specific to this crate).
    pub min_chunk_ms: u32,
    /// How long the trailing RMS has to stay under `silence_rms_threshold` before a
    /// chunk boundary is considered "found", once `min_chunk_ms` has been reached.
    pub silence_hold_ms: u32,
    /// RMS (of samples in `-1.0..=1.0`) below this is treated as silence. This is a
    /// crude placeholder, *not* a real VAD — see the crate root docs' open-questions
    /// section. Expect this to need per-microphone tuning, or outright replacement
    /// with a real VAD (WebRTC VAD, Silero, or whisper.cpp's own bundled VAD model —
    /// see `FullParams::enable_vad` in `lib.rs`'s doc comment), before this leaves the
    /// spike stage.
    pub silence_rms_threshold: f32,
    /// Granularity at which trailing RMS is recomputed as samples arrive. Smaller
    /// values make the silence boundary more precise at the cost of more RMS passes.
    pub vad_window_ms: u32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 16_000,
            target_chunk_ms: 4_000,
            max_chunk_ms: 8_000,
            min_chunk_ms: 500,
            silence_hold_ms: 300,
            silence_rms_threshold: 0.01,
            vad_window_ms: 100,
        }
    }
}

/// One whisper.cpp-ready chunk cut from the buffer, plus its absolute sample range so
/// the caller can compute `audio_start_ms`/`audio_end_ms` for `SttEvent::FinalTranscript`
/// without trusting whisper.cpp's own (chunk-relative) segment timestamps.
#[derive(Debug, PartialEq)]
pub(crate) struct PendingChunk {
    pub pcm: Vec<f32>,
    pub start_sample: u64,
    pub end_sample: u64,
}

/// Accumulates PCM across `send_audio` calls and cuts it into [`PendingChunk`]s per
/// [`ChunkConfig`]. Holds no whisper-rs types at all — see module docs.
pub(crate) struct ChunkBuffer {
    config: ChunkConfig,
    samples: Vec<f32>,
    /// Absolute sample index of `samples[0]`, taken from the caller-supplied
    /// `start_sample` the moment this buffer starts accumulating again after being
    /// empty (i.e. right after the previous cut, or at construction). Not
    /// self-tracked by counting pushed samples, so a gap the caller reports in its own
    /// `AudioChunk::start_sample` sequence is reflected in the emitted chunk's range
    /// rather than silently papered over. Within a single buffer's lifetime, pushed
    /// audio is assumed contiguous (this matches every other adapter in this
    /// workspace, none of which reconcile per-call `start_sample` against a running
    /// total either).
    buffer_start_sample: Option<u64>,
    /// Consecutive milliseconds of trailing near-silence observed since the buffer
    /// last reset (on construction or the previous cut), recomputed in
    /// `vad_window_ms` steps as samples arrive.
    silent_run_ms: u32,
}

/// Below this, `flush()` drops the remainder instead of emitting it — see `flush`'s
/// doc comment.
const MIN_FLUSH_MS: u32 = 100;

impl ChunkBuffer {
    pub fn new(config: ChunkConfig) -> Self {
        Self {
            config,
            samples: Vec::new(),
            buffer_start_sample: None,
            silent_run_ms: 0,
        }
    }

    fn ms_to_samples(&self, ms: u32) -> usize {
        (self.config.sample_rate_hz as u64 * ms as u64 / 1000) as usize
    }

    fn samples_to_ms(&self, samples: usize) -> u32 {
        (samples as u64 * 1000 / self.config.sample_rate_hz.max(1) as u64) as u32
    }

    /// Pushes newly captured PCM (starting at the caller's absolute `start_sample`)
    /// and returns zero or more chunks cut from the buffer, in order. Processes in
    /// `vad_window_ms`-sized windows internally so a single large push (callers are
    /// free to hand over multi-second buffers at once — `stt_api::AudioChunk`'s own
    /// docs say chunk size is the caller's choice) can still yield multiple chunks in
    /// one call, rather than only ever cutting on the *next* `push`.
    pub fn push(&mut self, pcm: &[f32], start_sample: u64) -> Vec<PendingChunk> {
        if pcm.is_empty() {
            return Vec::new();
        }

        let mut cuts = Vec::new();
        let window = self.ms_to_samples(self.config.vad_window_ms).max(1);
        let mut offset = 0;
        while offset < pcm.len() {
            let end = (offset + window).min(pcm.len());
            let window_samples = &pcm[offset..end];
            // Re-checked per window, not just once at the top of `push`: a single
            // large push can trigger more than one cut in this loop (see
            // `a_single_large_push_can_yield_multiple_chunks`), and each cut resets
            // `buffer_start_sample` to `None` — the *next* window after such a cut is
            // the true start of the next buffer, at this push's `start_sample` plus
            // however many samples of this same push were already consumed.
            if self.samples.is_empty() {
                self.buffer_start_sample = Some(start_sample + offset as u64);
            }
            self.samples.extend_from_slice(window_samples);

            if rms(window_samples) < self.config.silence_rms_threshold {
                self.silent_run_ms = self
                    .silent_run_ms
                    .saturating_add(self.samples_to_ms(window_samples.len()).max(1));
            } else {
                self.silent_run_ms = 0;
            }

            let buffered_ms = self.samples_to_ms(self.samples.len());
            let hit_silence_boundary = buffered_ms >= self.config.min_chunk_ms
                && buffered_ms >= self.config.target_chunk_ms
                && self.silent_run_ms >= self.config.silence_hold_ms;
            let hit_hard_cap = buffered_ms >= self.config.max_chunk_ms;

            if hit_silence_boundary || hit_hard_cap {
                cuts.push(self.cut());
            }

            offset = end;
        }
        cuts
    }

    fn cut(&mut self) -> PendingChunk {
        let pcm = std::mem::take(&mut self.samples);
        let start_sample = self.buffer_start_sample.unwrap_or(0);
        let end_sample = start_sample + pcm.len() as u64;
        self.buffer_start_sample = None;
        self.silent_run_ms = 0;
        PendingChunk {
            pcm,
            start_sample,
            end_sample,
        }
    }

    /// Called from `finalize()`: returns whatever is left over as a final chunk,
    /// bypassing `min_chunk_ms`/silence-detection entirely (there's no "wait for more
    /// audio" option once the caller has said the session is over). The one thing it
    /// still refuses is a sliver under [`MIN_FLUSH_MS`] — more likely to be trailing
    /// silence/padding than real speech, and whisper.cpp is known to hallucinate text
    /// on such fragments rather than correctly emit nothing.
    pub fn flush(&mut self) -> Option<PendingChunk> {
        if self.samples.is_empty() {
            return None;
        }
        if self.samples_to_ms(self.samples.len()) < MIN_FLUSH_MS {
            self.samples.clear();
            self.buffer_start_sample = None;
            self.silent_run_ms = 0;
            return None;
        }
        Some(self.cut())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// A short, easy-to-reason-about config: 100 samples/sec instead of 16000, so
    /// test data can use tens of samples instead of thousands.
    fn test_config() -> ChunkConfig {
        ChunkConfig {
            sample_rate_hz: 100,
            target_chunk_ms: 1_000, // 100 samples
            max_chunk_ms: 3_000,    // 300 samples
            min_chunk_ms: 200,      // 20 samples
            silence_hold_ms: 300,   // 30 samples
            silence_rms_threshold: 0.01,
            vad_window_ms: 100, // 10 samples per window
        }
    }

    fn tone(n: usize) -> Vec<f32> {
        vec![0.5; n]
    }

    fn silence(n: usize) -> Vec<f32> {
        vec![0.0; n]
    }

    #[test]
    fn empty_push_yields_no_chunks() {
        let mut buf = ChunkBuffer::new(test_config());
        assert!(buf.push(&[], 0).is_empty());
    }

    #[test]
    fn no_cut_before_target_and_silence_hold_are_both_satisfied() {
        let mut buf = ChunkBuffer::new(test_config());
        // 90 samples of tone: under target_chunk_ms (100 samples), never cuts no
        // matter how it's chopped into pushes.
        let cuts = buf.push(&tone(90), 0);
        assert!(cuts.is_empty());
    }

    #[test]
    fn cuts_once_target_reached_and_then_silence_hold_elapses() {
        let mut buf = ChunkBuffer::new(test_config());
        // 100 samples of tone reaches target_chunk_ms exactly, but no silence yet.
        assert!(buf.push(&tone(100), 0).is_empty());
        // 20 samples of silence (2 windows) is under silence_hold_ms (30 samples) —
        // still no cut.
        assert!(buf.push(&silence(20), 100).is_empty());
        // One more silent window (10 samples) crosses silence_hold_ms (30 samples
        // total silence) with buffered audio already >= target — now it cuts.
        let cuts = buf.push(&silence(10), 120);
        assert_eq!(cuts.len(), 1);
        let chunk = &cuts[0];
        assert_eq!(chunk.start_sample, 0);
        assert_eq!(chunk.end_sample, 130);
        assert_eq!(chunk.pcm.len(), 130);
    }

    #[test]
    fn hard_cap_forces_a_cut_even_without_silence() {
        let mut buf = ChunkBuffer::new(test_config());
        // 300 samples of continuous tone, well past max_chunk_ms (300 samples) and
        // never silent — must still cut via the hard cap, not hang forever.
        let cuts = buf.push(&tone(300), 0);
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].start_sample, 0);
        assert_eq!(cuts[0].end_sample, 300);
    }

    #[test]
    fn a_single_large_push_can_yield_multiple_chunks() {
        let mut buf = ChunkBuffer::new(test_config());
        // 700 samples of continuous tone in one push: hard cap fires at 300 and 600,
        // leaving 100 samples buffered (under both target and max) — 2 cuts.
        let cuts = buf.push(&tone(700), 0);
        assert_eq!(cuts.len(), 2);
        assert_eq!((cuts[0].start_sample, cuts[0].end_sample), (0, 300));
        assert_eq!((cuts[1].start_sample, cuts[1].end_sample), (300, 600));
    }

    #[test]
    fn consecutive_chunks_have_contiguous_sample_ranges() {
        let mut buf = ChunkBuffer::new(test_config());
        let mut all_cuts = Vec::new();
        all_cuts.extend(buf.push(&tone(300), 0));
        all_cuts.extend(buf.push(&tone(300), 300));
        assert_eq!(all_cuts.len(), 2);
        assert_eq!(all_cuts[0].end_sample, all_cuts[1].start_sample);
    }

    #[test]
    fn many_small_pushes_accumulate_the_same_as_one_big_push() {
        let mut small = ChunkBuffer::new(test_config());
        let mut cuts = Vec::new();
        for i in 0..30 {
            cuts.extend(small.push(&tone(10), i * 10));
        }
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].pcm.len(), 300);
    }

    #[test]
    fn flush_on_empty_buffer_returns_none() {
        let mut buf = ChunkBuffer::new(test_config());
        assert!(buf.flush().is_none());
    }

    #[test]
    fn flush_drops_a_sliver_under_the_min_flush_floor() {
        let mut buf = ChunkBuffer::new(test_config());
        // 5 samples at 100Hz = 50ms, under MIN_FLUSH_MS (100ms) — never reaches
        // silence_hold_ms's window granularity either, so push() itself won't cut it.
        assert!(buf.push(&tone(5), 0).is_empty());
        assert!(buf.flush().is_none());
    }

    #[test]
    fn flush_emits_a_remainder_at_or_above_the_min_flush_floor_even_under_min_chunk_ms() {
        let mut buf = ChunkBuffer::new(test_config());
        // 15 samples = 150ms: at/above MIN_FLUSH_MS (100ms) but under min_chunk_ms
        // (200ms) — push() won't cut it, but flush() must still emit it.
        assert!(buf.push(&tone(15), 0).is_empty());
        let chunk = buf.flush().expect("remainder at/above the flush floor is emitted");
        assert_eq!(chunk.pcm.len(), 15);
        assert_eq!((chunk.start_sample, chunk.end_sample), (0, 15));
    }

    #[test]
    fn flush_after_a_cut_only_returns_the_new_remainder() {
        let mut buf = ChunkBuffer::new(test_config());
        let cuts = buf.push(&tone(300), 0); // hard-cap cut, buffer now empty
        assert_eq!(cuts.len(), 1);
        assert!(buf.flush().is_none());
    }

    #[test]
    fn buffer_start_sample_reflects_the_callers_reported_offset_after_a_gap() {
        let mut buf = ChunkBuffer::new(test_config());
        let cuts = buf.push(&tone(300), 0);
        assert_eq!(cuts[0].start_sample, 0);
        // Caller reports a gap (e.g. dropped frames) before the next push — the next
        // chunk's start_sample must reflect that gap, not silently continue from 300.
        let chunk = {
            buf.push(&tone(10), 1_000);
            buf.flush().unwrap()
        };
        assert_eq!(chunk.start_sample, 1_000);
    }

    #[test]
    fn rms_of_silence_is_zero_and_of_a_tone_is_its_amplitude() {
        assert_eq!(rms(&silence(10)), 0.0);
        assert!((rms(&tone(10)) - 0.5).abs() < 1e-6);
        assert_eq!(rms(&[]), 0.0);
    }
}
