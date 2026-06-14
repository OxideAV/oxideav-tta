#![no_main]

//! Drive the registered raw-`.tta` **demuxer** with arbitrary
//! fuzz-supplied bytes (round 299).
//!
//! The existing `decode` / `streaming_decode` targets cover the
//! single-shot decode entry points behind [`oxideav_tta::decode`], but
//! the framework demuxer reached through the `registry` feature is a
//! distinct code path: it parses the TTA1 header + seek table at open
//! time, then emits one self-contained mini-TTA1 file per audio frame
//! and answers O(1) `seek_to` requests off the cumulative seek-table
//! byte offsets (`spec/01` §4). None of that machinery — the per-frame
//! `file_offset + disk_size` slice arithmetic in particular — runs
//! through the `decode` target's surface, because the seek table the
//! decoder builds is internal while the demuxer's seek table is
//! consumed verbatim from attacker-controlled bytes.
//!
//! The contract under test is purely that the calls *return*: a
//! malformed stream yields `Err(..)` (an `oxideav-core` error), a
//! well-formed one yields a `Box<dyn Demuxer>` whose `next_packet` /
//! `seek_to` / `streams` / `duration_micros` methods never panic,
//! integer-overflow (in a debug build), index out of bounds, or OOM.
//! The return values are intentionally discarded.
//!
//! Surface exercised on every input:
//!
//! 1. [`ContainerRegistry::open_demuxer`] via a freshly-registered
//!    [`RuntimeContext`] — the same path the host app takes.
//! 2. `next_packet` drained to EOF (bounded by a frame-count cap so a
//!    pathological seek table cannot make the loop unbounded), which
//!    forces the per-frame `build_single_frame_file` slice arithmetic.
//! 3. `seek_to` with fuzz-chosen `(stream_index, pts)` pairs derived
//!    from the input's own prefix — including out-of-range stream
//!    indices, negative pts, and past-end pts — followed by another
//!    bounded `next_packet` drain from the post-seek frame cursor.
//!
//! The harness body is clean-room: there is no reference-implementation
//! oracle, only the panic-free / typed-error contract.

use libfuzzer_sys::fuzz_target;

use std::io::Cursor;

use oxideav_core::{CodecId, CodecResolver, ProbeContext, ReadSeek, RuntimeContext};
use oxideav_tta::register;

/// A resolver that never maps a tag — the TTA demuxer self-describes
/// its single stream and ignores the resolver, so this is sufficient.
struct NoopResolver;
impl CodecResolver for NoopResolver {
    fn resolve_tag(&self, _ctx: &ProbeContext) -> Option<CodecId> {
        None
    }
}

/// Cap the number of packets we pull so a pathological seek table that
/// somehow advances the cursor by zero cannot turn the drain into an
/// unbounded loop. A real `.tta` seek table is `4 * frame_count + 4`
/// bytes, so the frame count is bounded by the input length anyway, but
/// the cap keeps the per-iteration cost predictable for the fuzzer.
const MAX_PACKETS: usize = 4096;

fuzz_target!(|data: &[u8]| {
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);

    let resolver = NoopResolver;

    // Open through the public registry path. Most malformed inputs are
    // rejected here (bad magic / CRC / out-of-range header fields /
    // truncated seek table) with a typed error — that is fine.
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    let mut demuxer = match ctx.containers.open_demuxer("tta", input, &resolver) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Cheap accessors must not panic.
    let _ = demuxer.format_name();
    let _ = demuxer.streams();
    let _ = demuxer.duration_micros();

    // Drain packets to EOF (bounded). This forces the per-frame
    // mini-file assembly (`build_single_frame_file`), whose
    // `file_offset + disk_size` slice arithmetic reads attacker-chosen
    // seek-table byte sizes against the actual file length.
    for _ in 0..MAX_PACKETS {
        match demuxer.next_packet() {
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // Derive a small battery of seek probes from the input's own prefix
    // so attacker-chosen `(stream_index, pts)` pairs are driven against
    // attacker-chosen byte streams. We include the obvious edge shapes
    // (stream 0 vs out-of-range, pts = 0, negative pts, a large pts that
    // clamps past the last frame) plus a fuzz-derived pts.
    let fuzz_pts = {
        let mut bytes = [0u8; 8];
        for (i, b) in data.iter().take(8).enumerate() {
            bytes[i] = *b;
        }
        i64::from_le_bytes(bytes)
    };
    let probes: [(u32, i64); 6] = [
        (0, 0),
        (0, -1),
        (0, i64::MAX),
        (0, fuzz_pts),
        (1, fuzz_pts),
        (u32::MAX, 0),
    ];
    for (stream_index, pts) in probes {
        let _ = demuxer.seek_to(stream_index, pts);
        // After a seek, drain again from the post-seek frame cursor so
        // the seek-then-read path is exercised, not just the seek
        // arithmetic in isolation.
        for _ in 0..MAX_PACKETS {
            match demuxer.next_packet() {
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
});
