//! Hand-built minimal TTA bitstream — exercises the entropy decoder
//! and stage-A/B predictor cascade end-to-end, without going through
//! the `ffmpeg` encoder. This is the smoke test that runs even on
//! machines without `ffmpeg` installed.

use oxideav_core::bits::BitWriterLsb;
use oxideav_core::SampleFormat;
use oxideav_tta::crc::crc32;
use oxideav_tta::decoder::{decode_one_frame, RICE_INIT_K};
use oxideav_tta::header::{TtaHeader, HEADER_LEN, SIGNATURE};

fn build_header(channels: u16, bps: u16, sample_rate: u32, total_samples: u32) -> Vec<u8> {
    let mut h = vec![0u8; HEADER_LEN];
    h[0..4].copy_from_slice(SIGNATURE);
    h[4..6].copy_from_slice(&1u16.to_le_bytes());
    h[6..8].copy_from_slice(&channels.to_le_bytes());
    h[8..10].copy_from_slice(&bps.to_le_bytes());
    h[10..14].copy_from_slice(&sample_rate.to_le_bytes());
    h[14..18].copy_from_slice(&total_samples.to_le_bytes());
    let crc = crc32(&h[0..18]);
    h[18..22].copy_from_slice(&crc.to_le_bytes());
    h
}

#[test]
fn header_round_trip_basic_fields() {
    let h = build_header(2, 16, 48_000, 12_000);
    let parsed = TtaHeader::parse(&h).unwrap();
    assert_eq!(parsed.channels, 2);
    assert_eq!(parsed.bits_per_sample, 16);
    assert_eq!(parsed.sample_rate, 48_000);
    assert_eq!(parsed.total_samples, 12_000);
}

/// Build a minimal one-frame TTA-style stream: depth-0 Rice values
/// only (no escape branch), with `k0=k1=10` initial parameters as TTA
/// always uses. Each sample is encoded as: terminator bit '0',
/// followed by a 10-bit suffix carrying the **un-zig-zag-encoded**
/// residual. Stage-A and Stage-B run with all-zero initial state, so
/// the first sample reconstructs to (residual + 0 + 0) = residual.
///
/// We cannot easily predict the multi-sample reconstruction without
/// a separate encoder model, so we keep this test to: build a single
/// sample and assert the decoder produces the matching i16 PCM byte.
#[test]
fn single_sample_decode_sanity() {
    // We pick value=3 in the un-zigzag domain → un_zigzag(3) = +2.
    // With all-zero predictor state, the recovered sample is +2.
    // 16-bit output: byte sequence [0x02, 0x00] little-endian.
    let mut bw = BitWriterLsb::new();
    bw.write_u32(0, 1); // depth-0 terminator
    bw.write_u32(3, RICE_INIT_K); // 10-bit suffix
    bw.align_to_byte();
    let mut body = bw.finish();
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());

    // 1 sample, 1 channel, 16-bit, sample_rate that yields a >=1
    // frame. We use sample_rate = 245 which makes frame_size = 256
    // (large enough to be valid). total_samples = 1 makes
    // last_frame_size = 1.
    let h = build_header(1, 16, 245, 1);
    let parsed = TtaHeader::parse(&h).unwrap();

    let frame = decode_one_frame(&body, &parsed, SampleFormat::S16, None).expect("frame decode");
    let oxideav_core::Frame::Audio(af) = frame else {
        panic!("expected audio frame")
    };
    assert_eq!(af.samples, 1);
    assert_eq!(af.data[0], vec![0x02, 0x00]);
}
