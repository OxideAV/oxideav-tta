#![no_main]

//! Drive the **seek-table-addressed surface** — `Decoder::seek_to_sample`
//! / `decode_frame_at` / `frame_iter_from` / `decode_from_sample` /
//! `decode_sample_range` and the registry demuxer's `seek_to` — over a
//! valid encoder-produced stream whose **seek-table entries have been
//! rewritten to attacker-chosen values**, both behind a *re-computed,
//! valid* seek-table CRC (so the decoder trusts them and every
//! random-access path runs off forged frame sizes) and behind the
//! original, now-*stale* CRC (so the `spec/01` §4.3 unseekable-mode
//! discipline must engage).
//!
//! ## Why a separate target
//!
//! `corrupt_decode` XOR-mutates bytes anywhere past the header — a
//! flipped seek-table byte there almost always breaks the table CRC and
//! lands the stream in unseekable mode, where the random-access API
//! refuses before touching the entries. `geometry` forges header
//! geometry and cycles script bytes into the entries but pairs them
//! with arbitrary (non-frame) body bytes, so nothing decodes past the
//! per-frame CRC. Neither reaches the state this target is about: a
//! CRC-**valid** table whose per-frame `disk_size` entries disagree with
//! the real frame boundaries of an otherwise intact stream — entries of
//! `0..=3` (below the 4-byte trailer minimum), entries that shift every
//! later frame's `file_offset` onto mid-frame bytes, an entry that
//! runs past end-of-file, an entry that is off by exactly one byte so
//! the body decodes but the trailer CRC is read from the wrong place.
//! Every random-access path then does its slice arithmetic and its
//! `sample_count`-derived preallocation from those forged values.
//!
//! ## Contract under test
//!
//! Panic-freedom and typed errors on every path, plus the structural
//! invariants the API documents:
//!
//! * `seek_to_sample(i)` for `i < total_samples` on a seekable stream
//!   returns `Ok(p)` with `p.frame_index < frames.len()` and
//!   `p.sample_offset_in_frame < regular_frame_samples`, and
//!   `frame_index * regular + offset == i`.
//! * `decode_frame_at(fi)` on `Ok` yields exactly
//!   `frames[fi].sample_count * channels` entries.
//! * With the stale CRC, `is_seekable()` is `false` and every
//!   random-access call returns `Err(SeekTableUnreliable)`, while the
//!   linear `decode_all` / `frame_iter` still *return* (an `Err` is
//!   fine — the entries are forged).
//! * With an **empty** mutation script the entries are untouched, so
//!   the re-CRC'd stream is byte-identical to the encoder's and every
//!   path must reproduce the encoder's input PCM bit-exactly (eager,
//!   `frame_iter` concatenation, `decode_from_sample(mid)` suffix).
//! * The registry demuxer over the forged stream opens (or refuses
//!   with a typed error), drains bounded, and answers `seek_to` without
//!   panicking.
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0      : channels seed       → channels = (b0 % 6) + 1
//!   byte 1      : bit-depth selector  → bps = 16 + (b1 % 9), in 16..=24
//!   bytes 2-3   : sample_rate seed    → LE u16, masked to 64..=0x7FF so
//!                 the regular frame length is small (66..=2139 samples)
//!                 and a few thousand samples span several frames
//!   bytes 4-5   : sample-count seed   → LE u16 per-channel samples,
//!                 capped at MAX_SAMPLES_PER_CHANNEL
//!   byte 6      : seek probe seed
//!   bytes 7..   : split in half — PCM seed (first half), then the
//!                 entry script (second half): records of
//!                 (u8 entry-index, LE u32 new disk_size); an entry
//!                 index is folded into the frame count. An empty
//!                 script leaves the table intact (the differential
//!                 baseline).
//! ```

use libfuzzer_sys::fuzz_target;

use std::io::Cursor;

use oxideav_core::{CodecId, CodecResolver, ProbeContext, ReadSeek, RuntimeContext};
use oxideav_tta::{register, Decoder, Error};

const MAX_SAMPLES_PER_CHANNEL: usize = 6000;
const HEADER_LEN: usize = 22;
const MAX_PACKETS: usize = 256;

struct NoopResolver;
impl CodecResolver for NoopResolver {
    fn resolve_tag(&self, _ctx: &ProbeContext) -> Option<CodecId> {
        None
    }
}

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

/// Build the encoder input PCM from seed bytes (sign-extended to the
/// declared depth so the encoder accepts every value).
fn build_pcm(seed: &[u8], slot_count: usize, byte_depth: usize) -> Vec<i32> {
    let mut samples = Vec::with_capacity(slot_count);
    for i in 0..slot_count {
        let s = if seed.is_empty() {
            ((i as i32) % 23) - 11
        } else {
            let base = (i * byte_depth) % seed.len();
            let b0 = seed[base] as u32;
            let b1 = seed[(base + 1) % seed.len()] as u32;
            if byte_depth == 2 {
                i16::from_le_bytes([b0 as u8, b1 as u8]) as i32
            } else {
                let b2 = seed[(base + 2) % seed.len()] as u32;
                let raw = b0 | (b1 << 8) | (b2 << 16);
                if raw & 0x0080_0000 != 0 {
                    (raw | 0xFF00_0000) as i32
                } else {
                    raw as i32
                }
            }
        };
        samples.push(s);
    }
    samples
}

/// Random-access + linear battery on a seekable decoder over forged
/// entries: every call must return; `Ok` results must carry the
/// documented shape.
fn drive_seekable(dec: &Decoder<'_>, probe_seed: u8) {
    let nch = dec.header.channels as usize;
    let total = dec.total_samples() as u64;
    let regular = dec.header.regular_frame_samples() as u64;
    let fc = dec.frames.len();

    let _ = dec.decode_all();
    for r in dec.frame_iter() {
        if r.is_err() {
            break;
        }
    }
    if fc == 0 {
        return;
    }

    // Frame-indexed random access over every frame (fc is small: the
    // sample cap and rate mask bound it to a few dozen), then suffix
    // walks from four representative start frames (a walk from every
    // frame would make the battery quadratic in `fc`).
    for (fi, fd) in dec.frames.iter().enumerate() {
        if let Ok(pcm) = dec.decode_frame_at(fi) {
            assert_eq!(
                pcm.len(),
                fd.sample_count as usize * nch,
                "decode_frame_at({fi}) length must follow the descriptor"
            );
        }
    }
    for fi in [0, fc / 2, fc - 1, (probe_seed as usize) % fc] {
        for r in dec.frame_iter_from(fi) {
            if r.is_err() {
                break;
            }
        }
    }
    assert!(matches!(
        dec.decode_frame_at(fc),
        Err(Error::FrameIndexOutOfRange)
    ));

    // Sample-keyed random access at a fuzz-chosen index plus the edges.
    if total > 0 {
        let probes = [
            0u64,
            total - 1,
            (probe_seed as u64 * 977) % total,
            total / 2,
        ];
        for &si in &probes {
            match dec.seek_to_sample(si) {
                Ok(p) => {
                    assert!(p.frame_index < fc, "seek frame index in range");
                    assert!(
                        (p.sample_offset_in_frame as u64) < regular,
                        "seek offset below regular frame length"
                    );
                    assert_eq!(
                        p.frame_index as u64 * regular + p.sample_offset_in_frame as u64,
                        si,
                        "seek point must reconstruct the requested sample index"
                    );
                }
                Err(e) => panic!("seek_to_sample({si}) on a seekable stream errored: {e:?}"),
            }
            let _ = dec.decode_from_sample(si);
            let _ = dec.decode_sample_range(si, total);
            let _ = dec.decode_sample_range(0, si);
            if let Ok(it) = dec.frame_iter_from_sample(si) {
                for r in it {
                    if r.is_err() {
                        break;
                    }
                }
            }
        }
        assert!(matches!(
            dec.seek_to_sample(total),
            Err(Error::SampleIndexOutOfRange)
        ));
    }
}

/// Unseekable-mode battery (`spec/01` §4.3): every random-access call
/// must refuse with `SeekTableUnreliable`; linear + explicit-index
/// paths must still return.
fn drive_unseekable(dec: &Decoder<'_>) {
    assert!(!dec.is_seekable());
    let total = dec.total_samples() as u64;
    let probes = [0u64, total / 2, total.saturating_sub(1)];
    for &si in &probes {
        assert!(matches!(
            dec.seek_to_sample(si),
            Err(Error::SeekTableUnreliable)
        ));
        assert!(matches!(
            dec.decode_from_sample(si),
            Err(Error::SeekTableUnreliable)
        ));
        assert!(matches!(
            dec.decode_sample_range(si, total),
            Err(Error::SeekTableUnreliable)
        ));
        assert!(dec.frame_iter_from_sample(si).is_err());
        assert!(matches!(
            dec.seek_to_time(core::time::Duration::ZERO),
            Err(Error::SeekTableUnreliable)
        ));
    }
    let _ = dec.decode_all();
    for r in dec.frame_iter() {
        if r.is_err() {
            break;
        }
    }
    let fc = dec.frames.len();
    if fc > 0 {
        let _ = dec.decode_frame_at(fc / 2);
        for r in dec.frame_iter_from(fc / 2) {
            if r.is_err() {
                break;
            }
        }
    }
}

/// Registry demuxer over the (possibly forged) stream: open, bounded
/// drain, seek probes, bounded drain again.
fn drive_demuxer(bytes: &[u8], probe_seed: u8) {
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);
    let resolver = NoopResolver;
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    let Ok(mut demuxer) = ctx.containers.open_demuxer("tta", input, &resolver) else {
        return;
    };
    let _ = demuxer.duration_micros();
    for _ in 0..MAX_PACKETS {
        if demuxer.next_packet().is_err() {
            break;
        }
    }
    let probes: [(u32, i64); 5] = [
        (0, 0),
        (0, -1),
        (0, i64::MAX),
        (0, probe_seed as i64 * 100_000),
        (1, 0),
    ];
    for (stream_index, pts) in probes {
        let _ = demuxer.seek_to(stream_index, pts);
        for _ in 0..MAX_PACKETS {
            if demuxer.next_packet().is_err() {
                break;
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 7 {
        return;
    }

    // ── Geometry from the structured prefix ────────────────────────
    let channels = ((data[0] as u16) % 6) + 1;
    let bits_per_sample: u16 = 16 + (data[1] % 9) as u16;
    let sample_rate = (u16::from_le_bytes([data[2], data[3]]) as u32 & 0x7FF).max(64);
    let samples_per_channel =
        (u16::from_le_bytes([data[4], data[5]]) as usize).min(MAX_SAMPLES_PER_CHANNEL);
    let probe_seed = data[6];
    if samples_per_channel == 0 {
        return;
    }
    let nch = channels as usize;
    let byte_depth = (bits_per_sample as usize).div_ceil(8);

    let tail = &data[7..];
    let split = tail.len() / 2;
    let pcm_seed = &tail[..split];
    let script = &tail[split..];

    // ── Encode a structurally valid stream ─────────────────────────
    let pcm = build_pcm(pcm_seed, samples_per_channel * nch, byte_depth);
    let Ok(original) = oxideav_tta::encode(&pcm, channels, bits_per_sample, sample_rate) else {
        return;
    };
    let frame_count = {
        let regular = ((sample_rate as u64) * 256 / 245) as usize;
        samples_per_channel.div_ceil(regular)
    };
    let table_len = frame_count * 4 + 4;
    if original.len() < HEADER_LEN + table_len {
        return;
    }

    // ── Forge the entries per the script ───────────────────────────
    let mut forged = original.clone();
    let mut touched = false;
    for rec in script.as_chunks::<5>().0 {
        if frame_count == 0 {
            break;
        }
        let idx = (rec[0] as usize) % frame_count;
        let val = u32::from_le_bytes([rec[1], rec[2], rec[3], rec[4]]);
        let at = HEADER_LEN + idx * 4;
        forged[at..at + 4].copy_from_slice(&val.to_le_bytes());
        touched = true;
    }

    // Variant A: forged entries behind a VALID table CRC — the decoder
    // trusts every value.
    let mut trusted = forged.clone();
    let crc = ieee_crc32(&trusted[HEADER_LEN..HEADER_LEN + table_len - 4]);
    trusted[HEADER_LEN + table_len - 4..HEADER_LEN + table_len].copy_from_slice(&crc.to_le_bytes());

    match Decoder::new(&trusted) {
        Ok(dec) => {
            assert!(dec.is_seekable(), "re-CRC'd table must validate");
            drive_seekable(&dec, probe_seed);
            if !touched {
                // Untouched table: byte-identical to the encoder's
                // stream, so every path reproduces the input PCM.
                assert_eq!(trusted, original);
                let eager = dec.decode_all().expect("intact stream decodes");
                assert_eq!(eager, pcm, "eager decode must reproduce the encoder input");
                let mut lazy = Vec::new();
                for r in dec.frame_iter() {
                    lazy.extend_from_slice(&r.expect("intact frame"));
                }
                assert_eq!(lazy, eager, "frame_iter must agree with decode_all");
                let mid = (samples_per_channel / 2) as u64;
                let suffix = dec.decode_from_sample(mid).expect("suffix decodes");
                assert_eq!(&suffix[..], &pcm[mid as usize * nch..]);
            }
        }
        Err(e) => {
            // The re-CRC'd table parses structurally; only the geometry
            // gates can refuse it, and those are typed.
            let _ = e;
        }
    }

    // Variant B: forged entries behind the STALE original CRC —
    // unseekable mode must engage whenever an entry actually changed.
    if touched && forged != original {
        if let Ok(dec) = Decoder::new(&forged) {
            drive_unseekable(&dec);
        }
    }

    // ── Registry demuxer over both variants ────────────────────────
    drive_demuxer(&trusted, probe_seed);
    if touched {
        drive_demuxer(&forged, probe_seed);
    }
});
