#![no_main]

//! Differential fuzz target for the round-240..262 typed-accessor
//! surface on the TTA1 header / seek-table layer.
//!
//! Two independent code paths in the crate validate the same `spec/01`
//! §3 field invariants:
//!
//! 1. the **byte-level parser** behind `Decoder::new` (magic + CRC32 +
//!    inline range checks on `format` / `channels` / `bits_per_sample`
//!    / `sample_rate`, in on-wire field order), and
//! 2. the **typed-accessor lift** `StreamHeader::typed()` →
//!    `TypedStreamHeader::from_header` (the same five lifts, documented
//!    to surface the same first error the parser would for the same
//!    raw values).
//!
//! This target drives both paths with the same attacker-chosen raw
//! fields and asserts they agree:
//!
//! - `typed()` is `Ok` iff every per-field lift (`format_typed` /
//!   `channel_count_typed` / `bits_per_sample_typed` /
//!   `sample_rate_typed`) is `Ok`, and on `Err` it carries the FIRST
//!   per-field error in `spec/01` §3 table order.
//! - A 22-byte on-wire header synthesized from the same fields (valid
//!   magic + valid CRC32 so only field validation can reject) makes
//!   `Decoder::new` surface exactly the `typed()` error; when
//!   `typed()` is `Ok` the construction proceeds past field validation
//!   (format=2 → `PasswordRequired`; format=1 → `Truncated` at the
//!   absent seek table — never a field-validation variant).
//! - Every derived projection on the aggregate view agrees with its
//!   raw-header sibling: `to_header()` round-trip, `byte_depth`,
//!   `regular_frame_samples`, `total_duration` (3-way with
//!   `TotalSamples::duration_at`), `pcm_byte_len` product rule, and
//!   the full `FrameGeometry` invariant set of `spec/01` §4.1
//!   (`1 <= last <= regular`, closed-form
//!   `(fc - 1) * regular + last == total_samples`, `frame_samples_at`
//!   at first/last/past-end, `seek_table_size_bytes == 4 * fc + 4`,
//!   exact-multiple predicate, empty-stream degradation).
//! - The `FrameDescriptor` lifts (`disk_size_typed` /
//!   `sample_count_typed`, `spec/01` §4.2 / §5.1 / §5.5) and the
//!   `SeekPoint` lifts (`frame_index_typed` / `sample_offset_typed`,
//!   `spec/01` §4.1) accept exactly their documented windows against a
//!   geometry derived from in-range fields, with round-trip /
//!   `is_last` / `is_frame_boundary` / `interleaved_skip` agreement.
//!
//! Totality is also pinned on the raw (unfolded) header: the
//! infallible accessors (`bytes_per_sample`, `frame_geometry`,
//! `total_duration`, `total_samples_typed`) must not panic for ANY
//! `(u16, u16, u16, u32, u32)` field combination, including the
//! `sample_rate == 0` → `regular == 0` degenerate the parser would
//! have rejected.
//!
//! The harness re-implements nothing from the crate except the
//! IEEE-802.3 CRC32 of `spec/01` §6 (reflected polynomial
//! `0xEDB88320`, init/xorout `0xFFFFFFFF`), needed to synthesize a
//! header whose CRC check passes so field validation is the only
//! rejection path under test.

use libfuzzer_sys::fuzz_target;
use oxideav_tta::{Decoder, Error, FrameDescriptor, SeekPoint, StreamHeader};

/// IEEE-802.3 CRC32 per `spec/01` §6 (bitwise, LSB-first, reflected
/// polynomial `0xEDB88320`, initial register and output XOR both
/// `0xFFFFFFFF`). Used only to make the synthesized header's CRC
/// valid; correctness of the crate's own CRC is covered by the
/// existing `decode` target + unit tests.
fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let lsb = crc & 1;
            crc >>= 1;
            if lsb != 0 {
                crc ^= 0xEDB8_8320;
            }
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// Synthesize the 22-byte on-wire header (`spec/01` §3) for the raw
/// fields with valid magic + valid CRC so that `Decoder::new` can
/// only reject on field validation (or proceed to the seek table).
fn header_bytes(h: &StreamHeader) -> [u8; 22] {
    let mut buf = [0u8; 22];
    buf[0..4].copy_from_slice(b"TTA1");
    buf[4..6].copy_from_slice(&h.format.to_le_bytes());
    buf[6..8].copy_from_slice(&h.channels.to_le_bytes());
    buf[8..10].copy_from_slice(&h.bits_per_sample.to_le_bytes());
    buf[10..14].copy_from_slice(&h.sample_rate.to_le_bytes());
    buf[14..18].copy_from_slice(&h.total_samples.to_le_bytes());
    let crc = crc32_ieee(&buf[..18]);
    buf[18..22].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// First per-field lift error in `spec/01` §3 table order, via the
/// crate's own per-field accessors (NOT a re-statement of the range
/// checks — the ranges themselves live in one place in the crate).
fn first_per_field_error(h: &StreamHeader) -> Option<Error> {
    if let Err(e) = h.format_typed() {
        return Some(e);
    }
    if let Err(e) = h.channel_count_typed() {
        return Some(e);
    }
    if let Err(e) = h.bits_per_sample_typed() {
        return Some(e);
    }
    if let Err(e) = h.sample_rate_typed() {
        return Some(e);
    }
    None
}

/// Core agreement check between the typed-lift surface and the
/// byte-level parser, plus the derived-projection invariants.
fn check_header(h: &StreamHeader) {
    // Totality: the infallible accessors must not panic for ANY raw
    // field combination, in-range or not.
    let _ = h.bytes_per_sample();
    let _ = h.regular_frame_samples();
    let _ = h.frame_geometry();
    let _ = h.frame_geometry_typed();
    let _ = h.total_duration();
    let _ = h.total_samples_typed();

    let typed = h.typed();
    let expected_err = first_per_field_error(h);

    // Byte-level differential: valid magic + valid CRC, so the parser
    // reaches field validation with the exact same raw values.
    let on_wire = header_bytes(h);
    let parsed = Decoder::new(&on_wire);

    match (&typed, expected_err) {
        (Err(e), Some(first)) => {
            // Aggregate lift error == first per-field error in §3 order.
            assert_eq!(*e, first, "typed() error is not the first per-field error");
            // Parser surfaces the same variant for the same raw bytes.
            // Exception: format == 2 with otherwise-arbitrary fields is
            // a VALID format per Format::from_raw, so it cannot appear
            // here; PasswordRequired never collides with a field error.
            assert_eq!(
                parsed.as_ref().err(),
                Some(&first),
                "byte-level parser disagrees with typed() first error"
            );
        }
        (Ok(t), None) => {
            // Field validation passed in both worlds; the parser must
            // proceed PAST field validation: format=2 gates on the
            // password before the seek table, format=1 hits the absent
            // seek table (a 22-byte buffer can never satisfy
            // `4 * frame_count + 4` because frame_count >= 0 needs
            // at least 4 more bytes).
            match parsed {
                Err(Error::PasswordRequired) => assert!(t.requires_password()),
                Err(Error::Truncated) => {}
                Err(other) => {
                    panic!("parser took an unexpected path on a field-valid header: {other:?}")
                }
                Ok(_) => panic!("parser succeeded on a 22-byte header-only buffer"),
            }
            // format=2 with a password supplied must also clear field
            // validation and reach the seek table.
            if t.requires_password() {
                assert_eq!(
                    Decoder::new_with_password(&on_wire, b"fuzz").err(),
                    Some(Error::Truncated),
                    "password-lifted construction must reach the seek table"
                );
            }
            check_projections(h, t);
        }
        (Ok(_), Some(_)) | (Err(_), None) => {
            panic!("typed() Ok/Err disagrees with the per-field lifts")
        }
    }
}

/// Derived-projection agreement + `spec/01` §4.1 geometry invariants,
/// only reachable when every field is in range.
fn check_projections(h: &StreamHeader, t: &oxideav_tta::TypedStreamHeader) {
    // Lossless round-trip back to the on-wire data model.
    assert_eq!(t.to_header(), *h, "to_header() round-trip mismatch");

    // Per-field projections agree with the raw-header siblings.
    assert_eq!(t.byte_depth(), h.bytes_per_sample());
    assert_eq!(t.regular_frame_samples(), h.regular_frame_samples());
    assert_eq!(t.requires_password(), h.format == 2);
    assert_eq!(t.total_samples().count(), h.total_samples);

    // Duration: 3-way agreement (aggregate / header convenience /
    // TotalSamples primitive).
    let d = t.total_duration();
    assert_eq!(d, h.total_duration());
    assert_eq!(d, h.total_samples_typed().duration_at(h.sample_rate));

    // §3.4 product rule, computed independently in u64.
    let expected_pcm =
        (h.total_samples as u64) * (h.channels as u64) * (h.bytes_per_sample() as u64);
    assert_eq!(t.pcm_byte_len(), expected_pcm, "pcm_byte_len product rule");

    // Geometry: aggregate view == typed projection == bare tuple.
    let g = t.frame_geometry();
    assert_eq!(g, h.frame_geometry_typed());
    let (fc, last) = h.frame_geometry();
    assert_eq!(g.frame_count(), fc);
    assert_eq!(g.last_frame_samples(), last);
    let regular = h.regular_frame_samples();
    assert_eq!(g.regular_frame_samples(), regular);
    // sample_rate >= 1 by construction here, so regular >= 1.
    assert!(
        regular >= 1,
        "regular_frame_samples must be >= 1 for a valid rate"
    );

    // §4.1 invariant set.
    assert_eq!(g.seek_table_size_bytes() as u64, 4 * (fc as u64) + 4);
    assert_eq!(
        g.frame_samples_at(fc),
        None,
        "past-end frame index must be None"
    );
    if h.total_samples == 0 {
        assert!(g.is_empty());
        assert_eq!(fc, 0);
        assert_eq!(last, 0);
        assert!(!g.is_exact_multiple());
        assert_eq!(g.total_samples(), 0);
        assert_eq!(g.frame_samples_at(0), None);
    } else {
        assert!(!g.is_empty());
        assert!(fc >= 1);
        assert!(
            (1..=regular).contains(&last),
            "1 <= last <= regular per spec/01 §4.1"
        );
        assert_eq!(g.is_exact_multiple(), last == regular);
        // Closed form in u64: (fc - 1) * regular + last == total.
        assert_eq!(
            (fc as u64 - 1) * (regular as u64) + last as u64,
            h.total_samples as u64,
            "frame geometry closed form"
        );
        assert_eq!(g.total_samples(), h.total_samples);
        let first_expected = if fc == 1 { last } else { regular };
        assert_eq!(g.frame_samples_at(0), Some(first_expected));
        assert_eq!(g.frame_samples_at(fc - 1), Some(last));
        if fc >= 3 {
            // Any strictly-interior frame carries the regular count.
            assert_eq!(g.frame_samples_at(fc / 2), Some(regular));
        }
    }
}

/// `FrameDescriptor` lift agreement per `spec/01` §4.2 / §5.1 / §5.5.
fn check_frame_descriptor(disk_size: u32, sample_count: u32, regular: u32) {
    let fd = FrameDescriptor {
        file_offset: 0,
        disk_size,
        sample_count,
        is_last: false,
    };
    match fd.disk_size_typed() {
        Ok(len) => {
            assert!(disk_size >= 4, "disk_size_typed accepted < 4");
            assert_eq!(len.total_size(), disk_size);
            assert_eq!(len.body_size(), disk_size - 4);
            assert_eq!(len.body_size(), fd.body_size());
        }
        Err(e) => {
            assert!(disk_size < 4, "disk_size_typed rejected >= 4");
            assert_eq!(e, Error::InvalidFrameByteLength(disk_size));
            assert_eq!(fd.body_size(), 0, "raw body_size must saturate below 4");
        }
    }
    match fd.sample_count_typed() {
        Ok(sc) => {
            assert!(sample_count >= 1, "sample_count_typed accepted 0");
            assert_eq!(sc.count(), sample_count);
            assert_eq!(
                sc.is_within_regular_bound(regular),
                sample_count <= regular,
                "regular-bound gate"
            );
        }
        Err(e) => {
            assert_eq!(sample_count, 0, "sample_count_typed rejected >= 1");
            assert_eq!(e, Error::InvalidFrameSampleCount(0));
        }
    }
}

/// `SeekPoint` lift agreement per `spec/01` §4.1 against a geometry
/// derived from in-range fields.
fn check_seek_point(frame_index: usize, offset: u32, fc: u32, regular: u32, channels: u16) {
    let sp = SeekPoint {
        frame_index,
        sample_offset_in_frame: offset,
    };
    match sp.frame_index_typed(fc as usize) {
        Ok(fi) => {
            assert!(
                frame_index < fc as usize,
                "frame_index_typed accepted past-end"
            );
            assert_eq!(fi.index(), frame_index);
            assert_eq!(fi.is_last(fc as usize), frame_index + 1 == fc as usize);
        }
        Err(e) => {
            assert!(
                frame_index >= fc as usize,
                "frame_index_typed rejected in-window"
            );
            assert_eq!(e, Error::InvalidFrameIndex(frame_index));
        }
    }
    // usize::MAX is never a legal index for any u32-sized table.
    assert_eq!(
        SeekPoint {
            frame_index: usize::MAX,
            sample_offset_in_frame: 0,
        }
        .frame_index_typed(fc as usize)
        .err(),
        Some(Error::InvalidFrameIndex(usize::MAX))
    );
    match sp.sample_offset_typed(regular) {
        Ok(so) => {
            assert!(offset < regular, "sample_offset_typed accepted >= regular");
            assert_eq!(so.offset(), offset);
            assert_eq!(so.is_frame_boundary(), offset == 0);
            assert_eq!(
                so.interleaved_skip(channels),
                (offset as usize).saturating_mul(channels as usize)
            );
        }
        Err(e) => {
            assert!(offset >= regular, "sample_offset_typed rejected in-window");
            assert_eq!(e, Error::InvalidInFrameSampleOffset(offset));
        }
    }
}

fn le_u16(data: &[u8], at: usize) -> u16 {
    let mut b = [0u8; 2];
    for (i, slot) in b.iter_mut().enumerate() {
        *slot = data.get(at + i).copied().unwrap_or(0);
    }
    u16::from_le_bytes(b)
}

fn le_u32(data: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    for (i, slot) in b.iter_mut().enumerate() {
        *slot = data.get(at + i).copied().unwrap_or(0);
    }
    u32::from_le_bytes(b)
}

fuzz_target!(|data: &[u8]| {
    // Raw attacker-chosen fields (zero-padded past the input end).
    let format = le_u16(data, 0);
    let channels = le_u16(data, 2);
    let bits_per_sample = le_u16(data, 4);
    let sample_rate = le_u32(data, 6);
    let total_samples = le_u32(data, 10);
    let disk_size = le_u32(data, 14);
    let sample_count = le_u32(data, 18);
    let frame_index = le_u32(data, 22) as usize;
    let offset = le_u32(data, 26);

    // Pass 1: the raw header as-is (mostly out-of-range — pins the
    // rejection agreement and the totality of the infallible
    // accessors).
    let raw = StreamHeader {
        format,
        channels,
        bits_per_sample,
        sample_rate,
        total_samples,
    };
    check_header(&raw);

    // Pass 2: fields folded into their documented windows so every
    // iteration also exercises the typed-Ok projection set. The folds
    // are surjective onto the full valid windows of spec/01 §3.1-§3.4.
    let folded = StreamHeader {
        format: 1 + (format & 1),
        channels: 1 + channels % 6,
        bits_per_sample: 16 + bits_per_sample % 9,
        sample_rate: 1 + sample_rate % 0x007F_FFFF,
        total_samples,
    };
    check_header(&folded);

    // FrameDescriptor + SeekPoint lifts against the folded (valid)
    // stream's geometry.
    let regular = folded.regular_frame_samples();
    let (fc, _) = folded.frame_geometry();
    check_frame_descriptor(disk_size, sample_count, regular);
    check_seek_point(frame_index, offset, fc, regular, folded.channels);
});
