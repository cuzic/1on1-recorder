//! 48kHz mono PCM(f32) -> Opus in an Ogg container. `OpusHead`/`OpusTags` are
//! hand-built per RFC 7845 §5.1/§5.2, since the `ogg`/`opus` crates don't generate
//! container headers on their own.

use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use opus::{Application, Channels, Encoder};
use std::io::Write;

use crate::error::SegmentStoreError;

pub const SAMPLE_RATE_HZ: u32 = 48_000;
/// 20ms @ 48kHz mono.
pub const FRAME_SAMPLES: usize = 960;
const MAX_PACKET_BYTES: usize = 4000;

fn opus_head_packet() -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(1); // channel count (mono)
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&SAMPLE_RATE_HZ.to_le_bytes()); // input sample rate (informational)
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    head
}

fn opus_tags_packet() -> Vec<u8> {
    let vendor = b"segment-store";
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    tags
}

/// Encodes 48kHz mono f32 PCM into an Ogg Opus byte stream. A trailing partial frame
/// is zero-padded to `FRAME_SAMPLES`. The final packet's granule position (total
/// encoded sample count) is what `recovery::read_total_samples` later reads back to
/// reconstruct a segment's exact duration without decoding audio.
pub fn encode_segment_to_ogg_opus(pcm: &[f32], bitrate_bps: i32) -> Result<Vec<u8>, SegmentStoreError> {
    let mut encoder = Encoder::new(SAMPLE_RATE_HZ, Channels::Mono, Application::Audio)?;
    encoder.set_bitrate(opus::Bitrate::Bits(bitrate_bps))?;

    let serial: u32 = 0x5347_4d54; // "SGMT"
    let mut buffer: Vec<u8> = Vec::new();
    {
        let mut writer = PacketWriter::new(&mut buffer);
        writer.write_packet(opus_head_packet(), serial, PacketWriteEndInfo::EndPage, 0)?;
        writer.write_packet(opus_tags_packet(), serial, PacketWriteEndInfo::EndPage, 0)?;

        let frame_count = if pcm.is_empty() {
            0
        } else {
            pcm.len().div_ceil(FRAME_SAMPLES)
        };
        let mut granule_pos: u64 = 0;
        let mut encode_buf = vec![0u8; MAX_PACKET_BYTES];

        for i in 0..frame_count {
            let start = i * FRAME_SAMPLES;
            let end = (start + FRAME_SAMPLES).min(pcm.len());
            let mut frame = vec![0.0f32; FRAME_SAMPLES];
            frame[..end - start].copy_from_slice(&pcm[start..end]);

            let n = encoder.encode_float(&frame, &mut encode_buf)?;
            granule_pos += FRAME_SAMPLES as u64;

            let end_info = if i + 1 == frame_count {
                PacketWriteEndInfo::EndStream
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            writer.write_packet(encode_buf[..n].to_vec(), serial, end_info, granule_pos)?;
        }
    }
    let _ = Write::flush(&mut buffer);

    Ok(buffer)
}
