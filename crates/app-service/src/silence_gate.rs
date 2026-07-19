//! Local, cheap energy-threshold VAD used to decide whether a chunk of PCM is worth
//! sending to a (metered) streaming STT session at all, as opposed to
//! `stt_whisper::chunk_buffer::ChunkBuffer`'s similarly crude RMS VAD, which instead
//! decides *where to cut* audio that's always eventually sent. Streaming STT
//! providers bill for wall-clock audio duration sent, so a track that's mostly one
//! side listening in silence (typical for the "Remote" track, and often "Self" too)
//! wastes real money if every silent second is streamed anyway. This is deliberately
//! not a real VAD (WebRTC VAD/Silero/etc.) — see [`GateConfig`]'s threshold docs.
//!
//! [`SilenceGate::process`] is the entry point: it walks the pushed samples in
//! `vad_window_ms`-sized windows, running a `Speaking`/`Hangover`/`Silent` state
//! machine per window, and returns the resulting [`GateAction`]s (adjacent windows
//! that resolve to the same kind of action are coalesced into one).

use std::collections::VecDeque;

/// What the caller should do with a span of audio after a [`SilenceGate::process`]
/// call. Several of these can be returned from a single `process` call, in order,
/// since one push can span more than one state transition (e.g. the tail end of a
/// long silence followed by the start of speech).
#[derive(Debug, PartialEq)]
pub enum GateAction<'a> {
    /// Send this span as-is; it was judged speech (or within the post-speech
    /// hangover grace period) for its entire duration.
    Send(&'a [f32]),
    /// Send this owned buffer: the `pre_roll_ms` of audio buffered from just before
    /// speech was detected, stitched to the front of the window that triggered
    /// `Silent -> Speaking`. Owned (rather than borrowed) because it's assembled
    /// from the pre-roll ring buffer plus a slice of the input, not a single
    /// contiguous slice of the caller's input.
    SendStitched(Vec<f32>),
    /// This many samples were judged silence and permanently discarded (never sent,
    /// and no longer retrievable from the pre-roll buffer either). See the module
    /// docs / `SilenceGate` docs for why this only counts samples once they're
    /// evicted from the pre-roll ring, not the moment they're judged silent.
    Drop { sample_count: u64 },
}

/// Tunables for [`SilenceGate`]. All `_ms` fields are milliseconds; `sample_rate_hz`
/// must match the audio actually pushed into `process`.
#[derive(Debug, Clone)]
pub struct GateConfig {
    pub sample_rate_hz: u32,
    /// Granularity at which RMS is recomputed as samples arrive — the gate's state
    /// machine advances one window at a time rather than once per `process` call, so
    /// a single large push can still resolve into multiple `GateAction`s.
    pub vad_window_ms: u32,
    /// How long a `Speaking -> Hangover` transition holds before giving up and
    /// declaring `Silent`, so a short pause for breath or a filler "えーと" mid-sentence
    /// isn't treated as the end of speech and doesn't cut the sender off.
    ///
    /// Quantized to whole `vad_window_ms` windows: the window in which RMS first
    /// drops below `silent_rms_threshold` is always sent in full (state resolves to
    /// `Hangover`, which still sends), so the shortest possible grace period is one
    /// window, regardless of how small `hangover_ms` is set (including 0). This
    /// implementation targets `hangover_ms` values comfortably larger than
    /// `vad_window_ms` (the default is a 10x margin) where this rounding is
    /// negligible; it is not meant to support exact sub-window hangover values.
    pub hangover_ms: u32,
    /// How much audio immediately preceding a detected `Silent -> Speaking`
    /// transition is stitched back onto the front of what gets sent, so the leading
    /// consonant of the first word isn't clipped by the VAD's reaction time.
    pub pre_roll_ms: u32,
    /// RMS (of samples in `-1.0..=1.0`) at or above this is treated as speech —
    /// triggers `Hangover -> Speaking` and `Silent -> Speaking`. Deliberately higher
    /// than `silent_rms_threshold` (hysteresis) so borderline audio doesn't flap
    /// between states on every window. Like `chunk_buffer::ChunkConfig`'s
    /// `silence_rms_threshold` (which this is modeled on), this is a crude
    /// placeholder that's strongly environment/microphone-dependent and expected to
    /// need per-deployment tuning, or eventual replacement with a real VAD.
    pub speaking_rms_threshold: f32,
    /// RMS below this is treated as (the start of) silence — triggers
    /// `Speaking -> Hangover`. See `speaking_rms_threshold`'s docs re: hysteresis and
    /// tuning.
    pub silent_rms_threshold: f32,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 16_000,
            vad_window_ms: 100,
            hangover_ms: 1_000,
            pre_roll_ms: 200,
            // See the threshold fields' own docs: these mirror
            // `chunk_buffer::ChunkConfig::silence_rms_threshold`'s 0.01 crude RMS
            // placeholder, split into a hysteresis pair around it.
            speaking_rms_threshold: 0.02,
            silent_rms_threshold: 0.01,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Actively sending; RMS has been at/above `speaking_rms_threshold` (or this is
    /// the initial post-stitch window).
    Speaking,
    /// RMS dropped below `silent_rms_threshold` at `since_samples` (on the gate's own
    /// running sample clock); still sending, but will fall back to `Silent` once
    /// `hangover_ms` (converted to samples) elapses without RMS recovering to
    /// `speaking_rms_threshold`. Measured in samples, not milliseconds, so the
    /// hangover deadline is exact regardless of how many samples are pushed per
    /// `process` call — see `clock_samples`'s docs for why a millisecond clock would
    /// drift.
    Hangover { since_samples: u64 },
    /// Not sending; incoming audio is only retained in the pre-roll ring buffer, in
    /// case speech resumes.
    Silent,
}

/// Speech/silence gate for one audio track's stream of PCM pushes. See the module
/// docs for the "why" and [`GateConfig`] for tunables.
///
/// Starts in [`State::Silent`] — a fresh recording (especially the "Remote" track,
/// whose owner may not speak at all for the first while) is far more likely to open
/// with silence than speech, and starting `Speaking` would mean unconditionally
/// sending everything up to the first detected silence.
pub struct SilenceGate {
    config: GateConfig,
    state: State,
    /// Ring buffer of the most recent `pre_roll_ms` of audio seen while `Silent`
    /// (or while a window's RMS resolves to `Silent`, e.g. right as `Hangover`
    /// expires). Drained and stitched to the front of the triggering window on
    /// `Silent -> Speaking`.
    pre_roll: VecDeque<f32>,
    pre_roll_capacity: usize,
    window_samples: usize,
    /// `config.hangover_ms` converted to samples once at construction time (see
    /// `clock_samples`'s docs for why the hangover deadline is compared in samples,
    /// not milliseconds).
    hangover_samples: u64,
    /// Cumulative samples of audio processed so far, used as the running clock
    /// against which `Hangover { since_samples }` is measured. Not wall-clock time —
    /// it only advances as samples are actually processed by `process`. Deliberately
    /// kept in samples rather than milliseconds: converting each window's sample
    /// count to milliseconds and summing those (instead of summing samples and
    /// converting once) would truncate sub-millisecond remainders on every window,
    /// and small enough per-call pushes (e.g. one sample at a time) would round to
    /// 0ms and stall the clock entirely, so `Hangover` would never expire.
    clock_samples: u64,
}

impl SilenceGate {
    pub fn new(config: GateConfig) -> Self {
        let pre_roll_capacity = ms_to_samples(config.sample_rate_hz, config.pre_roll_ms);
        let window_samples = ms_to_samples(config.sample_rate_hz, config.vad_window_ms).max(1);
        let hangover_samples = ms_to_samples(config.sample_rate_hz, config.hangover_ms) as u64;
        Self {
            state: State::Silent,
            pre_roll: VecDeque::with_capacity(pre_roll_capacity),
            pre_roll_capacity,
            window_samples,
            hangover_samples,
            clock_samples: 0,
            config,
        }
    }

    /// Pushes newly captured PCM and returns zero or more [`GateAction`]s, in order.
    /// Processes internally in `vad_window_ms`-sized windows, so a single push can
    /// resolve into several actions (e.g. a trailing `Drop` for the silence this push
    /// started with, followed by a `SendStitched` once speech is detected partway
    /// through). Adjacent windows that resolve to the same action are coalesced.
    pub fn process<'a>(&mut self, samples: &'a [f32]) -> Vec<GateAction<'a>> {
        let mut actions: Vec<GateAction<'a>> = Vec::new();
        if samples.is_empty() {
            return actions;
        }

        // Coalescing accumulators: `send_run_start` is the offset (into `samples`)
        // where the current contiguous run of "send as-is" windows began, and
        // `drop_run` is the running total of a contiguous run of pre-roll-evicted
        // samples. Both are flushed into `actions` whenever the run is interrupted by
        // a different kind of action.
        let mut send_run_start: Option<usize> = None;
        let mut drop_run: u64 = 0;

        let mut offset = 0usize;
        while offset < samples.len() {
            let end = (offset + self.window_samples).min(samples.len());
            let window = &samples[offset..end];
            self.clock_samples = self.clock_samples.saturating_add(window.len() as u64);

            let level = rms(window);
            let old_state = self.state;
            self.state = next_state(
                old_state,
                level,
                self.clock_samples,
                self.hangover_samples,
                &self.config,
            );

            match (old_state, self.state) {
                (State::Silent, State::Speaking) => {
                    // Flush any pending drop run first: everything gathered so far
                    // was genuinely discarded, and is disjoint from what's about to
                    // be sent below.
                    if drop_run > 0 {
                        actions.push(GateAction::Drop {
                            sample_count: drop_run,
                        });
                        drop_run = 0;
                    }
                    debug_assert!(
                        send_run_start.is_none(),
                        "no send run can be open while State::Silent"
                    );
                    let mut stitched: Vec<f32> =
                        Vec::with_capacity(self.pre_roll.len() + window.len());
                    stitched.extend(self.pre_roll.drain(..));
                    stitched.extend_from_slice(window);
                    actions.push(GateAction::SendStitched(stitched));
                    // Any further windows in this call that remain Speaking start a
                    // fresh contiguous slice-backed run from the next offset.
                }
                (_, State::Silent) => {
                    // Either freshly falling out of Hangover, or continuing a run of
                    // silence — either way this window's audio is only a pre-roll
                    // candidate now, never sent.
                    if let Some(start) = send_run_start.take() {
                        actions.push(GateAction::Send(&samples[start..offset]));
                    }
                    drop_run += self.push_pre_roll(window);
                }
                _ => {
                    // Speaking, or Hangover (which still sends — see State's docs).
                    if drop_run > 0 {
                        actions.push(GateAction::Drop {
                            sample_count: drop_run,
                        });
                        drop_run = 0;
                    }
                    if send_run_start.is_none() {
                        send_run_start = Some(offset);
                    }
                }
            }

            offset = end;
        }

        if let Some(start) = send_run_start.take() {
            actions.push(GateAction::Send(&samples[start..]));
        }
        if drop_run > 0 {
            actions.push(GateAction::Drop {
                sample_count: drop_run,
            });
        }

        actions
    }

    /// Pushes `window` into the pre-roll ring buffer, evicting the oldest samples
    /// once it's at capacity, and returns how many samples were evicted (i.e.
    /// permanently, countably dropped — see `GateAction::Drop`'s docs on why this,
    /// and not "every sample seen while Silent", is what gets counted).
    fn push_pre_roll(&mut self, window: &[f32]) -> u64 {
        if self.pre_roll_capacity == 0 {
            // No pre-roll retention configured at all: every incoming sample is
            // immediately, definitionally evicted.
            return window.len() as u64;
        }
        let mut evicted = 0u64;
        for &sample in window {
            if self.pre_roll.len() == self.pre_roll_capacity {
                self.pre_roll.pop_front();
                evicted += 1;
            }
            self.pre_roll.push_back(sample);
        }
        evicted
    }
}

fn next_state(
    state: State,
    level: f32,
    clock_samples: u64,
    hangover_samples: u64,
    config: &GateConfig,
) -> State {
    match state {
        State::Speaking => {
            if level < config.silent_rms_threshold {
                State::Hangover { since_samples: clock_samples }
            } else {
                State::Speaking
            }
        }
        State::Hangover { since_samples } => {
            if level >= config.speaking_rms_threshold {
                State::Speaking
            } else if clock_samples.saturating_sub(since_samples) >= hangover_samples {
                State::Silent
            } else {
                State::Hangover { since_samples }
            }
        }
        State::Silent => {
            if level >= config.speaking_rms_threshold {
                State::Speaking
            } else {
                State::Silent
            }
        }
    }
}

fn ms_to_samples(sample_rate_hz: u32, ms: u32) -> usize {
    (sample_rate_hz as u64 * ms as u64 / 1000) as usize
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short, easy-to-reason-about config: 100 samples/sec instead of 16000, so
    /// test data can use tens of samples instead of thousands. Mirrors
    /// `chunk_buffer`'s test config style.
    fn test_config() -> GateConfig {
        GateConfig {
            sample_rate_hz: 100,
            vad_window_ms: 100,  // 10 samples per window
            hangover_ms: 300,    // 3 windows
            pre_roll_ms: 200,    // 2 windows / 20 samples
            speaking_rms_threshold: 0.02,
            silent_rms_threshold: 0.01,
        }
    }

    fn speech(n: usize) -> Vec<f32> {
        vec![0.5; n]
    }

    fn silence(n: usize) -> Vec<f32> {
        vec![0.0; n]
    }

    fn total_sent_and_dropped(actions: &[GateAction<'_>]) -> u64 {
        actions
            .iter()
            .map(|a| match a {
                GateAction::Send(s) => s.len() as u64,
                GateAction::SendStitched(s) => s.len() as u64,
                GateAction::Drop { sample_count } => *sample_count,
            })
            .sum()
    }

    #[test]
    fn starts_silent_and_drops_leading_silence() {
        let mut gate = SilenceGate::new(test_config());
        // 10 windows (100 samples) of silence, well past pre-roll capacity (20
        // samples) and never triggering speech: everything should resolve to Drop.
        let input = silence(100);
        let actions = gate.process(&input);
        assert!(actions
            .iter()
            .all(|a| matches!(a, GateAction::Drop { .. })));
        let dropped: u64 = actions
            .iter()
            .map(|a| match a {
                GateAction::Drop { sample_count } => *sample_count,
                _ => 0,
            })
            .sum();
        // The last pre_roll_ms (20 samples) worth is retained in the ring buffer,
        // not yet dropped, so only 100 - 20 = 80 samples are counted as dropped so
        // far.
        assert_eq!(dropped, 80);
    }

    #[test]
    fn speaking_continues_to_send() {
        let mut gate = SilenceGate::new(test_config());
        // Get into Speaking first via a stitched transition, then feed more speech.
        let onset = speech(10);
        let _ = gate.process(&onset);
        let more = speech(50);
        let actions = gate.process(&more);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            GateAction::Send(s) => assert_eq!(s.len(), 50),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn long_silence_after_speech_eventually_drops() {
        let mut gate = SilenceGate::new(test_config());
        let onset = speech(50);
        let _ = gate.process(&onset); // enter Speaking
        // 900ms of silence: well past hangover_ms (300ms), and past hangover's 60
        // remaining silent samples exceed the 20-sample pre-roll capacity too, so
        // some of it must actually be evicted (not just buffered) -> Drop.
        let input = silence(90);
        let actions = gate.process(&input);
        assert!(
            actions.iter().any(|a| matches!(a, GateAction::Drop { .. })),
            "expected at least one Drop action, got {actions:?}"
        );
    }

    #[test]
    fn hangover_expires_even_when_pushed_one_sample_at_a_time() {
        // Regression test: the internal clock used to accumulate whole milliseconds
        // per `process` call (via a per-window ms conversion), which truncated to 0
        // whenever a call's window was smaller than one millisecond's worth of
        // samples — e.g. pushing audio one sample at a time never advanced the clock
        // at all, so `Hangover` could never expire. The clock is now tracked in
        // samples (see `SilenceGate::clock_samples`'s docs), which is exact
        // regardless of push granularity.
        let mut gate = SilenceGate::new(test_config());
        let onset = speech(50);
        let _ = gate.process(&onset); // enter Speaking

        // Push far more than hangover_ms (300ms = 30 samples) worth of silence, one
        // sample at a time, and confirm the gate still eventually drops audio.
        let mut saw_drop = false;
        for _ in 0..90 {
            let one_sample = silence(1);
            let actions = gate.process(&one_sample);
            if actions.iter().any(|a| matches!(a, GateAction::Drop { .. })) {
                saw_drop = true;
                break;
            }
        }
        assert!(
            saw_drop,
            "expected Hangover to eventually expire and drop audio even when pushed \
             one sample at a time"
        );
    }

    #[test]
    fn short_silence_within_hangover_keeps_sending() {
        let mut gate = SilenceGate::new(test_config());
        let onset = speech(50);
        let _ = gate.process(&onset); // enter Speaking
        // 20 samples = 200ms of silence: under hangover_ms (300ms) -> still Hangover,
        // which still sends (no audio dropped, nothing cut off).
        let pause = silence(20);
        let actions = gate.process(&pause);
        assert!(
            !actions.iter().any(|a| matches!(a, GateAction::Drop { .. })),
            "short pause within hangover must not drop audio: {actions:?}"
        );
        let sent: u64 = actions
            .iter()
            .map(|a| match a {
                GateAction::Send(s) => s.len() as u64,
                GateAction::SendStitched(s) => s.len() as u64,
                GateAction::Drop { .. } => 0,
            })
            .sum();
        assert_eq!(sent, 20);

        // And speech resuming within the grace period goes straight back to Speaking
        // (no re-stitch, since we never actually left "sending").
        let resume = speech(10);
        let actions = gate.process(&resume);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], GateAction::Send(s) if s.len() == 10));
    }

    #[test]
    fn silence_to_speaking_stitches_pre_roll() {
        let mut gate = SilenceGate::new(test_config());
        // 40 samples of silence: fills (and evicts from) the 20-sample pre-roll ring,
        // leaving exactly the most recent 20 samples buffered.
        let lead_in = silence(40);
        let _ = gate.process(&lead_in);
        // Now speech starts.
        let onset = speech(10);
        let actions = gate.process(&onset);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            GateAction::SendStitched(s) => {
                // 20 samples of pre-roll (silence) + 10 samples of speech.
                assert_eq!(s.len(), 30);
                assert!(s[..20].iter().all(|&x| x == 0.0));
                assert!(s[20..].iter().all(|&x| x == 0.5));
            }
            other => panic!("expected SendStitched, got {other:?}"),
        }
    }

    #[test]
    fn single_process_call_can_span_multiple_transitions() {
        let mut gate = SilenceGate::new(test_config());
        // First half: 50 samples of silence (continuing the initial Silent state).
        // Second half: 50 samples of speech (triggers Silent -> Speaking mid-call).
        let mut input = silence(50);
        input.extend(speech(50));
        let actions = gate.process(&input);

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, GateAction::Drop { .. })),
            "expected a Drop for the leading silence: {actions:?}"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, GateAction::SendStitched(_))),
            "expected a SendStitched for the speech onset: {actions:?}"
        );
        // Order matters: the drop (leading silence) must precede the stitched send.
        let drop_idx = actions
            .iter()
            .position(|a| matches!(a, GateAction::Drop { .. }))
            .unwrap();
        let send_idx = actions
            .iter()
            .position(|a| matches!(a, GateAction::SendStitched(_)))
            .unwrap();
        assert!(drop_idx < send_idx);
    }

    #[test]
    fn no_pre_roll_double_counting_across_a_full_cycle() {
        let mut gate = SilenceGate::new(test_config());
        // Silence, then speech, then silence again, then speech again — exercise
        // pre-roll fill/evict/drain repeatedly.
        let mut all_actions: Vec<u64> = Vec::new();
        let mut total_input = 0usize;
        for chunk in [silence(35), speech(40), silence(60), speech(15)] {
            total_input += chunk.len();
            let actions = gate.process(&chunk);
            all_actions.push(total_sent_and_dropped(&actions));
        }
        // Whatever's still sitting in the pre-roll ring at the very end hasn't been
        // reported as Drop yet (by design — see push_pre_roll's docs) and also was
        // never sent, so it's the one legitimate gap between input and
        // sent-plus-dropped.
        let accounted: u64 = all_actions.iter().sum();
        let still_buffered = gate.pre_roll.len() as u64;
        assert_eq!(accounted + still_buffered, total_input as u64);
    }

    #[test]
    fn empty_process_yields_no_actions() {
        let mut gate = SilenceGate::new(test_config());
        assert!(gate.process(&[]).is_empty());
    }

    #[test]
    fn rms_of_silence_is_zero_and_of_a_tone_is_its_amplitude() {
        assert_eq!(rms(&silence(10)), 0.0);
        assert!((rms(&speech(10)) - 0.5).abs() < 1e-6);
        assert_eq!(rms(&[]), 0.0);
    }
}
