//! Criterion benchmarks for the framework raw-`.tta` **demuxer**
//! ([`src/registry.rs`](../src/registry.rs)).
//!
//! Round 319 (depth-mode bench): the framework `Demuxer` reached
//! through the default-on `registry` feature parses the TTA1 header +
//! seek table at open (`spec/01-bitstream-framing.md` §3–§4), emits
//! one self-contained mini-TTA1 packet per audio frame
//! (`build_single_frame_file`), and answers `seek_to(pts)` in O(1) off
//! the cumulative seek-table offsets (`spec/01` §4.1: every non-last
//! frame holds exactly `floor(sample_rate * 256 / 245)` per-channel
//! samples, so the containing frame is `pts / regular_samples`). That
//! surface was given a `demuxer` cargo-fuzz target in r299 (panic-
//! freedom + cross-API agreement) but no Criterion harness — so any
//! future optimisation round (e.g. lazy mini-file assembly, caching
//! the cumulative offsets, hoisting the open-time per-frame bounds
//! check) has no baseline to A/B against. This file closes that gap.
//!
//! The demuxer is driven through the **same public path the host app
//! takes** — `RuntimeContext::new()` + `register(ctx)` +
//! `ctx.containers.open_demuxer("tta", input, &resolver)` — not any
//! crate-private entry point, so the numbers reflect production cost
//! including the `ReadSeek` read-to-end and the registry lookup.
//!
//! Scenarios (the same parameter cube as the sibling `decode.rs` /
//! `streaming.rs` benches so demuxer cost is comparable per shape):
//!
//!   - **open**: `open_demuxer` alone — read-to-end + optional ID3v2
//!     skip + 22-byte header parse + `4*frame_count+4`-byte seek-table
//!     parse + the open-time per-frame byte-window bounds check. This
//!     is the O(frame_count) cost a player pays once per file. Run
//!     across mono16-44k1-1s, stereo16-44k1-1s, stereo24-48k-500ms,
//!     6ch16-48k-250ms and the format=2 stereo cell.
//!   - **drain**: open + `next_packet` looped to `Eof`. Forces the
//!     per-frame `build_single_frame_file` mini-TTA1 re-prefix (new
//!     22-byte header + 1-entry seek table + the frame body copied
//!     verbatim) for every frame in the stream. This is the
//!     allocation-heavy hot path a streaming consumer hits.
//!   - **seek_to**: open once, then `seek_to(0, pts)` at a mid-stream
//!     sample. Pure `spec/01` §4.1 sample → frame arithmetic + cursor
//!     re-anchor — sub-microsecond expected; the bench is a regression
//!     sentinel against accidentally turning the O(1) lookup into a
//!     linear walk of the frame table.
//!
//! Every stream is synthesised in-bench from a deterministic xorshift
//! seed (mirroring `decode.rs`) and encoded once at setup via the
//! production `encode` / `encode_with_password` entry points — no
//! checked-in fixture files, no `docs/` reads.
//!
//! Run with:
//!     cargo bench -p oxideav-tta --bench demuxer

use std::io::Cursor;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_core::{CodecId, CodecResolver, ProbeContext, ReadSeek, RuntimeContext};
use oxideav_tta::{encode, encode_with_password, register};

/// Cheap deterministic xorshift32 — mirrors the helper used in the
/// sibling TTA bench harnesses (`decode.rs`, `encode.rs`,
/// `roundtrip.rs`, `streaming.rs`) so bench inputs across every file
/// come from the same generator.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Build `n_samples * channels` interleaved i32 PCM with a low-
/// amplitude tone-plus-noise pattern (identical shape to
/// `decode.rs::build_pcm`) so the encoded streams these benches demux
/// are the same realistic-residual streams the decode benches consume.
fn build_pcm(n_samples: usize, channels: u16, bits_per_sample: u16) -> Vec<i32> {
    let nch = channels as usize;
    let mut out = Vec::with_capacity(n_samples * nch);
    let mut state: u32 = 0xCAFE_F00D;
    let amp = if bits_per_sample <= 16 {
        1 << 13 // ~25% of 16-bit range
    } else {
        1 << 21 // ~25% of 24-bit range
    };
    for s in 0..n_samples {
        let phase = (s % 256) as i32 - 128;
        let env = (phase * amp) / 128;
        for ch in 0..nch {
            let noise = (xorshift32(&mut state) as i32) >> 24; // -128..127
            let chan_bias = (ch as i32) * (amp / 16);
            out.push(env + chan_bias + noise);
        }
    }
    out
}

/// A resolver that never maps a tag — the TTA demuxer self-describes
/// its single stream and ignores the resolver, so this is sufficient
/// (same no-op resolver the `demuxer` fuzz target uses).
struct NoopResolver;
impl CodecResolver for NoopResolver {
    fn resolve_tag(&self, _ctx: &ProbeContext) -> Option<CodecId> {
        None
    }
}

/// Open the registered raw-`.tta` demuxer over `tta` through the public
/// `RuntimeContext` + `ContainerRegistry::open_demuxer` path — the same
/// path the host app and the `demuxer` fuzz target take.
fn open_tta_demuxer(tta: &[u8]) -> Box<dyn oxideav_core::Demuxer> {
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);
    let resolver = NoopResolver;
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(tta.to_vec()));
    ctx.containers
        .open_demuxer("tta", input, &resolver)
        .expect("open_demuxer")
}

/// One parameter-cube cell: a label plus its synthesised, encoded TTA1
/// byte stream and a mid-stream pts to seek to.
struct Cell {
    label: &'static str,
    /// Throughput denominator = decoded PCM byte count.
    pcm_bytes: u64,
    tta: Vec<u8>,
    /// A sample index roughly in the middle of the stream, for the
    /// `seek_to` scenario.
    mid_pts: i64,
}

fn cell_format1(label: &'static str, n: usize, channels: u16, bps: u16, sample_rate: u32) -> Cell {
    let pcm = build_pcm(n, channels, bps);
    let tta = encode(&pcm, channels, bps, sample_rate).expect("encode");
    let byte_depth = bps.div_ceil(8) as u64;
    Cell {
        label,
        pcm_bytes: n as u64 * channels as u64 * byte_depth,
        tta,
        mid_pts: (n / 2) as i64,
    }
}

fn cell_format2(
    label: &'static str,
    n: usize,
    channels: u16,
    bps: u16,
    sample_rate: u32,
    password: &[u8],
) -> Cell {
    let pcm = build_pcm(n, channels, bps);
    let tta =
        encode_with_password(&pcm, channels, bps, sample_rate, password).expect("encode format=2");
    let byte_depth = bps.div_ceil(8) as u64;
    Cell {
        label,
        pcm_bytes: n as u64 * channels as u64 * byte_depth,
        tta,
        mid_pts: (n / 2) as i64,
    }
}

/// The shared parameter cube. Sample counts chosen so multi-frame
/// streams arise (`regular_frame_samples = floor(sr * 256 / 245)` is
/// ~46_073 at 44.1 kHz and ~50_155 at 48 kHz, so 1 s @ 44.1 kHz spans
/// two frames; the cube exercises both single- and multi-frame
/// streams for the `next_packet` drain).
fn cube() -> Vec<Cell> {
    vec![
        cell_format1("mono16_44k1_1s", 44_100, 1, 16, 44_100),
        cell_format1("stereo16_44k1_1s", 44_100, 2, 16, 44_100),
        cell_format1("stereo24_48k_500ms", 24_000, 2, 24, 48_000),
        cell_format1("6ch16_48k_250ms", 12_000, 6, 16, 48_000),
        cell_format2(
            "stereo16_44k1_format2_1s",
            44_100,
            2,
            16,
            44_100,
            b"bench-r319",
        ),
    ]
}

fn bench_open(c: &mut Criterion) {
    let cube = cube();
    let mut g = c.benchmark_group("demuxer_open");
    for cell in &cube {
        g.throughput(Throughput::Bytes(cell.tta.len() as u64));
        g.bench_with_input(BenchmarkId::from_parameter(cell.label), cell, |b, cell| {
            b.iter(|| {
                let dmx = open_tta_demuxer(criterion::black_box(&cell.tta));
                criterion::black_box(dmx.streams().len());
            });
        });
    }
    g.finish();
}

fn bench_drain(c: &mut Criterion) {
    let cube = cube();
    let mut g = c.benchmark_group("demuxer_drain");
    for cell in &cube {
        // Throughput keyed on decoded PCM byte count so the number is
        // comparable to the sibling decode benches.
        g.throughput(Throughput::Bytes(cell.pcm_bytes));
        g.bench_with_input(BenchmarkId::from_parameter(cell.label), cell, |b, cell| {
            b.iter(|| {
                let mut dmx = open_tta_demuxer(criterion::black_box(&cell.tta));
                let mut packets = 0usize;
                while let Ok(pkt) = dmx.next_packet() {
                    packets += pkt.data.len();
                }
                criterion::black_box(packets);
            });
        });
    }
    g.finish();
}

fn bench_seek_to(c: &mut Criterion) {
    let cube = cube();
    let mut g = c.benchmark_group("demuxer_seek_to");
    for cell in &cube {
        // The demuxer is opened once per iteration (cheap relative to a
        // decode) and re-used; `seek_to` is the measured constant-time
        // lookup. Opening inside the closure keeps each iteration
        // independent without sharing a `&mut` across the harness.
        g.bench_with_input(BenchmarkId::from_parameter(cell.label), cell, |b, cell| {
            let mut dmx = open_tta_demuxer(&cell.tta);
            b.iter(|| {
                let landed = dmx
                    .seek_to(0, criterion::black_box(cell.mid_pts))
                    .expect("seek_to");
                criterion::black_box(landed);
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_open, bench_drain, bench_seek_to);
criterion_main!(benches);
