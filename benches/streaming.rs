//! Criterion benchmarks for the round-187 streaming + random-access
//! decode surface on [`Decoder`](../src/decoder.rs).
//!
//! Round 193 (depth-mode bench follow-up): r187 layered a streaming +
//! random-access decode API on top of the existing eager `decode_all`
//! path (`Decoder::frame_iter`, `Decoder::decode_frame_at`,
//! `Decoder::seek_to_sample`, `Decoder::frame_iter_from`), and r190
//! added a `streaming_decode` cargo-fuzz target that asserts cross-API
//! agreement. What was missing was a Criterion harness to make the
//! per-API cost visible — without it any future optimisation round
//! (e.g. caching the seek-table lookup, batching the per-frame state
//! reset, hoisting the qm priming write out of the hot init loop) has
//! no baseline to A/B against.
//!
//! Round 198 (depth-mode parameter-cube extension): the original four
//! scenarios all run against a single 3 s stereo 16-bit 44.1 kHz
//! stream, which mirrors only one cell of the parameter cube the
//! sibling `decode.rs` / `encode.rs` / `roundtrip.rs` benches cover
//! (mono16, stereo16, stereo24, 6ch16, format=2). The r198 addition
//! adds a `streaming_frame_iter_cube` group that walks `frame_iter`
//! across the format=1 sub-cube (mono16-44k1-1s, stereo24-48k-500ms,
//! 6ch16-48k-250ms), plus a `streaming_decode_frame_at_cube` group
//! that picks the middle frame of each cell so random-access cost is
//! visible per shape (single-frame cells pick frame 0). Format=2 is
//! omitted because the current public streaming surface
//! (`Decoder::new` → `frame_iter` / `decode_frame_at`) is format=1
//! only — format=2 reaches PCM through the eager
//! [`oxideav_tta::decode_with_password`] path, which is already
//! benchmarked by `decode.rs::decode_stereo_16bit_44k1_format2_1s`.
//! These scenarios give future optimisation rounds A/B baselines
//! across the actual TTA parameter space rather than the original
//! single stereo16-44k1 point. The original four scenarios are kept
//! verbatim as the per-API comparison anchor — the new scenarios are
//! additive.
//!
//! All four (original) entry points decode against the **same** multi-
//! frame stream (3 seconds of synthesised stereo 16-bit PCM at 44.1
//! kHz, which `regular_frame_samples = floor(44_100 * 256 / 245) =
//! 46_073` per `spec/01` §4.1 splits across **three** TTA frames — two
//! full + one tail), so the scenarios are directly comparable:
//!
//!   - **streaming_frame_iter_3s_stereo16**: sequential lazy decode
//!     via `frame_iter`. Should be within noise of the eager
//!     `decode_all` baseline (`decode.rs::decode_stereo_16bit_44k1_1s`
//!     × 3) since the work is identical — only the output buffering
//!     differs.
//!   - **streaming_decode_frame_at_middle**: random-access decode of
//!     **one** frame in the middle of the stream via
//!     `Decoder::decode_frame_at(1)`. Cost is the per-frame Rice +
//!     Stage-A LMS + Stage-B + decorrelation work; the bench gives
//!     future random-access consumers (e.g. an MKV cue-driven seek)
//!     a measured "one frame's worth of decode" number.
//!   - **streaming_seek_to_sample_middle**: pure `seek_to_sample`
//!     lookup — no decode, just the `spec/01` §4.1 sample → frame
//!     arithmetic. Sub-microsecond expected; the bench is a
//!     regression sentinel against accidentally turning the constant-
//!     time lookup into a linear walk of `self.frames`.
//!   - **streaming_frame_iter_from_middle**: resume-from-seek via
//!     `frame_iter_from(1)`. Decodes frames `[1, 2]` only — should be
//!     roughly 2/3 of the `frame_iter` cost (= the work for the two
//!     unfrozen tail frames).
//!
//! Each scenario reuses the same `Decoder<'a>` across iterations
//! (the decoder is `Clone` and stateless w.r.t. decode — every frame
//! resets its trackers per `spec/01` §5.1 + `spec/02..05` §3.1, so
//! reusing it does not contaminate measurements). The compressed
//! stream is encoded once at bench setup via the production `encode`
//! entry point — no checked-in fixture files.
//!
//! Run with:
//!     cargo bench -p oxideav-tta --bench streaming

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_tta::{encode, Decoder};

/// Cheap deterministic xorshift32 — mirrors the helper used in the
/// other three TTA bench harnesses (`decode.rs`, `encode.rs`,
/// `roundtrip.rs`) so the bench inputs across all four files come
/// from the same generator.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Build `n_samples * channels` interleaved i32 PCM with the same
/// tone-plus-noise shape the sibling decode/encode/roundtrip benches
/// use, so the synthesised workload is comparable across all four
/// harnesses. Amplitude is scaled to ~25 % of the bit depth range so
/// Stage-A LMS has something to predict and Rice has non-trivial
/// residuals to emit.
fn build_pcm(n_samples: usize, channels: u16, bits_per_sample: u16) -> Vec<i32> {
    let nch = channels as usize;
    let mut out = Vec::with_capacity(n_samples * nch);
    let mut state: u32 = 0xCAFE_F00D;
    let amp = if bits_per_sample <= 16 {
        1 << 13
    } else {
        1 << 21
    };
    for s in 0..n_samples {
        let phase = (s % 256) as i32 - 128;
        let env = (phase * amp) / 128;
        for ch in 0..nch {
            let noise = (xorshift32(&mut state) as i32) >> 24;
            let chan_bias = (ch as i32) * (amp / 16);
            out.push(env + chan_bias + noise);
        }
    }
    out
}

/// Build the shared 3-second stereo 16-bit 44.1 kHz TTA byte stream
/// used by every scenario in this file. 3 s × 44_100 = 132_300
/// per-channel samples; `regular_frame_samples = floor(44_100 * 256 /
/// 245) = 46_073` per `spec/01` §4.1, so the stream spans 3 frames
/// (`132_300 = 46_073 * 2 + 40_154`). Returns the (pcm_len_per_chan,
/// channels, bps, tta_bytes) tuple so the per-bench `Throughput` can
/// be set against the PCM size (callers report decode throughput,
/// not compressed throughput).
fn build_three_frame_stereo_stream() -> (usize, u16, u16, Vec<u8>) {
    const N: usize = 132_300; // 3 s @ 44.1 kHz
    let pcm = build_pcm(N, 2, 16);
    let tta = encode(&pcm, 2, 16, 44_100).expect("encode 3s stereo16");
    (N, 2, 16, tta)
}

fn bench_streaming_frame_iter(c: &mut Criterion) {
    let (n, nch, bps, tta) = build_three_frame_stereo_stream();
    let dec = Decoder::new(&tta).expect("decoder construct");
    assert_eq!(dec.frames.len(), 3, "expected 3-frame stream layout");
    let mut g = c.benchmark_group("streaming_frame_iter_3s_stereo16");
    g.throughput(Throughput::Bytes(
        (n * (bps as usize / 8) * nch as usize) as u64,
    ));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("stereo/16/44k1/3s"), |b| {
        b.iter(|| {
            let dec = criterion::black_box(&dec);
            let mut total = 0usize;
            for frame in dec.frame_iter() {
                let pcm = frame.expect("frame decode");
                total = total.wrapping_add(pcm.len());
            }
            criterion::black_box(total);
        });
    });
    g.finish();
}

fn bench_streaming_decode_frame_at_middle(c: &mut Criterion) {
    let (_n, nch, bps, tta) = build_three_frame_stereo_stream();
    let dec = Decoder::new(&tta).expect("decoder construct");
    // Random-access decode of the middle frame: full per-frame work
    // (Rice + LMS + Stage-B + decorr cascade + CRC32 verify) without
    // the cost of the preceding frame. r187 makes this legitimate
    // because every frame resets state per spec/01 §5.1.
    let target = 1usize;
    assert!(target < dec.frames.len(), "target frame must be in range");
    let mid_frame_samples = dec.frames[target].sample_count as usize;
    let mut g = c.benchmark_group("streaming_decode_frame_at_middle");
    g.throughput(Throughput::Bytes(
        (mid_frame_samples * (bps as usize / 8) * nch as usize) as u64,
    ));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("stereo/16/44k1/frame=1"), |b| {
        b.iter(|| {
            let dec = criterion::black_box(&dec);
            let pcm = dec.decode_frame_at(target).expect("decode_frame_at");
            criterion::black_box(pcm.len());
        });
    });
    g.finish();
}

fn bench_streaming_seek_to_sample_middle(c: &mut Criterion) {
    let (_n, _nch, _bps, tta) = build_three_frame_stereo_stream();
    let dec = Decoder::new(&tta).expect("decoder construct");
    // The middle frame's first per-channel sample: forces the
    // `spec/01` §4.1 `sample_index / regular_frame_samples` path
    // without trivially returning 0. Pure arithmetic — no decode.
    let regular = dec.header.regular_frame_samples() as u64;
    let target_sample = regular; // first sample of frame index 1
    let mut g = c.benchmark_group("streaming_seek_to_sample_middle");
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo/44k1/sample=46073"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let pt = dec
                    .seek_to_sample(criterion::black_box(target_sample))
                    .expect("seek_to_sample");
                criterion::black_box(pt);
            });
        },
    );
    g.finish();
}

fn bench_streaming_frame_iter_from_middle(c: &mut Criterion) {
    let (_n, nch, bps, tta) = build_three_frame_stereo_stream();
    let dec = Decoder::new(&tta).expect("decoder construct");
    // `frame_iter_from(1)` decodes frames [1, 2] only — the bench
    // value is the resume-from-seek cost (= what an interactive
    // seek-and-play path actually pays, on top of the constant-time
    // `seek_to_sample` lookup measured above).
    let start = 1usize;
    let resumed_samples: usize = dec.frames[start..]
        .iter()
        .map(|f| f.sample_count as usize)
        .sum();
    let mut g = c.benchmark_group("streaming_frame_iter_from_middle");
    g.throughput(Throughput::Bytes(
        (resumed_samples * (bps as usize / 8) * nch as usize) as u64,
    ));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("stereo/16/44k1/start=1"), |b| {
        b.iter(|| {
            let dec = criterion::black_box(&dec);
            let mut total = 0usize;
            for frame in dec.frame_iter_from(start) {
                let pcm = frame.expect("frame decode");
                total = total.wrapping_add(pcm.len());
            }
            criterion::black_box(total);
        });
    });
    g.finish();
}

// ───────────── r198 parameter-cube extension ─────────────
//
// The four original scenarios above are anchored to a single 3 s
// stereo 16-bit 44.1 kHz stream so the per-API costs (frame_iter vs
// decode_all vs decode_frame_at vs seek_to_sample vs frame_iter_from)
// are directly comparable on identical PCM. The scenarios below sweep
// the streaming `frame_iter` (and the random-access `decode_frame_at`)
// across the same shape cube the sibling `decode.rs` baseline covers
// (mono16-44k1-1s, stereo24-48k-500ms, 6ch16-48k-250ms, format=2
// stereo16-44k1-1s) so future optimisation rounds — e.g. caching the
// per-channel Stage-A LMS init, hoisting the qm priming write,
// switching the per-frame CRC32 verify off the byte loop, batching the
// Rice unary fast path on wide-residual 24-bit input — have an A/B
// baseline per parameter cell rather than only at the stereo16
// anchor. PCM is built with the same `build_pcm` helper used by the
// other three benches so the synthesised workload is the same as the
// eager `decode.rs` numbers; no checked-in fixtures.

/// Build the multi-frame TTA1 byte stream for one parameter-cube cell
/// and return `(per_channel_samples, channels, bits_per_sample,
/// tta_bytes)` so callers can set `Throughput` against the PCM size.
fn build_cube_stream(
    n_samples: usize,
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
) -> (usize, u16, u16, Vec<u8>) {
    let pcm = build_pcm(n_samples, channels, bits_per_sample);
    let tta = encode(&pcm, channels, bits_per_sample, sample_rate).expect("cube encode");
    (n_samples, channels, bits_per_sample, tta)
}

/// Sweep `frame_iter` across the (channels × bps × sample_rate) cube.
/// Each cell builds its TTA byte stream once at bench setup, constructs
/// the [`Decoder`] once, then iterates `frame_iter` per bench sample.
/// The per-iteration work is the full Rice + Stage-A LMS + Stage-B +
/// decorrelation cascade across every frame in the stream — exactly
/// what the eager `decode.rs` baseline measures, but routed through the
/// lazy iterator so any future optimisation that changes only one of
/// the two paths surfaces here.
///
/// Format=2 is intentionally **omitted** from this cube: the streaming
/// surface (`Decoder::new` → `frame_iter`) is format=1-only via the
/// current public API. The format=2 path is reached through
/// [`oxideav_tta::decode_with_password`], which is an eager call rather
/// than a streaming one. Adding a streaming format=2 bench cell would
/// either require widening the public API surface (a separate
/// concern from this depth-mode bench round) or fall back to the eager
/// `decode_with_password` — which is already covered by the
/// `decode.rs` baseline cell `decode_stereo_16bit_44k1_format2_1s`. So
/// the streaming cube tracks format=1 only and the eager bench covers
/// format=2.
fn bench_streaming_frame_iter_cube(c: &mut Criterion) {
    // (name, n_samples, channels, bps, sample_rate)
    let cells: &[(&str, usize, u16, u16, u32)] = &[
        ("mono16_44k1_1s", 44_100, 1, 16, 44_100),
        ("stereo24_48k_500ms", 24_000, 2, 24, 48_000),
        ("ch6_16bit_48k_250ms", 12_000, 6, 16, 48_000),
    ];
    let mut g = c.benchmark_group("streaming_frame_iter_cube");
    g.sample_size(20);
    for (label, n, nch, bps, sr) in cells.iter().copied() {
        let (_, _, _, tta) = build_cube_stream(n, nch, bps, sr);
        let dec = Decoder::new(&tta).expect("decoder construct");
        g.throughput(Throughput::Bytes(
            (n * (bps as usize / 8) * nch as usize) as u64,
        ));
        g.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let mut total = 0usize;
                for frame in dec.frame_iter() {
                    let pcm = frame.expect("frame decode");
                    total = total.wrapping_add(pcm.len());
                }
                criterion::black_box(total);
            });
        });
    }
    g.finish();
}

/// Sweep `decode_frame_at` across the same cube as
/// `bench_streaming_frame_iter_cube`. For multi-frame cells the target
/// is `frames.len() / 2` (middle frame); single-frame cells decode
/// frame 0 so the bench still reports a measurement for that shape
/// rather than silently skipping. Random-access cost is the per-frame
/// init (LMS and Rice tracker reset) plus the single frame's Rice,
/// LMS, Stage-B and decorrelation work — i.e. what an MKV or MP4
/// cue-driven seek-then-play would pay per landing point. Per the
/// format=2 omission rationale above, this cube tracks format=1 only.
fn bench_streaming_decode_frame_at_cube(c: &mut Criterion) {
    let cells: &[(&str, usize, u16, u16, u32)] = &[
        ("mono16_44k1_1s", 44_100, 1, 16, 44_100),
        ("stereo24_48k_500ms", 24_000, 2, 24, 48_000),
        ("ch6_16bit_48k_250ms", 12_000, 6, 16, 48_000),
    ];
    let mut g = c.benchmark_group("streaming_decode_frame_at_cube");
    g.sample_size(20);
    for (label, n, nch, bps, sr) in cells.iter().copied() {
        let (_, _, _, tta) = build_cube_stream(n, nch, bps, sr);
        let dec = Decoder::new(&tta).expect("decoder construct");
        let n_frames = dec.frames.len();
        // Pick the middle frame for multi-frame streams; fall back to
        // frame 0 for the rare single-frame shape so every cube cell
        // contributes a measurement.
        let target = if n_frames > 1 { n_frames / 2 } else { 0 };
        let frame_pcm_samples = dec.frames[target].sample_count as usize;
        g.throughput(Throughput::Bytes(
            (frame_pcm_samples * (bps as usize / 8) * nch as usize) as u64,
        ));
        g.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let pcm = dec.decode_frame_at(target).expect("decode_frame_at");
                criterion::black_box(pcm.len());
            });
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_streaming_frame_iter,
    bench_streaming_decode_frame_at_middle,
    bench_streaming_seek_to_sample_middle,
    bench_streaming_frame_iter_from_middle,
    bench_streaming_frame_iter_cube,
    bench_streaming_decode_frame_at_cube,
);
criterion_main!(benches);
