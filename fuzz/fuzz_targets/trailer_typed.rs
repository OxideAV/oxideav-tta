#![no_main]

//! Differential fuzz target for the typed-accessor surface on the
//! trailer-detection layer (`spec/01` §7).
//!
//! The round-188-era `scan_trailers` target drives the byte-level
//! scanner [`oxideav_tta::scan_trailers`] for panic-freedom but then
//! discards the [`oxideav_tta::TrailerInfo`] it returns. The typed
//! lift on that result — `TrailerInfo::id3v1_typed` /
//! `TrailerInfo::apev2_typed` / `TrailerInfo::combined_byte_range`, and
//! the [`oxideav_tta::Id3v1Range`] / [`oxideav_tta::ApeV2Range`]
//! projection accessors — is never exercised by a fuzzer. This target
//! covers that gap with the same differential discipline the
//! `typed_header` target applies to the §3 header layer: the raw
//! `(start, len)` tuples on the parser-produced `TrailerInfo` and their
//! typed lifts must agree on every documented `spec/01` §7 invariant.
//!
//! Two complementary entry points feed `TrailerInfo` from the same
//! fuzz bytes:
//!
//! 1. **`scan_trailers(data)`** — the full framing path. When the
//!    fuzz bytes happen to form a parseable TTA1 header + seek table,
//!    the scanner computes the end-of-stream offset and walks the
//!    §7 trailer region. The resulting `TrailerInfo` is, by
//!    construction, valid at `file_len = data.len()`.
//! 2. **`detect_trailers(data, eos)`** — the scanner driven directly,
//!    with a fuzz-derived `eos` folded across `data.len()` so the
//!    trailer-region arithmetic is reached without needing a parseable
//!    in-stream prefix. Its `TrailerInfo` is valid at
//!    `file_len = data.len()` too.
//!
//! For each parser-produced `TrailerInfo` the target asserts the
//! **lift-totality** contract documented on `id3v1_typed` /
//! `apev2_typed`: a `TrailerInfo` returned by the scanner ALWAYS lifts
//! cleanly at the `file_len` it was scanned against. Concretely, for
//! `file_len = data.len()`:
//!
//! - `id3v1_typed(file_len)` is `Ok` and, when `Some`, carries
//!   `len() == 128`, `end() == file_len` (anchored at file end per §7),
//!   `is_at_file_end(file_len) == true`, a non-empty `byte_range()` of
//!   exactly 128 bytes, and `start()/len()` equal to the raw tuple.
//! - `apev2_typed(file_len)` is `Ok` and, when `Some`, carries
//!   `len() >= ApeV2Range::FOOTER_SIZE` (32), `end() <= file_len`
//!   (bounded by the file per §7), `header_and_body_size() ==
//!   len() - 32`, a non-empty `byte_range()`, and `start()/len()`
//!   equal to the raw tuple.
//! - When BOTH trailers are present, the §7 "APE immediately before
//!   ID3v1" adjacency holds on the parser output: the APE range ends
//!   exactly where the ID3v1 range starts
//!   (`apev2.end() == id3v1.start()`), the ID3v1 range is NOT at file
//!   end-relative for the APE (`apev2.is_at_file_end(file_len) ==
//!   false`), and `combined_byte_range()` spans both
//!   (`start == apev2.start()`, `start + len == id3v1.end() ==
//!   file_len`, `len == id3v1.len() + apev2.len()`).
//! - `combined_byte_range()` agrees with the raw fields in every
//!   presence combination: `None` iff `is_empty()`; equal to the lone
//!   trailer's `(start, len)` when exactly one is present; the
//!   min-start / max-end hull when both are present.
//!
//! The typed accessors are ALSO exercised on hand-constructed
//! `TrailerInfo` literals built from fuzz-chosen raw tuples (NOT
//! parser-produced) to drive the rejection side of `from_raw`: this is
//! the "ad-hoc fixture" path the doc comments call out as the only way
//! to reach `Error::InvalidId3v1Range` / `Error::InvalidApeV2Range`.
//! For a literal `TrailerInfo { id3v1: Some((s, l)), .. }` the target
//! asserts `id3v1_typed(file_len)` is `Ok(Some(_))` iff
//! `l == 128 && s.checked_add(l) == Some(file_len)`, and otherwise
//! `Err(InvalidId3v1Range(s, l))`; symmetrically `apev2_typed` is
//! `Ok(Some(_))` iff `l >= 32 && s + l <= file_len` (no overflow),
//! else `Err(InvalidApeV2Range(s, l))`. This pins the typed accessors
//! to the exact §7 predicate windows their `from_raw` lifts document.
//!
//! Contract: no path panics, integer-overflows (debug build), or
//! indexes out of bounds for ANY input. The crate's own
//! `tests/malformed_props.rs` validates the scanner's *offsets*; this
//! target validates the *typed lift* layered on top of them.

use libfuzzer_sys::fuzz_target;
use oxideav_tta::{detect_trailers, scan_trailers, ApeV2Range, Error, Id3v1Range, TrailerInfo};

/// Assert the documented `spec/01` §7 invariants on a `TrailerInfo`
/// that was produced by the scanner (`scan_trailers` / `detect_trailers`)
/// at `file_len`. Such a value must always lift cleanly.
fn check_parser_produced(info: TrailerInfo, file_len: usize) {
    // ── ID3v1 lift: always Ok; Some ⇒ §7 anchored-128 invariants. ──
    let id3 = info
        .id3v1_typed(file_len)
        .expect("parser-produced ID3v1 range must lift cleanly at its own file_len");
    assert_eq!(id3.is_some(), info.id3v1.is_some());
    if let Some(r) = id3 {
        let (raw_s, raw_l) = info.id3v1.unwrap();
        assert_eq!(r.start(), raw_s);
        assert_eq!(r.len(), raw_l);
        assert_eq!(r.len(), 128, "ID3v1 is a fixed 128-byte trailer (§7)");
        assert!(!r.is_empty());
        assert_eq!(r.end(), file_len, "ID3v1 anchored at file end (§7)");
        assert!(r.is_at_file_end(file_len));
        let range = r.byte_range();
        assert_eq!(range.start, r.start());
        assert_eq!(range.end, r.end());
        assert_eq!(range.end - range.start, 128);
    }

    // ── APEv2 lift: always Ok; Some ⇒ §7 footer-min / in-file. ──
    let ape = info
        .apev2_typed(file_len)
        .expect("parser-produced APEv2 range must lift cleanly at its own file_len");
    assert_eq!(ape.is_some(), info.apev2.is_some());
    if let Some(r) = ape {
        let (raw_s, raw_l) = info.apev2.unwrap();
        assert_eq!(r.start(), raw_s);
        assert_eq!(r.len(), raw_l);
        assert!(
            r.len() >= ApeV2Range::FOOTER_SIZE,
            "APEv2 region is at least the 32-byte footer (§7)"
        );
        assert!(!r.is_empty());
        assert!(
            r.end() <= file_len,
            "APEv2 region lies within the file (§7)"
        );
        assert_eq!(r.header_and_body_size(), r.len() - ApeV2Range::FOOTER_SIZE);
        let range = r.byte_range();
        assert_eq!(range.start, r.start());
        assert_eq!(range.end, r.end());
    }

    // ── §7 "APE immediately before ID3v1" adjacency when both present. ──
    if let (Some(i), Some(a)) = (id3, ape) {
        assert_eq!(
            a.end(),
            i.start(),
            "APE footer sits immediately before the ID3v1 trailer (§7)"
        );
        assert!(
            !a.is_at_file_end(file_len),
            "with ID3v1 present, APE is not at file end (§7)"
        );
    }

    // ── combined_byte_range agrees with the raw presence combination. ──
    let combined = info.combined_byte_range();
    assert_eq!(combined.is_none(), info.is_empty());
    match (info.id3v1, info.apev2) {
        (None, None) => assert!(combined.is_none()),
        (Some(t), None) | (None, Some(t)) => assert_eq!(combined, Some(t)),
        (Some((s_i, l_i)), Some((s_a, l_a))) => {
            let (cs, cl) = combined.expect("both present ⇒ combined hull is Some");
            assert_eq!(cs, s_i.min(s_a));
            let hull_end = (s_i + l_i).max(s_a + l_a);
            assert_eq!(cs + cl, hull_end);
            // Parser output is adjacent (APE end == ID3 start), so the
            // hull is exactly the two lengths summed and reaches file_len.
            assert_eq!(cs, s_a);
            assert_eq!(cs + cl, file_len);
            assert_eq!(cl, l_i + l_a);
        }
    }
}

/// Drive the rejection side of `from_raw` through a hand-constructed
/// `TrailerInfo` literal (NOT parser-produced) with fuzz-chosen raw
/// tuples, pinning the typed accessors to their documented §7 windows.
fn check_literal_lift(
    id3v1: Option<(usize, usize)>,
    apev2: Option<(usize, usize)>,
    file_len: usize,
) {
    let info = TrailerInfo { id3v1, apev2 };

    // ID3v1: Ok(Some) iff len==128 and start+len==file_len (no overflow).
    match info.id3v1_typed(file_len) {
        Ok(None) => assert!(id3v1.is_none()),
        Ok(Some(r)) => {
            let (s, l) = id3v1.expect("Ok(Some) ⇒ a tuple was present");
            assert_eq!(l, 128);
            assert_eq!(s.checked_add(l), Some(file_len));
            assert_eq!((r.start(), r.len()), (s, l));
            // Cross-check the standalone constructor agrees with the lift.
            assert_eq!(Id3v1Range::from_raw(s, l, file_len), Ok(r));
        }
        Err(Error::InvalidId3v1Range(es, el)) => {
            let (s, l) = id3v1.expect("Err lift ⇒ a tuple was present");
            assert_eq!((es, el), (s, l));
            // The rejection predicate: NOT (len==128 && start+len==file_len).
            let ok = l == 128 && s.checked_add(l) == Some(file_len);
            assert!(!ok, "rejected a tuple that satisfies the §7 ID3v1 window");
        }
        Err(other) => panic!("unexpected ID3v1 lift error: {other:?}"),
    }

    // APEv2: Ok(Some) iff len>=32 and start+len<=file_len (no overflow).
    match info.apev2_typed(file_len) {
        Ok(None) => assert!(apev2.is_none()),
        Ok(Some(r)) => {
            let (s, l) = apev2.expect("Ok(Some) ⇒ a tuple was present");
            assert!(l >= 32);
            match s.checked_add(l) {
                Some(e) => assert!(e <= file_len),
                None => panic!("Ok lift on an overflowing APE tuple"),
            }
            assert_eq!((r.start(), r.len()), (s, l));
            assert_eq!(ApeV2Range::from_raw(s, l, file_len), Ok(r));
        }
        Err(Error::InvalidApeV2Range(es, el)) => {
            let (s, l) = apev2.expect("Err lift ⇒ a tuple was present");
            assert_eq!((es, el), (s, l));
            let ok = l >= 32 && matches!(s.checked_add(l), Some(e) if e <= file_len);
            assert!(!ok, "rejected a tuple that satisfies the §7 APEv2 window");
        }
        Err(other) => panic!("unexpected APEv2 lift error: {other:?}"),
    }

    // combined_byte_range never panics and matches the raw presence set.
    let combined = info.combined_byte_range();
    assert_eq!(combined.is_none(), info.is_empty());
}

/// Read a little-endian `usize` from up to 8 fuzz bytes, advancing the
/// cursor. Missing bytes read as zero.
fn take_usize(data: &[u8], cur: &mut usize) -> usize {
    let mut v: u64 = 0;
    for i in 0..8 {
        let b = data.get(*cur + i).copied().unwrap_or(0);
        v |= (b as u64) << (8 * i);
    }
    *cur += 8;
    v as usize
}

fuzz_target!(|data: &[u8]| {
    let file_len = data.len();

    // (1) Full framing path: scan_trailers derives eos from a parseable
    //     header, then walks §7. Its TrailerInfo lifts cleanly.
    if let Ok(info) = scan_trailers(data) {
        check_parser_produced(info, file_len);
    }

    // (2) Direct scanner: fold a fuzz-derived eos across data.len() so
    //     the §7 trailer-region arithmetic is reached without needing a
    //     parseable in-stream prefix. detect_trailers never reads bytes
    //     before eos and its TrailerInfo also lifts cleanly at file_len.
    if !data.is_empty() {
        let eos_seed = data[0] as usize;
        // Spread the seed across the buffer length so small inputs still
        // exercise both "eos inside the buffer" and "eos == len" cases.
        let eos = if file_len == 0 {
            0
        } else {
            (eos_seed.wrapping_mul(7)) % (file_len + 1)
        };
        let info = detect_trailers(data, eos);
        check_parser_produced(info, file_len);
    }

    // (3) Hand-constructed literals: drive the from_raw rejection side
    //     with fuzz-chosen raw tuples against a fuzz-chosen file_len.
    let mut cur = 0usize;
    let present_bits = data.first().copied().unwrap_or(0);
    let lit_file_len = take_usize(data, &mut cur).wrapping_add(1).min(1 << 40);
    let id3 = if present_bits & 1 != 0 {
        let s = take_usize(data, &mut cur) % (lit_file_len + 1);
        // Bias len toward 128 (the only accepted ID3v1 length) half the
        // time so the Ok-side gets exercised, raw len otherwise.
        let l = if present_bits & 2 != 0 {
            128
        } else {
            take_usize(data, &mut cur) % 512
        };
        Some((s, l))
    } else {
        None
    };
    let ape = if present_bits & 4 != 0 {
        let s = take_usize(data, &mut cur) % (lit_file_len + 1);
        // Bias len toward >=32 (the accepted window) so the Ok-side is hit.
        let l = if present_bits & 8 != 0 {
            32 + (take_usize(data, &mut cur) % 256)
        } else {
            take_usize(data, &mut cur) % 64
        };
        Some((s, l))
    } else {
        None
    };
    check_literal_lift(id3, ape, lit_file_len);
});
