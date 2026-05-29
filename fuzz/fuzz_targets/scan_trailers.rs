#![no_main]

//! Drive arbitrary fuzz-supplied bytes through `oxideav_tta::scan_trailers`.
//!
//! The framing layer's trailer-detection path (`spec/01` §7) walks the
//! optional ID3v2 prefix, parses the 22-byte TTA1 stream header, then
//! the `4 * frame_count + 4`-byte seek table, sums each frame's
//! `disk_size` to compute the byte just past the last frame's trailing
//! CRC, and finally scans the bytes past that offset for an ID3v1
//! trailer (last 128 bytes start with `'TAG'`) and / or an APEv2
//! footer (32-byte `'APETAGEX'` block whose declared `tag_size` field
//! determines the tag region). Every step does attacker-controlled
//! arithmetic against header fields:
//!
//! - `frame_count = ceil(total_samples / regular_frame_samples)` where
//!   `regular_frame_samples = sample_rate * 256 / 245` — `sample_rate`
//!   and `total_samples` are u32 header fields the attacker chooses.
//! - `seek_table_len = frame_count * 4 + 4` — a `frame_count` of
//!   `u32::MAX / 4` overflows naive `usize` arithmetic.
//! - `eos = file_offset[N-1] + disk_size[N-1]` — both are u32 from the
//!   seek table; the sum can exceed `usize::MAX` on 32-bit hosts and
//!   the wrapped value would point into the in-stream bytes.
//! - The APEv2 footer's `tag_size` field (LE u32 at footer offset 12)
//!   and "has-header" flag bit (offset 20, bit 31) drive a subtraction
//!   that must clip against `eos` so the scanner never reads in-stream
//!   bytes.
//!
//! Contract under test: `scan_trailers(bytes)` ALWAYS returns a
//! `Result` — no panic, no abort, no integer overflow (debug build),
//! no out-of-bounds index, no allocation proportional to an
//! attacker-controlled header field. The return value is intentionally
//! discarded; the trailer offsets it reports are validated by the
//! in-crate `tests/malformed_props.rs` property suite, not by this
//! fuzz target (which is panic-free-only by design).
//!
//! This is decode-side and complementary to `decode.rs`: the existing
//! decode target stops at `Decoder::new` for malformed framing, so a
//! header that parses but produces a degenerate seek table never
//! reaches the trailer scanner via that path. `scan_trailers` exposes
//! the trailer-region arithmetic directly and is the surface this
//! target covers.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // `scan_trailers` accepts the full file buffer (ID3v2 prefix +
    // TTA1 header + seek table + frame blobs + optional trailers) and
    // returns `Ok(TrailerInfo)` for any well-formed framing, or
    // `Err(Error::…)` for malformed framing. Neither branch may panic.
    let _ = oxideav_tta::scan_trailers(data);
});
