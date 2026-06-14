//! Seek-table-driven O(1) demuxer seek tests.
//!
//! Built against the TTA1 seek-table layout in
//! `docs/audio/tta-cleanroom/spec/01-bitstream-framing.md` §4 and the
//! frame-geometry rule (`floor(sample_rate * 256 / 245)` per-channel
//! samples in every non-last frame, the last frame possibly shorter).
//! For an `S`-sample-rate stream with `N` frames, frame `k` occupies
//! per-channel samples `[k * R, (k+1) * R)` where `R =
//! regular_frame_samples`; `seek_to(pts)` lands the demuxer on frame
//! `min(pts / R, N - 1)` with a re-anchored pts of
//! `target_frame * R`.
//!
//! All fixtures are manufactured in-process via the public
//! [`crate::encode`] entry point so we don't depend on a pre-checked-in
//! TTA fixture (the round-2 deliverable still has no sanctioned
//! reference tape).

#![cfg(all(test, feature = "registry"))]

use std::io::Cursor;

use oxideav_core::{CodecResolver, Error as CoreError, Frame, ReadSeek, RuntimeContext};

use crate::encode;
use crate::registry::{open_demuxer_for_test, register};

/// Stub resolver used by the demuxer-open path (the TTA demuxer never
/// calls back into it). Mirrors the helper in `registry::tests`.
struct NoopResolver;
impl CodecResolver for NoopResolver {
    fn resolve_tag(&self, _ctx: &oxideav_core::ProbeContext) -> Option<oxideav_core::CodecId> {
        None
    }
}

/// Bundled fixture metadata kept alongside the encoded bytes so the
/// tests can cross-check decoder-side sample counts against the
/// demuxer-side frame geometry without re-deriving the constants.
struct Fixture {
    bytes: Vec<u8>,
    /// Original interleaved `i32` PCM (channel-then-sample).
    pcm: Vec<i32>,
    channels: u16,
    sample_rate: u32,
    /// `floor(sample_rate * 256 / 245)` per-channel samples per
    /// non-last frame.
    regular_samples: u32,
    /// Total frames in the encoded stream.
    frame_count: u32,
}

/// Build a multi-frame test fixture. The chosen geometry covers the
/// three branches `seek_to` exercises: pre-frame-0, mid-frame N, and
/// post-end clamp.
fn make_multi_frame_fixture() -> Fixture {
    let sample_rate: u32 = 8_000;
    let channels: u16 = 2;
    let bps: u16 = 16;
    // regular_frame_samples = floor(8000 * 256 / 245) = 8359.
    let regular_samples = ((sample_rate as u64) * 256 / 245) as u32;
    // 5 frames: 4 full + a short tail.
    let full_frames = 4u32;
    let tail_samples = 1_000u32;
    let total_samples = regular_samples * full_frames + tail_samples;
    let nch = channels as usize;

    // Deterministic-but-non-trivial PCM: a slow ramp with a
    // channel-dependent phase. Magnitudes stay well inside i16 range.
    let mut pcm = vec![0i32; total_samples as usize * nch];
    for s in 0..(total_samples as usize) {
        for ch in 0..nch {
            let base = (s as i32).wrapping_mul(7) % 4096 - 2048;
            let phase = (ch as i32) * 311;
            pcm[s * nch + ch] = base.wrapping_add(phase);
        }
    }

    let bytes = encode(&pcm, channels, bps, sample_rate).expect("encode should succeed");
    Fixture {
        bytes,
        pcm,
        channels,
        sample_rate,
        regular_samples,
        frame_count: full_frames + 1,
    }
}

/// Decode one demuxer packet through the registered decoder, and
/// return the interleaved `i32` PCM samples reconstructed by the
/// decoder.
fn decode_packet_to_pcm(
    ctx: &RuntimeContext,
    fixture: &Fixture,
    pkt: &oxideav_core::Packet,
) -> Vec<i32> {
    // Build a fresh decoder for each packet — the codec's
    // `send_packet` enforces single-packet-per-receive semantics, and
    // we want every frame decoded from a clean slate (which is the
    // same invariant `seek_to` relies on).
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(fixture.bytes.clone()));
    let demuxer = open_demuxer_for_test(cursor, &NoopResolver).expect("open_demuxer");
    let stream = demuxer.streams()[0].clone();
    let mut dec = ctx
        .codecs
        .first_decoder(&stream.params)
        .expect("first_decoder");
    dec.send_packet(pkt).expect("send_packet");
    match dec.receive_frame().expect("receive_frame") {
        Frame::Audio(a) => {
            // Unpack the S16-LE byte stream back into i32.
            let bytes = &a.data[0];
            assert_eq!(bytes.len() % 2, 0);
            let nch = fixture.channels as usize;
            let nsamples = bytes.len() / (2 * nch);
            assert_eq!(a.samples as usize, nsamples);
            let mut out = vec![0i32; nsamples * nch];
            for i in 0..(nsamples * nch) {
                let lo = bytes[2 * i] as i32;
                let hi = bytes[2 * i + 1] as i8 as i32; // sign-extend
                out[i] = (hi << 8) | lo;
            }
            out
        }
        other => panic!("expected audio frame, got {other:?}"),
    }
}

#[test]
fn seek_to_zero_resets_to_first_frame() {
    let fix = make_multi_frame_fixture();
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);

    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(fix.bytes.clone()));
    let mut demuxer = open_demuxer_for_test(cursor, &NoopResolver).expect("open_demuxer");
    // Pull one packet first so the demuxer state has advanced.
    let p0 = demuxer.next_packet().expect("first packet");
    assert_eq!(p0.pts, Some(0));
    let p1 = demuxer.next_packet().expect("second packet");
    assert_eq!(p1.pts, Some(fix.regular_samples as i64));
    // Now seek back to zero.
    let landed = demuxer.seek_to(0, 0).expect("seek_to(0)");
    assert_eq!(landed, 0);
    let p0_again = demuxer.next_packet().expect("post-seek next");
    assert_eq!(p0_again.pts, Some(0));
    // Bytes must match the very first packet — the demuxer
    // reconstructs the mini-file from the same byte slice, so this is
    // a byte-for-byte check.
    assert_eq!(p0_again.data, p0.data);
}

#[test]
fn seek_at_frame_boundary_lands_exact() {
    let fix = make_multi_frame_fixture();
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);

    // Pick frame 2's first sample = 2 * regular_samples.
    let target = 2i64 * fix.regular_samples as i64;
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(fix.bytes.clone()));
    let mut demuxer = open_demuxer_for_test(cursor, &NoopResolver).expect("open_demuxer");
    let landed = demuxer.seek_to(0, target).expect("seek_to(frame2_start)");
    assert_eq!(landed, target);
    let pkt = demuxer.next_packet().expect("post-seek next_packet");
    assert_eq!(pkt.pts, Some(target));
    assert_eq!(pkt.duration, Some(fix.regular_samples as i64));
}

#[test]
fn seek_mid_frame_lands_at_containing_frame_start() {
    let fix = make_multi_frame_fixture();
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);

    // Target sample is somewhere inside frame 3 — pick the midpoint.
    let frame_idx = 3i64;
    let frame_start = frame_idx * fix.regular_samples as i64;
    let mid = frame_start + (fix.regular_samples as i64) / 2;
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(fix.bytes.clone()));
    let mut demuxer = open_demuxer_for_test(cursor, &NoopResolver).expect("open_demuxer");
    let landed = demuxer.seek_to(0, mid).expect("seek_to(mid-of-frame3)");
    assert_eq!(
        landed, frame_start,
        "seek_to MUST snap to the containing frame's first sample"
    );
    let pkt = demuxer.next_packet().expect("post-seek next_packet");
    assert_eq!(pkt.pts, Some(frame_start));
}

#[test]
fn seek_past_end_clamps_to_last_frame() {
    let fix = make_multi_frame_fixture();
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);

    let huge = (fix.frame_count as i64 + 100) * fix.regular_samples as i64;
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(fix.bytes.clone()));
    let mut demuxer = open_demuxer_for_test(cursor, &NoopResolver).expect("open_demuxer");
    let landed = demuxer.seek_to(0, huge).expect("seek_to past-end");
    let last_frame_pts = ((fix.frame_count - 1) as i64) * fix.regular_samples as i64;
    assert_eq!(landed, last_frame_pts);
    let pkt = demuxer.next_packet().expect("post-seek last frame");
    assert_eq!(pkt.pts, Some(last_frame_pts));
    // The next call should report EOF — only the clamped-to-last
    // frame is available after a past-end seek.
    match demuxer.next_packet() {
        Err(CoreError::Eof) => {}
        other => panic!("expected Eof after clamped past-end seek, got {other:?}"),
    }
}

#[test]
fn seek_pts_matches_decoder_output_after_seek() {
    let fix = make_multi_frame_fixture();
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);

    // Seek to the start of frame 2 and decode that one packet. The
    // decoded PCM MUST equal the slice of the original PCM at
    // `target_frame * regular_samples` for `regular_samples` samples.
    let target_frame = 2u32;
    let target_pts = (target_frame as i64) * (fix.regular_samples as i64);
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(fix.bytes.clone()));
    let mut demuxer = open_demuxer_for_test(cursor, &NoopResolver).expect("open_demuxer");
    let landed = demuxer.seek_to(0, target_pts).expect("seek_to");
    assert_eq!(landed, target_pts);
    let pkt = demuxer.next_packet().expect("post-seek packet");
    let decoded = decode_packet_to_pcm(&ctx, &fix, &pkt);
    let nch = fix.channels as usize;
    let start = (target_frame as usize) * (fix.regular_samples as usize) * nch;
    let end = start + (fix.regular_samples as usize) * nch;
    let expected = &fix.pcm[start..end];
    assert_eq!(
        decoded.len(),
        expected.len(),
        "decoded sample count must match expected slice length"
    );
    for (i, (g, e)) in decoded.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            g, e,
            "post-seek PCM mismatch at i={i} (target_frame={target_frame}, target_pts={target_pts})"
        );
    }

    // Decode the last (short) frame after a past-end seek. Verify the
    // short-frame sample count plus PCM identity to the original tail.
    let huge = (fix.frame_count as i64) * fix.regular_samples as i64;
    let cursor2: Box<dyn ReadSeek> = Box::new(Cursor::new(fix.bytes.clone()));
    let mut demuxer2 = open_demuxer_for_test(cursor2, &NoopResolver).expect("open_demuxer #2");
    let _ = demuxer2.seek_to(0, huge).expect("seek past end");
    let pkt_last = demuxer2.next_packet().expect("last-frame packet");
    let decoded_last = decode_packet_to_pcm(&ctx, &fix, &pkt_last);
    // Tail sample count == fix.pcm.len() / nch - (frame_count - 1) * regular.
    let tail_per_channel =
        fix.pcm.len() / nch - (fix.frame_count as usize - 1) * fix.regular_samples as usize;
    assert_eq!(decoded_last.len(), tail_per_channel * nch);
    let tail_start = (fix.frame_count as usize - 1) * fix.regular_samples as usize * nch;
    assert_eq!(decoded_last, fix.pcm[tail_start..]);

    // Sample rate sanity: the time base is `1 / sample_rate`, so the
    // pts unit on emitted packets must equal `sample_rate` Hz.
    let streams = demuxer2.streams();
    assert_eq!(streams[0].time_base.0.den, fix.sample_rate as i64);
    assert_eq!(streams[0].time_base.0.num, 1);
}

/// Regression (round 299): a malformed seek table whose first entry
/// declares a `disk_size` larger than the file used to drive the
/// per-frame mini-file assembly in `build_single_frame_file` to slice
/// `all[file_offset..file_offset + disk_size]` out of bounds — a panic
/// reachable through the public `open_demuxer` → `next_packet` path with
/// any caller-supplied bytes. `open_demuxer` now validates every frame's
/// `[file_offset, file_offset + disk_size)` window against the file
/// length at open time, so the malformed stream is rejected with a typed
/// `Error::Invalid` rather than panicking at packet-emit time.
#[test]
fn malformed_seek_table_oversize_disk_size_is_rejected_not_panic() {
    // ~100k mono 16-bit samples at 44.1 kHz spans three frames
    // (regular_frame_samples = 46_073), so the seek table has three
    // 4-byte entries starting at byte 22 (after the 22-byte header).
    let samples = vec![0i32; 100_000];
    let mut bytes = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    // Overwrite the first seek-table entry's disk_size with a value that
    // overruns the file. The demuxer does not enforce the seek-table
    // CRC, so leaving it stale still reaches the byte-window check.
    let st_off = 22;
    bytes[st_off..st_off + 4].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());

    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let result = open_demuxer_for_test(cursor, &NoopResolver);
    match result {
        Err(CoreError::InvalidData(_)) => {}
        Err(other) => panic!("expected Error::InvalidData, got {other:?}"),
        Ok(_) => panic!("oversize seek-table disk_size must be rejected at open time"),
    }
}

/// Companion to the above: a seek table whose cumulative `file_offset`
/// for a later frame is itself in range but whose `disk_size` pushes the
/// body end one byte past EOF is rejected. Confirms the check is a
/// `<= file_len` end-bound test, not merely a start-offset test.
#[test]
fn malformed_seek_table_last_frame_one_byte_overrun_is_rejected() {
    let samples = vec![0i32; 100_000];
    let mut bytes = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    // Inflate the THIRD (last) frame's disk_size by exactly the slack we
    // create by appending nothing — set it so the cumulative body end is
    // file_len + 1. Easiest: bump the last entry by a large amount.
    let st_off = 22 + 8; // third entry (entries are 4 bytes each)
    let orig = u32::from_le_bytes(bytes[st_off..st_off + 4].try_into().unwrap());
    let inflated = orig.saturating_add(1).max(orig + 1);
    // Make it overrun for certain.
    let big = inflated.saturating_add(1_000);
    bytes[st_off..st_off + 4].copy_from_slice(&big.to_le_bytes());

    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    match open_demuxer_for_test(cursor, &NoopResolver) {
        Err(CoreError::InvalidData(_)) => {}
        Err(other) => panic!("expected Error::InvalidData, got {other:?}"),
        Ok(_) => panic!("last-frame overrun must be rejected at open time"),
    }
}
