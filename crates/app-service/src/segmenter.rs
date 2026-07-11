//! Cuts one track's aligned PCM into fixed-duration chunks, numbered by a `sequence`
//! that's meaningful *across* tracks: because `Self` and `Remote` are each aligned to
//! the same host-clock timeline at the same nominal rate (see `timeline_adapter`),
//! cutting both at the same sample offsets means sequence N always covers the same
//! `timeline_start_ms` window in both tracks.

/// One fixed-duration (or, for the last one, however-much-is-left) slice of aligned
/// PCM, ready to hand to `segment_store::encode_segment_to_ogg_opus`.
pub struct PendingSegment<'a> {
    pub sequence: u64,
    pub timeline_start_ms: u64,
    pub pcm: &'a [f32],
}

pub fn segment_pcm(pcm: &[f32], sample_rate: u32, segment_duration_ms: u32) -> Vec<PendingSegment<'_>> {
    let samples_per_segment = (sample_rate as u64 * segment_duration_ms as u64 / 1000) as usize;
    if samples_per_segment == 0 || pcm.is_empty() {
        return Vec::new();
    }

    pcm.chunks(samples_per_segment)
        .enumerate()
        .map(|(i, chunk)| PendingSegment { sequence: i as u64, timeline_start_ms: i as u64 * segment_duration_ms as u64, pcm: chunk })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_multiple_produces_equal_length_segments() {
        let pcm = vec![0.0f32; 300];
        let segments = segment_pcm(&pcm, 10, 10_000); // 100 samples/segment
        assert_eq!(segments.len(), 3);
        for (i, s) in segments.iter().enumerate() {
            assert_eq!(s.sequence, i as u64);
            assert_eq!(s.timeline_start_ms, i as u64 * 10_000);
            assert_eq!(s.pcm.len(), 100);
        }
    }

    #[test]
    fn a_remainder_becomes_a_shorter_trailing_segment() {
        let pcm = vec![0.0f32; 250];
        let segments = segment_pcm(&pcm, 10, 10_000);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[2].pcm.len(), 50);
    }

    #[test]
    fn empty_input_produces_no_segments() {
        assert!(segment_pcm(&[], 48_000, 30_000).is_empty());
    }
}
