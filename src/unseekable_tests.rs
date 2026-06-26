//! Unseekable-mode discipline tests (`spec/01` §4.3).
//!
//! Per `spec/01-bitstream-framing.md` §4.3: "It is possible to decode a
//! TTA file with a corrupted seek table, but in 'unseekable' mode
//! only." When the seek-table CRC32 fails, the byte offsets in the
//! table can no longer be trusted to land on frame boundaries, so the
//! random-access surface refuses with [`crate::Error::SeekTableUnreliable`]
//! while linear decode ([`crate::Decoder::decode_all`] /
//! [`crate::Decoder::frame_iter`]) continues unaffected.
//!
//! Fixtures are manufactured in-process via the public [`crate::encode`]
//! entry point (the round-2 deliverable still has no sanctioned
//! reference tape), then the seek-table CRC region is byte-corrupted to
//! simulate a damaged table whose entries are nonetheless still readable
//! in stored order (the case where linear decode must still succeed).

#![cfg(test)]

use crate::{encode, Decoder, Error};

/// Build a deterministic multi-frame stereo 16-bit stream and return
/// `(bytes, regular_frame_samples, frame_count, total_samples)`.
fn make_multi_frame_stream() -> (Vec<i32>, Vec<u8>, u32, u32, u32) {
    let sample_rate: u32 = 8_000;
    let channels: u16 = 2;
    let bps: u16 = 16;
    let regular = ((sample_rate as u64) * 256 / 245) as u32; // 8359
    let total_samples = regular * 3 + 777; // 4 frames, short tail
    let nch = channels as usize;
    let mut pcm = vec![0i32; total_samples as usize * nch];
    let mut x: u32 = 0x1234_5678;
    for v in pcm.iter_mut() {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        // Keep magnitudes well inside i16.
        *v = ((x & 0x3FFF) as i32) - 0x2000;
    }
    let bytes = encode(&pcm, channels, bps, sample_rate).expect("encode");
    let frame_count = total_samples.div_ceil(regular);
    (pcm, bytes, regular, frame_count, total_samples)
}

/// Corrupt the seek-table CRC32 in an encoder-produced stream so the
/// table parses (entries are still readable) but fails its CRC check,
/// driving the decoder into unseekable mode. The 22-byte header is
/// followed by `frame_count * 4` entry bytes and then the 4-byte
/// seek-table CRC (`spec/01` §4.2 / §4.3); we flip a bit in that CRC.
fn corrupt_seek_table_crc(bytes: &mut [u8], frame_count: u32) {
    let crc_off = 22 + (frame_count as usize) * 4;
    bytes[crc_off] ^= 0x01;
}

#[test]
fn corrupt_seek_table_makes_stream_unseekable_but_linearly_decodable() {
    let (pcm, mut bytes, _regular, frame_count, _total) = make_multi_frame_stream();

    // Baseline: the pristine stream is seekable and decodes.
    let pristine = Decoder::new(&bytes).expect("pristine parse");
    assert!(pristine.is_seekable(), "pristine stream must be seekable");
    let pristine_pcm = pristine.decode_all().expect("pristine decode");
    assert_eq!(pristine_pcm, pcm, "pristine roundtrip");

    // Corrupt the seek-table CRC.
    corrupt_seek_table_crc(&mut bytes, frame_count);
    let dec = Decoder::new(&bytes).expect("corrupt-table stream must still parse");
    assert!(
        !dec.is_seekable(),
        "stream with a failed seek-table CRC must report unseekable"
    );

    // Linear decode must still succeed and produce the identical PCM:
    // the entries are unchanged (only the CRC byte flipped), so the
    // in-order frame walk reads the same offsets.
    let linear = dec.decode_all().expect("linear decode must continue");
    assert_eq!(linear, pcm, "unseekable-mode linear decode is bit-exact");

    // frame_iter is also linear and must work.
    let mut via_iter = Vec::new();
    for f in dec.frame_iter() {
        via_iter.extend_from_slice(&f.expect("frame_iter must continue in unseekable mode"));
    }
    assert_eq!(via_iter, pcm, "unseekable-mode frame_iter is bit-exact");
}

#[test]
fn random_access_seeks_refuse_in_unseekable_mode() {
    let (_pcm, mut bytes, regular, frame_count, total) = make_multi_frame_stream();
    corrupt_seek_table_crc(&mut bytes, frame_count);
    let dec = Decoder::new(&bytes).expect("parse");
    assert!(!dec.is_seekable());

    // seek_to_sample refuses with the dedicated recoverable error,
    // taking precedence over a range check.
    assert_eq!(dec.seek_to_sample(0), Err(Error::SeekTableUnreliable));
    assert_eq!(
        dec.seek_to_sample(regular as u64),
        Err(Error::SeekTableUnreliable)
    );
    // Even an out-of-range index surfaces the unseekable error first.
    assert_eq!(
        dec.seek_to_sample(total as u64 + 99),
        Err(Error::SeekTableUnreliable)
    );

    // seek_to_time refuses (it funnels through seek_to_sample).
    assert_eq!(
        dec.seek_to_time(core::time::Duration::from_millis(10)),
        Err(Error::SeekTableUnreliable)
    );

    // The from-sample / from-time wrappers refuse.
    assert!(matches!(
        dec.frame_iter_from_sample(100),
        Err(Error::SeekTableUnreliable)
    ));
    assert!(matches!(
        dec.decode_from_sample(100),
        Err(Error::SeekTableUnreliable)
    ));
    assert!(matches!(
        dec.frame_iter_from_time(core::time::Duration::from_millis(5)),
        Err(Error::SeekTableUnreliable)
    ));
    assert!(matches!(
        dec.decode_from_time(core::time::Duration::from_millis(5)),
        Err(Error::SeekTableUnreliable)
    ));

    // Non-empty sample / time range requests refuse (they seek to the
    // leading boundary internally).
    assert!(matches!(
        dec.decode_sample_range(10, 200),
        Err(Error::SeekTableUnreliable)
    ));
    assert!(matches!(
        dec.frame_iter_sample_range(10, 200),
        Err(Error::SeekTableUnreliable)
    ));
    assert!(matches!(
        dec.decode_time_range(
            core::time::Duration::from_millis(1),
            core::time::Duration::from_millis(20)
        ),
        Err(Error::SeekTableUnreliable)
    ));
}

#[test]
fn explicit_index_access_works_in_unseekable_mode() {
    // `decode_frame_at` and `frame_iter_from` walk the table in stored
    // order rather than jumping to a computed offset, so they remain
    // available in unseekable mode (they mirror libtta continuing the
    // linear decode). They must reproduce the same per-frame PCM the
    // pristine stream yields.
    let (_pcm, mut bytes, _regular, frame_count, _total) = make_multi_frame_stream();

    let pristine = Decoder::new(&bytes).expect("pristine");
    let expected: Vec<Vec<i32>> = (0..pristine.frames.len())
        .map(|i| pristine.decode_frame_at(i).expect("pristine frame"))
        .collect();

    corrupt_seek_table_crc(&mut bytes, frame_count);
    let dec = Decoder::new(&bytes).expect("corrupt parse");
    assert!(!dec.is_seekable());

    for (i, exp) in expected.iter().enumerate() {
        let got = dec
            .decode_frame_at(i)
            .expect("explicit-index decode must work in unseekable mode");
        assert_eq!(&got, exp, "frame {i} mismatch via decode_frame_at");
    }

    // frame_iter_from(start) is also index-driven, not seek-driven.
    let mut tail = Vec::new();
    for f in dec.frame_iter_from(1) {
        tail.extend_from_slice(&f.expect("frame_iter_from"));
    }
    let mut expected_tail = Vec::new();
    for exp in &expected[1..] {
        expected_tail.extend_from_slice(exp);
    }
    assert_eq!(
        tail, expected_tail,
        "frame_iter_from tail in unseekable mode"
    );
}

#[test]
fn empty_range_is_ok_even_when_unseekable() {
    // A zero-width range touches no bytes and issues no seek, so it is
    // permitted regardless of seekability — it returns an empty buffer.
    let (_pcm, mut bytes, _regular, frame_count, total) = make_multi_frame_stream();
    corrupt_seek_table_crc(&mut bytes, frame_count);
    let dec = Decoder::new(&bytes).expect("parse");
    assert!(!dec.is_seekable());
    assert_eq!(
        dec.decode_sample_range(5, 5).expect("empty range"),
        Vec::<i32>::new()
    );
    assert_eq!(
        dec.decode_sample_range(total as u64, total as u64)
            .expect("empty boundary range"),
        Vec::<i32>::new()
    );
}

#[test]
fn seekable_stream_still_seeks_normally() {
    // Regression guard: the gate must not break the happy path.
    let (pcm, bytes, regular, _fc, _total) = make_multi_frame_stream();
    let dec = Decoder::new(&bytes).expect("parse");
    assert!(dec.is_seekable());
    let sp = dec.seek_to_sample(regular as u64 + 3).expect("seek");
    assert_eq!(sp.frame_index, 1);
    assert_eq!(sp.sample_offset_in_frame, 3);
    // Tail decode from a seek point agrees with the eager suffix.
    let nch = 2usize;
    let from = dec.decode_from_sample(10).expect("from-sample");
    assert_eq!(from, pcm[10 * nch..]);
}
