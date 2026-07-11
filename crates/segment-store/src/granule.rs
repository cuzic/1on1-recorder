use std::fs::File;
use std::path::Path;

use ogg::reading::PacketReader;

use crate::error::SegmentStoreError;
use crate::opus_ogg::SAMPLE_RATE_HZ;

/// Reads the last packet's absolute granule position out of a committed Ogg Opus
/// file — the total sample count already encoded into it, since
/// `opus_ogg::encode_segment_to_ogg_opus` sets `granule_pos += FRAME_SAMPLES` after
/// every frame. Lets recovery reconstruct a segment's exact duration without decoding
/// any audio.
pub fn read_total_samples(path: &Path) -> Result<u64, SegmentStoreError> {
    let file = File::open(path)?;
    let mut reader = PacketReader::new(file);
    let mut last_granule = 0u64;
    while let Some(packet) = reader.read_packet()? {
        last_granule = packet.absgp_page();
    }
    Ok(last_granule)
}

pub fn samples_to_ms(samples: u64) -> u32 {
    ((samples * 1000) / SAMPLE_RATE_HZ as u64) as u32
}
