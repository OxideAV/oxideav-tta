#![no_main]

//! Decode **hand-synthesised streams with a valid header + seek table
//! but attacker-chosen frame geometry**, asserting panic- and
//! runaway-allocation-freedom.
//!
//! ## Why a separate target
//!
//! The `decode` target feeds raw fuzz bytes into `oxideav_tta::decode`;
//! the 22-byte header ends in a CRC32 over its 18 leading bytes
//! (`spec/01` §3.5), so a byte-level mutator essentially never produces
//! a header that parses — it overwhelmingly exercises header rejection.
//! The `corrupt_decode` target closes part of that gap by running the
//! in-crate **encoder** and then mutating the region past the header,
//! but that pins the geometry fields (`channels`, `bits_per_sample`,
//! `sample_rate`, `total_samples`) to whatever the encoder wrote for a
//! small (`<= 2048`-sample) input, and bit-flips can't lift
//! `total_samples` to `u32::MAX` while keeping the header CRC valid. So
//! neither target explores the **geometry space itself**: a header whose
//! CRC is valid yet advertises an enormous `total_samples` (or a
//! ceiling-value `sample_rate` that drives the per-frame sample count
//! toward ~8.76 M) paired with almost no on-disk frame data.
//!
//! That space is exactly where an unbounded, header-derived
//! preallocation turns into a multi-gigabyte `Vec::with_capacity` abort
//! or a multi-hundred-megabyte zero-filled per-frame buffer. This target
//! hand-builds such streams — computing the header CRC and the
//! seek-table CRC itself so the decoder proceeds past both gates — from
//! arbitrary fuzz-chosen geometry, then drives every eager, streaming,
//! random-access, and range decode entry point.
//!
//! ## Contract under test
//!
//! Panic-freedom, no out-of-bounds indexing, no debug/ASAN integer
//! overflow, and — the point of this target — no attacker-controlled
//! allocation: a stream advertising billions of samples against a few
//! bytes of body must surface a typed [`oxideav_tta::Error`] (a
//! `Truncated` frame walk in practice), never an allocator abort or a
//! committed-page memory blow-up. No bit-exactness is asserted; the
//! bodies are arbitrary, so there is nothing to compare against.
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0     : format seed    → format = 1 (even) or 2 (odd)
//!   byte 1     : channels seed   → channels = (b1 % 6) + 1, in 1..=6
//!   byte 2     : bit-depth seed   → bps = 16 + (b2 % 9), in 16..=24
//!   bytes 3-6  : sample_rate seed  → LE u32 masked to 1..=0x007F_FFFF
//!   bytes 7-10 : total_samples     → LE u32, used verbatim (the whole
//!                u32 space, including u32::MAX)
//!   bytes 11.. : disk-size + body script — cycled to fill the seek
//!                table's per-frame `disk_size` entries and then appended
//!                as raw frame bytes.
//! ```
//!
//! An input shorter than 11 bytes returns immediately.

use libfuzzer_sys::fuzz_target;

use oxideav_tta::Decoder;

/// Cap the number of seek-table entries the fuzzer will itself
/// construct, so a small-`sample_rate` + huge-`total_samples` pairing
/// (which would imply hundreds of thousands of frames, hence a
/// multi-hundred-KiB seek table built *here*) is skipped rather than
/// bloating the fuzzer's own working set. The interesting decoder path —
/// a huge `total_samples` reachable with a *few* frames via a large
/// `sample_rate` — is still fully exercised (frame_count stays small).
const MAX_FRAME_COUNT: usize = 200_000;

/// IEEE-802.3 CRC32 (`spec/01` §6), inlined so this target does not
/// reach into the crate's private `crc32` module.
fn ieee_crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 11 {
        return;
    }

    // ── Geometry from the structured prefix ────────────────────────
    let format: u16 = if data[0] & 1 == 1 { 2 } else { 1 };
    let channels: u16 = ((data[1] as u16) % 6) + 1;
    let bits_per_sample: u16 = 16 + (data[2] % 9) as u16; // 16..=24
    let raw_rate = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
    let sample_rate = (raw_rate & 0x007F_FFFF).max(1); // 1..=0x7F_FFFF
    let total_samples = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);

    // Mirror StreamHeader::frame_geometry (spec/01 §4.1).
    let regular = ((sample_rate as u64) * 256 / 245) as u32;
    let frame_count: usize = if regular == 0 || total_samples == 0 {
        0
    } else if total_samples.is_multiple_of(regular) {
        (total_samples / regular) as usize
    } else {
        (total_samples / regular) as usize + 1
    };
    if frame_count > MAX_FRAME_COUNT {
        // Would force the fuzzer to build an oversized seek table; the
        // small-frame-count huge-total_samples case (the reachable
        // decoder risk) is covered by other inputs.
        return;
    }

    let script = &data[11..];

    // ── 22-byte stream header with a *valid* CRC ───────────────────
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"TTA1");
    bytes.extend_from_slice(&format.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&bits_per_sample.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&total_samples.to_le_bytes());
    let hdr_crc = ieee_crc32(&bytes[..18]);
    bytes.extend_from_slice(&hdr_crc.to_le_bytes());

    // ── Seek table: frame_count u32 disk sizes + a *valid* CRC ─────
    // Each disk_size is cycled from the script bytes (masked to a
    // modest ceiling so cumulative offsets stay in u64 range without
    // saturating); an empty script yields the 4-byte minimum entry.
    let entries_start = bytes.len();
    for i in 0..frame_count {
        let base = if script.is_empty() {
            4u32
        } else {
            let b0 = script[(i * 4) % script.len()] as u32;
            let b1 = script[(i * 4 + 1) % script.len()] as u32;
            // Keep sizes in [4, ~1 MiB]; the trailing 4 bytes are the
            // per-frame CRC, so 4 is the legal minimum (empty body).
            4 + (((b0 << 8) | b1) & 0x000F_FFFF)
        };
        bytes.extend_from_slice(&base.to_le_bytes());
    }
    let st_crc = ieee_crc32(&bytes[entries_start..]);
    bytes.extend_from_slice(&st_crc.to_le_bytes());

    // ── Arbitrary frame-region bytes (usually far short of what the
    //    seek table's disk sizes claim, so the frame walk runs off the
    //    end and must return Truncated, not over-allocate). ──────────
    bytes.extend_from_slice(script);

    // ── Drive every decode entry point. ────────────────────────────
    let _ = oxideav_tta::decode(&bytes);
    let _ = oxideav_tta::decode_with_password(&bytes, b"fuzz-pw");
    let _ = oxideav_tta::scan_trailers(&bytes);

    if let Ok(dec) = Decoder::new(&bytes) {
        let total = dec.total_samples();
        let fc = dec.frames.len();

        let _ = dec.decode_all();

        if total > 0 {
            // Suffix + range APIs share the header-derived preallocation
            // that this target is here to keep bounded.
            let mid = total as u64 / 2;
            let _ = dec.decode_from_sample(mid);
            let _ = dec.decode_sample_range(mid, total as u64);
            let _ = dec.seek_to_sample(mid.min(total as u64 - 1));
        }

        if fc > 0 && fc <= 4096 {
            for r in dec.frame_iter() {
                if r.is_err() {
                    break;
                }
            }
            let _ = dec.decode_frame_at((data[1] as usize) % fc);
        }
    }
});
