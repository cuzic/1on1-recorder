//! Maps provider-relative STT timestamps (`SttEvent::audio_start_ms`/`audio_end_ms`,
//! see `stt-api`) back to wall-clock milliseconds.
//!
//! Streaming STT providers compute `audio_start_ms`/`audio_end_ms` from the amount of
//! audio actually sent to them over the wire, not from real elapsed time. Two features
//! of the (in-progress) silence-skipping work break that assumption in opposite
//! directions:
//!
//! - Skipping silent audio (never sending it to the provider) means fewer samples are
//!   sent than were captured, so provider timestamps run *behind* wall-clock time.
//! - Some providers (Google/OpenAI) need synthetic "heartbeat" audio sent during long
//!   silences to avoid an idle-connection timeout; those extra samples were never
//!   actually captured, so provider timestamps run *ahead* of wall-clock time.
//!
//! `TimestampMapper` tracks a caller-recorded checkpoint series of
//! `(provider_samples_sent, net_offset_samples)` — where `net_offset_samples =
//! captured_samples_dropped - artificial_samples_injected` — and uses it to correct a
//! provider-relative millisecond value back to wall-clock.

/// Converts provider-relative millisecond timestamps to wall-clock milliseconds,
/// given a series of checkpoints recording how far provider-sent audio has drifted
/// from captured (wall-clock) audio.
pub struct TimestampMapper {
    /// `(cumulative provider_samples_sent, net_offset_samples at that point)`,
    /// appended in caller order. The caller records `provider_samples_sent`
    /// monotonically increasing, so this Vec is naturally sorted by that field —
    /// no explicit sort is needed here.
    checkpoints: Vec<(u64, i64)>,
    sample_rate_hz: u32,
}

impl TimestampMapper {
    pub fn new(sample_rate_hz: u32) -> Self {
        Self { checkpoints: Vec::new(), sample_rate_hz }
    }

    /// Records a checkpoint: as of `provider_samples_sent` cumulative samples having
    /// been sent to the provider, the net offset between captured and sent audio is
    /// `net_offset_samples` (`captured_samples_dropped - artificial_samples_injected`,
    /// signed). Callers must call this with a non-decreasing `provider_samples_sent`.
    ///
    /// A run of calls sharing the same `provider_samples_sent` (e.g. several
    /// `GateAction::Drop`s in a row during a long silence, where nothing is actually
    /// sent so the position doesn't advance) overwrites the last checkpoint in place
    /// rather than appending a new one — `to_wallclock_ms`'s binary search only ever
    /// looks at the *last* checkpoint for a given position anyway (see its doc
    /// comment), so the earlier entries for the same position would never be
    /// observably different, just extra `Vec` growth and search candidates for the
    /// lifetime of a long recording.
    pub fn record_checkpoint(&mut self, provider_samples_sent: u64, net_offset_samples: i64) {
        if let Some(last) = self.checkpoints.last_mut() {
            if last.0 == provider_samples_sent {
                last.1 = net_offset_samples;
                return;
            }
        }
        self.checkpoints.push((provider_samples_sent, net_offset_samples));
    }

    /// Corrects a provider-relative millisecond timestamp (`audio_start_ms`/
    /// `audio_end_ms`) to wall-clock milliseconds.
    ///
    /// Converts `provider_relative_ms` to a sample count, binary-searches the
    /// checkpoint series (sorted by `provider_samples_sent`) for the applicable
    /// `net_offset_samples`, converts *that* to milliseconds (once, here, to avoid
    /// accumulating per-checkpoint rounding error), and adds it to
    /// `provider_relative_ms`.
    ///
    /// Returns `provider_relative_ms` unchanged (zero offset) if there are no
    /// checkpoints yet, or if `provider_relative_ms` predates the first checkpoint.
    pub fn to_wallclock_ms(&self, provider_relative_ms: u64) -> u64 {
        if self.checkpoints.is_empty() {
            return provider_relative_ms;
        }

        let provider_relative_samples = ms_to_samples(provider_relative_ms, self.sample_rate_hz);

        // Find the last checkpoint whose `provider_samples_sent` is <= the query
        // point: that's the most recent offset known to apply at this timestamp.
        // `partition_point` returns the index of the first checkpoint whose
        // `provider_samples_sent` exceeds the query, so the applicable checkpoint
        // (if any) is one before that.
        let idx = self.checkpoints.partition_point(|&(sent, _)| sent <= provider_relative_samples);

        let net_offset_samples = match idx {
            0 => 0, // predates the first checkpoint: no correction yet.
            _ => self.checkpoints[idx - 1].1,
        };

        let offset_ms = signed_samples_to_ms(net_offset_samples, self.sample_rate_hz);
        provider_relative_ms.saturating_add_signed(offset_ms)
    }
}

fn ms_to_samples(ms: u64, sample_rate_hz: u32) -> u64 {
    ms * sample_rate_hz as u64 / 1000
}

/// Converts a signed sample count to signed milliseconds, rounding to the nearest
/// millisecond (ties away from zero) rather than truncating toward zero. Plain `/`
/// truncation would silently under-correct by up to ~1ms whenever `samples` isn't an
/// exact multiple of `sample_rate_hz / 1000` — e.g. -15_999 samples at 16_000Hz is
/// -999.9375ms, and truncating division yields -999 instead of the correct -1000,
/// leaving a stale 1ms in the result. Also guards against the (not expected in
/// practice) case of the conversion pushing a small negative sample count's
/// millisecond value below what `saturating_add_signed` can absorb by simply
/// carrying the sign through unchanged; the caller (`to_wallclock_ms`) uses
/// `saturating_add_signed` so the final result never goes below 0 even if
/// `net_offset_samples` were unexpectedly negative.
fn signed_samples_to_ms(samples: i64, sample_rate_hz: u32) -> i64 {
    let rate = sample_rate_hz as i64;
    let numerator = samples * 1000;
    if numerator >= 0 {
        (numerator + rate / 2) / rate
    } else {
        (numerator - rate / 2) / rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_checkpoints_at_the_same_position_are_compacted_in_place() {
        // Regression test: a run of `record_checkpoint` calls sharing the same
        // `provider_samples_sent` (as happens for a series of `GateAction::Drop`s
        // during one long silence, since nothing sent means the position doesn't
        // advance) must not grow the checkpoint list — only the *last* offset for a
        // given position is ever observable via `to_wallclock_ms`, so appending a
        // new entry each time would be unbounded memory growth for no behavioral
        // difference.
        let mut mapper = TimestampMapper::new(16_000);
        for dropped in [1_600u64, 3_200, 4_800] {
            mapper.record_checkpoint(5_000, dropped as i64);
        }
        assert_eq!(mapper.checkpoints.len(), 1, "same-position checkpoints should overwrite, not accumulate");

        // A later, different position still appends normally.
        mapper.record_checkpoint(6_000, 6_400);
        assert_eq!(mapper.checkpoints.len(), 2);

        // A query between the two positions (5_600 samples = 350ms) picks up the
        // final recorded offset for position 5_000 (4_800 samples = 300ms), not an
        // earlier value from the same compacted run.
        assert_eq!(mapper.to_wallclock_ms(350), 350 + 300);
    }

    #[test]
    fn no_checkpoints_returns_input_unchanged() {
        let mapper = TimestampMapper::new(16_000);
        assert_eq!(mapper.to_wallclock_ms(0), 0);
        assert_eq!(mapper.to_wallclock_ms(12_345), 12_345);
    }

    #[test]
    fn single_silence_drop_offsets_subsequent_timestamps() {
        let mut mapper = TimestampMapper::new(16_000);
        // 1 second of silence dropped (16_000 samples), recorded once provider has
        // been sent 5_000 samples (i.e. 312.5ms of provider-relative audio).
        mapper.record_checkpoint(5_000, 16_000);

        // Before the checkpoint (100ms = 1_600 samples < 5_000): no correction yet.
        assert_eq!(mapper.to_wallclock_ms(100), 100);

        // After the checkpoint (1_000ms = 16_000 samples >= 5_000): offset by the
        // dropped second.
        assert_eq!(mapper.to_wallclock_ms(1_000), 1_000 + 1_000);
    }

    #[test]
    fn multiple_silence_regions_pick_the_right_checkpoint_for_start_and_end() {
        let mut mapper = TimestampMapper::new(16_000);
        // First silence: 500ms dropped, recorded at provider_samples_sent = 16_000
        // (i.e. 1s of provider-relative audio sent so far).
        mapper.record_checkpoint(16_000, 8_000); // 500ms worth of samples
        // Second silence: another 300ms dropped, recorded at provider_samples_sent =
        // 32_000 (2s of provider-relative audio sent so far). Net offset accumulates.
        mapper.record_checkpoint(32_000, 8_000 + 4_800); // +300ms worth of samples

        // A transcript segment that starts within the first checkpoint's window and
        // ends within the second's.
        let start_provider_ms = 1_500; // samples = 24_000, in [16_000, 32_000)
        let end_provider_ms = 2_500; // samples = 40_000, >= 32_000

        let start_wall_ms = mapper.to_wallclock_ms(start_provider_ms);
        let end_wall_ms = mapper.to_wallclock_ms(end_provider_ms);

        assert_eq!(start_wall_ms, start_provider_ms + 500);
        assert_eq!(end_wall_ms, end_provider_ms + 800);
    }

    #[test]
    fn heartbeat_injection_reduces_offset() {
        let mut mapper = TimestampMapper::new(16_000);
        // 1s dropped, but 400ms of heartbeat injected in the same window: net offset
        // is drop - injected = 600ms worth of samples.
        let dropped_samples: i64 = 16_000;
        let injected_samples: i64 = 6_400; // 400ms
        mapper.record_checkpoint(5_000, dropped_samples - injected_samples);

        assert_eq!(mapper.to_wallclock_ms(1_000), 1_000 + 600);
    }

    #[test]
    fn heartbeat_only_region_offsets_backward() {
        let mut mapper = TimestampMapper::new(16_000);
        // No captured audio dropped, but heartbeat injected: net offset is negative,
        // meaning provider timestamps run ahead of wall-clock and must be pulled back.
        let injected_samples: i64 = 8_000; // 500ms
        mapper.record_checkpoint(5_000, -injected_samples);

        assert_eq!(mapper.to_wallclock_ms(1_000), 1_000 - 500);
    }

    #[test]
    fn long_leading_silence_then_multiple_heartbeats_then_speech() {
        // The trickiest real scenario per the design: a session opens, sits silent
        // for a long time (skipped entirely, never sent), during which several
        // heartbeat chunks are injected to keep the provider connection alive, and
        // only then does real speech begin. Verify the final offset after all of
        // that checkpoint history is exactly drop-so-far minus injected-so-far.
        let mut mapper = TimestampMapper::new(16_000);

        // 10s of leading silence is captured but never sent (fully dropped).
        let total_dropped_samples: i64 = 16_000 * 10;

        // Across that silence, 3 heartbeat chunks of 200ms each are injected to the
        // provider (so provider_samples_sent does advance, just not with real audio).
        let heartbeat_samples: i64 = 16_000 * 200 / 1000; // 3_200 samples per beat

        // Checkpoint 1: after first heartbeat sent (provider has been sent 3_200
        // samples so far). Silence dropped-so-far is still the full 10s (all of it
        // predates any sending), heartbeat injected-so-far is 1 beat.
        mapper.record_checkpoint(3_200, total_dropped_samples - heartbeat_samples);
        // Checkpoint 2: after second heartbeat.
        mapper.record_checkpoint(6_400, total_dropped_samples - 2 * heartbeat_samples);
        // Checkpoint 3: after third heartbeat, silence region ends here.
        mapper.record_checkpoint(9_600, total_dropped_samples - 3 * heartbeat_samples);

        // Speech begins right after: a transcript at provider_relative_ms
        // corresponding to, say, 9_600 + 4_800 = 14_400 samples sent (900ms).
        let provider_relative_ms = 900;
        let expected_offset_samples = total_dropped_samples - 3 * heartbeat_samples;
        let expected_offset_ms = expected_offset_samples * 1000 / 16_000;

        assert_eq!(
            mapper.to_wallclock_ms(provider_relative_ms),
            (provider_relative_ms as i64 + expected_offset_ms) as u64
        );

        // Sanity: with 10s dropped and only 600ms injected total, the net offset
        // should be a large positive number (timestamps run well behind wall-clock).
        assert!(expected_offset_ms > 9_000);
    }
}
