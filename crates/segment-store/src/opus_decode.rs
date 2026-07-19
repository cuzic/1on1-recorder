//! Decodes committed `.opus` (Ogg Opus) segment files back into 48kHz mono PCM(f32),
//! for on-demand re-transcription of previously recorded audio. Mirrors
//! `opus_ogg::encode_segment_to_ogg_opus` in reverse: skips the `OpusHead`/`OpusTags`
//! header packets (identified by their RFC 7845 magic prefixes, same as the encoder
//! writes them) and decodes every remaining packet with a single `opus::Decoder`.

use std::fs::File;
use std::path::Path;

use ogg::reading::PacketReader;
use opus::{Channels, Decoder};

use crate::error::SegmentStoreError;
use crate::opus_ogg::SAMPLE_RATE_HZ;

/// Largest legal Opus frame size (120ms @ 48kHz) — an upper bound on the samples
/// `decode_float` can produce for a single packet, regardless of the encoder's
/// (smaller, fixed) frame size.
const MAX_DECODED_SAMPLES_PER_PACKET: usize = 5760;

/// Decodes one committed `.opus` file (as produced by `encode_segment_to_ogg_opus` /
/// `commit_segment`) back into 48kHz mono PCM(f32). Lossy: the result is close to,
/// but not bit-identical with, the PCM that was originally encoded.
pub fn decode_segment_to_pcm(path: &Path) -> Result<Vec<f32>, SegmentStoreError> {
    let file = File::open(path)?;
    let mut reader = PacketReader::new(file);
    let mut decoder = Decoder::new(SAMPLE_RATE_HZ, Channels::Mono)?;
    let mut pcm = Vec::new();
    let mut decode_buf = [0.0f32; MAX_DECODED_SAMPLES_PER_PACKET];

    while let Some(packet) = reader.read_packet()? {
        if packet.data.starts_with(b"OpusHead") || packet.data.starts_with(b"OpusTags") {
            continue;
        }
        let n = decoder.decode_float(&packet.data, &mut decode_buf, false)?;
        pcm.extend_from_slice(&decode_buf[..n]);
    }

    Ok(pcm)
}

/// Decodes and concatenates multiple committed `.opus` files, in the given order,
/// into a single 48kHz mono PCM(f32) buffer — for reconstructing audio spanning
/// several segments (e.g. a manual re-transcription request whose time range crosses
/// segment boundaries). Segments are decoded whole; trimming to an exact sample range
/// within the boundary segments is the caller's responsibility.
pub fn decode_segments_to_pcm<P: AsRef<Path>>(paths: &[P]) -> Result<Vec<f32>, SegmentStoreError> {
    let mut pcm = Vec::new();
    for path in paths {
        pcm.extend(decode_segment_to_pcm(path.as_ref())?);
    }
    Ok(pcm)
}
