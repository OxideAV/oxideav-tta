//! Criterion benchmarks for the round-209 / round-215 / round-219
//! player-API range surface on [`Decoder`](../src/decoder.rs).
//!
//! Round 234 (depth-mode bench follow-up): the r209 / r215 / r219
//! rounds layered a player-grade convenience surface on top of the
//! round-187 streaming + random-access decode path —
//!
//!   - `Decoder::decode_from_sample(sample_index)` /
//!     `Decoder::frame_iter_from_sample(sample_index)` (r209): seek
//!     to a per-channel sample boundary, then play (eager / lazy)
//!     the tail.
//!   - `Decoder::decode_from_time(d)` /
//!     `Decoder::frame_iter_from_time(d)` /
//!     `Decoder::seek_to_time(d)` /
//!     `Decoder::total_duration()` (r215): duration-keyed analogues
//!     keyed on a `core::time::Duration` from stream start.
//!   - `Decoder::decode_sample_range(start, end)` /
//!     `Decoder::frame_iter_sample_range(start, end)` /
//!     `Decoder::decode_time_range(start, end)` /
//!     `Decoder::frame_iter_time_range(start, end)` (r219):
//!     half-open `[start, end)` range quartet that bounds both
//!     endpoints. The trailing frame is trimmed in-place via
//!     `Vec::truncate`, so frames past `end` are never decoded and
//!     the eager output is exactly `(end - start) * channels`
//!     interleaved entries.
//!
//! `benches/streaming.rs` already covers the r187 base surface
//! (`frame_iter`, `decode_frame_at`, `seek_to_sample`,
//! `frame_iter_from`) and the r198 / r204 parameter cube on it.
//! `benches/decode.rs` covers the eager `decode` / `decode_all`
//! baseline across the same shape cube. What was missing — until
//! this file — was an A/B baseline for the sample- and duration-
//! keyed sugar layered on top, so any future optimisation round
//! (e.g. caching the seek-table lookup for the range surface,
//! hoisting the duration → sample conversion out of the hot path,
//! batching the trailing-frame `Vec::truncate`, or fusing the
//! `seek_to_sample(start) → frame_iter_from(...) → drain prefix`
//! ritual into a single planner pass) has measured numbers per
//! scenario to compare against.
//!
//! All scenarios run against the same multi-frame stream the
//! `streaming.rs` anchor uses (3 seconds of synthesised stereo
//! 16-bit PCM at 44.1 kHz — `132_300` per-channel samples spanning
//! three TTA frames at `regular_frame_samples = floor(44_100 * 256
//! / 245) = 46_073` per `spec/01` §4.1, so the layout is two full
//! frames plus a 40_154-sample tail). The shared shape makes the
//! per-API cost comparison meaningful: tail vs range, eager vs
//! lazy, sample- vs duration-keyed each diff a single dimension
//! against the others.
//!
//! On top of the anchor scenarios, the format=2 cell at the same
//! `stereo16_44k1_3s` shape exercises the `Decoder::new_with_password`
//! reach (r204) through the eager range surface, so the marginal
//! cost of the per-frame qm re-prime (`spec/07` §3.5 / §3.6) is
//! directly comparable against the format=1 anchor.
//!
//! Each scenario constructs the `Decoder` once at bench setup and
//! reuses it across iterations — every TTA frame resets its
//! trackers per `spec/01` §5.1 + `spec/02..05` §3.1, so a shared
//! `Decoder` does not contaminate measurements. The compressed
//! stream is built in-bench via the production `encode` /
//! `encode_with_password` entry points; no checked-in fixture
//! files.
//!
//! Run with:
//!     cargo bench -p oxideav-tta --bench range

use core::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_tta::{encode, encode_with_password, Decoder};

/// Cheap deterministic xorshift32 — mirrors the helper used in the
/// other four TTA bench harnesses (`decode.rs`, `encode.rs`,
/// `roundtrip.rs`, `streaming.rs`) so the bench inputs across all
/// five files come from the same generator.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Build `n_samples * channels` interleaved i32 PCM with the same
/// tone-plus-noise shape the sibling benches use, so the synthesised
/// workload is comparable across all five harnesses.
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

/// Per-channel sample count of the anchor stream: 3 seconds at
/// 44.1 kHz. `regular_frame_samples = floor(44_100 * 256 / 245) =
/// 46_073` per `spec/01` §4.1, so the layout is
/// `132_300 = 46_073 * 2 + 40_154` (two full frames + tail).
const ANCHOR_PER_CHAN_SAMPLES: usize = 132_300;
const ANCHOR_CHANNELS: u16 = 2;
const ANCHOR_BITS_PER_SAMPLE: u16 = 16;
const ANCHOR_SAMPLE_RATE: u32 = 44_100;

/// Build the format=1 anchor stream once, returning the encoded
/// `Vec<u8>` plus the constants the per-bench `Throughput` needs.
fn build_anchor_stream() -> Vec<u8> {
    let pcm = build_pcm(
        ANCHOR_PER_CHAN_SAMPLES,
        ANCHOR_CHANNELS,
        ANCHOR_BITS_PER_SAMPLE,
    );
    encode(
        &pcm,
        ANCHOR_CHANNELS,
        ANCHOR_BITS_PER_SAMPLE,
        ANCHOR_SAMPLE_RATE,
    )
    .expect("encode 3s stereo16 anchor")
}

/// Build the format=2 anchor stream at the same shape as
/// [`build_anchor_stream`], priming Stage-A `qm[0..7]` with the
/// ECMA-182 CRC-64 digest of the supplied password (`spec/07`
/// §3.5–§3.6).
fn build_anchor_stream_password(password: &[u8]) -> Vec<u8> {
    let pcm = build_pcm(
        ANCHOR_PER_CHAN_SAMPLES,
        ANCHOR_CHANNELS,
        ANCHOR_BITS_PER_SAMPLE,
    );
    encode_with_password(
        &pcm,
        ANCHOR_CHANNELS,
        ANCHOR_BITS_PER_SAMPLE,
        ANCHOR_SAMPLE_RATE,
        password,
    )
    .expect("encode 3s stereo16 format=2 anchor")
}

/// PCM byte-throughput for the anchor's `n` per-channel samples.
fn anchor_throughput(n: usize) -> Throughput {
    Throughput::Bytes((n * (ANCHOR_BITS_PER_SAMPLE as usize / 8) * ANCHOR_CHANNELS as usize) as u64)
}

// ───────────── r209 player-API sugar: decode_from_sample /
//                  frame_iter_from_sample ─────────────

/// `Decoder::decode_from_sample(mid)` — eager tail decode from the
/// per-channel sample boundary at the first sample of the middle
/// frame. The cost is `seek_to_sample(mid)` (O(1) per `spec/01`
/// §4.1) plus the decode of every frame from there to end of
/// stream; for the anchor's 3-frame layout the bench measures the
/// cost of decoding the second + third frames.
fn bench_decode_from_sample_mid(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let regular = dec.header.regular_frame_samples() as u64;
    let target_sample = regular; // first per-channel sample of frame index 1
    let tail_per_chan = ANCHOR_PER_CHAN_SAMPLES - regular as usize;
    let mut g = c.benchmark_group("range_decode_from_sample_mid");
    g.throughput(anchor_throughput(tail_per_chan));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/sample=46073"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let pcm = dec
                    .decode_from_sample(criterion::black_box(target_sample))
                    .expect("decode_from_sample");
                criterion::black_box(pcm.len());
            });
        },
    );
    g.finish();
}

/// `Decoder::frame_iter_from_sample(mid)` — lazy analogue. Same
/// underlying work as `decode_from_sample` but per-frame
/// materialisation, so the diff against the eager scenario isolates
/// the `extend_from_slice` accumulation cost of the eager path.
fn bench_frame_iter_from_sample_mid(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let regular = dec.header.regular_frame_samples() as u64;
    let target_sample = regular;
    let tail_per_chan = ANCHOR_PER_CHAN_SAMPLES - regular as usize;
    let mut g = c.benchmark_group("range_frame_iter_from_sample_mid");
    g.throughput(anchor_throughput(tail_per_chan));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/sample=46073"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let iter = dec
                    .frame_iter_from_sample(criterion::black_box(target_sample))
                    .expect("frame_iter_from_sample");
                let mut total = 0usize;
                for frame in iter {
                    let pcm = frame.expect("frame decode");
                    total = total.wrapping_add(pcm.len());
                }
                criterion::black_box(total);
            });
        },
    );
    g.finish();
}

// ───────────── r215 duration-keyed sugar ─────────────

/// `Decoder::decode_from_time(d)` at the midpoint of
/// `total_duration` — exercises the duration → sample-index
/// conversion (`floor(time_ns * sample_rate / 1e9)` widened to
/// `u128`) followed by the same tail decode as
/// `decode_from_sample`. The bench diffs against
/// `decode_from_sample_mid` to isolate the marginal cost of the
/// duration-keyed path.
fn bench_decode_from_time_mid(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let half = dec.total_duration() / 2;
    let mut g = c.benchmark_group("range_decode_from_time_mid");
    g.throughput(anchor_throughput(ANCHOR_PER_CHAN_SAMPLES / 2));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/time=1.5s"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let d = criterion::black_box(half);
                let pcm = dec.decode_from_time(d).expect("decode_from_time");
                criterion::black_box(pcm.len());
            });
        },
    );
    g.finish();
}

/// `Decoder::seek_to_time(d)` — pure duration → sample-index plus
/// `spec/01` §4.1 sample → frame arithmetic. No decode. The bench
/// is a regression sentinel against accidentally turning the
/// constant-time lookup into a linear walk of `self.frames` (mirror
/// of the existing `streaming_seek_to_sample_middle` scenario on
/// the sample-keyed surface).
fn bench_seek_to_time_mid(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let half = dec.total_duration() / 2;
    let mut g = c.benchmark_group("range_seek_to_time_mid");
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/time=1.5s"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let d = criterion::black_box(half);
                let pt = dec.seek_to_time(d).expect("seek_to_time");
                criterion::black_box(pt);
            });
        },
    );
    g.finish();
}

// ───────────── r219 half-open range quartet ─────────────

/// `Decoder::decode_sample_range(quarter, three_quarter)` — eager
/// half-open range across roughly the middle 50 % of the stream.
/// The leading prefix is skipped (`spec/01` §4.1 seek), the
/// trailing frame is trimmed in-place via `Vec::truncate`, so the
/// returned buffer is exactly `(end - start) * channels`
/// interleaved entries. Frames past `end` are never decoded.
fn bench_decode_sample_range_middle_half(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let total = dec.header.total_samples as u64;
    let start = total / 4;
    let end = (3 * total) / 4;
    let span = (end - start) as usize;
    let mut g = c.benchmark_group("range_decode_sample_range_middle_half");
    g.throughput(anchor_throughput(span));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/[25%,75%)"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let pcm = dec
                    .decode_sample_range(criterion::black_box(start), criterion::black_box(end))
                    .expect("decode_sample_range");
                criterion::black_box(pcm.len());
            });
        },
    );
    g.finish();
}

/// `Decoder::frame_iter_sample_range(quarter, three_quarter)` —
/// lazy analogue. Same underlying work, no eager accumulation.
fn bench_frame_iter_sample_range_middle_half(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let total = dec.header.total_samples as u64;
    let start = total / 4;
    let end = (3 * total) / 4;
    let span = (end - start) as usize;
    let mut g = c.benchmark_group("range_frame_iter_sample_range_middle_half");
    g.throughput(anchor_throughput(span));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/[25%,75%)"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let iter = dec
                    .frame_iter_sample_range(criterion::black_box(start), criterion::black_box(end))
                    .expect("frame_iter_sample_range");
                let mut total = 0usize;
                for frame in iter {
                    let pcm = frame.expect("frame decode");
                    total = total.wrapping_add(pcm.len());
                }
                criterion::black_box(total);
            });
        },
    );
    g.finish();
}

/// `Decoder::decode_time_range(d_start, d_end)` — duration-keyed
/// half-open range across the middle 50 %. Pre-floors both
/// endpoints through the same conversion as
/// `decode_from_time`, then forwards to `decode_sample_range`.
fn bench_decode_time_range_middle_half(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let dur = dec.total_duration();
    let start = dur / 4;
    let end = (dur / 4) * 3;
    let mut g = c.benchmark_group("range_decode_time_range_middle_half");
    // Throughput is approximate (the floor conversion can shift the
    // exact span by ≤ 1 sample per endpoint); we report the
    // sample-keyed span as a stable proxy so cross-bench numbers
    // remain comparable.
    let total = dec.header.total_samples as u64;
    let span = ((3 * total / 4) - (total / 4)) as usize;
    g.throughput(anchor_throughput(span));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/[25%,75%)"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let s = criterion::black_box(start);
                let e = criterion::black_box(end);
                let pcm = dec.decode_time_range(s, e).expect("decode_time_range");
                criterion::black_box(pcm.len());
            });
        },
    );
    g.finish();
}

/// `Decoder::decode_sample_range(0, total_samples)` — full-stream
/// boundary case. Equivalent to `decode_all` per the half-open
/// contract; the diff against `decode.rs::decode_stereo_16bit_44k1_3s`
/// is the cost of routing through the range surface's
/// `seek_to_sample(0) → frame_iter_from(0) → drain prefix-of-zero`
/// path vs the eager `decode_all` direct call.
fn bench_decode_sample_range_full(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let total = dec.header.total_samples as u64;
    let mut g = c.benchmark_group("range_decode_sample_range_full");
    g.throughput(anchor_throughput(ANCHOR_PER_CHAN_SAMPLES));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/[0,total)"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let pcm = dec
                    .decode_sample_range(0, criterion::black_box(total))
                    .expect("decode_sample_range full");
                criterion::black_box(pcm.len());
            });
        },
    );
    g.finish();
}

/// `Decoder::decode_sample_range(s, s)` at the midpoint — empty
/// range boundary case. The implementation short-circuits before
/// touching the bitstream, so the cost is the validation + the
/// empty-`Vec` allocation. Regression sentinel against accidentally
/// routing the empty range through the seek + frame_iter path.
fn bench_decode_sample_range_empty(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let total = dec.header.total_samples as u64;
    let mid = total / 2;
    let mut g = c.benchmark_group("range_decode_sample_range_empty");
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/[mid,mid)"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let s = criterion::black_box(mid);
                let pcm = dec
                    .decode_sample_range(s, s)
                    .expect("decode_sample_range empty");
                criterion::black_box(pcm.len());
            });
        },
    );
    g.finish();
}

// ───────────── format=2 reach ─────────────

/// `Decoder::decode_sample_range(quarter, three_quarter)` against
/// the format=2 anchor stream — same parameter point as
/// `bench_decode_sample_range_middle_half`, but with the per-frame
/// qm re-prime (`spec/07` §3.5–§3.6) on every frame init. The
/// marginal cost over the format=1 sibling is the qm priming write
/// inside the frame-init block.
fn bench_decode_sample_range_format2_middle_half(c: &mut Criterion) {
    let password: &[u8] = b"oxideav-tta-r234-range-bench";
    let tta = build_anchor_stream_password(password);
    let dec = Decoder::new_with_password(&tta, password).expect("format=2 decoder construct");
    let total = dec.header.total_samples as u64;
    let start = total / 4;
    let end = (3 * total) / 4;
    let span = (end - start) as usize;
    let mut g = c.benchmark_group("range_decode_sample_range_format2_middle_half");
    g.throughput(anchor_throughput(span));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s_format2/[25%,75%)"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let pcm = dec
                    .decode_sample_range(criterion::black_box(start), criterion::black_box(end))
                    .expect("decode_sample_range format=2");
                criterion::black_box(pcm.len());
            });
        },
    );
    g.finish();
}

// ───────────── duration-arithmetic primitive ─────────────

/// `Decoder::total_duration()` — pure integer arithmetic on
/// `(total_samples, sample_rate)` at nanosecond granularity (no
/// floating-point intermediates), so the call is sub-nanosecond
/// expected. Bench is a regression sentinel against accidentally
/// promoting the helper to a heavier computation (e.g. switching to
/// `Duration::from_secs_f64` arithmetic).
fn bench_total_duration(c: &mut Criterion) {
    let tta = build_anchor_stream();
    let dec = Decoder::new(&tta).expect("anchor decoder construct");
    let mut g = c.benchmark_group("range_total_duration");
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo16_44k1_3s/total_duration"),
        |b| {
            b.iter(|| {
                let dec = criterion::black_box(&dec);
                let d: Duration = dec.total_duration();
                criterion::black_box(d);
            });
        },
    );
    g.finish();
}

criterion_group!(
    benches,
    bench_decode_from_sample_mid,
    bench_frame_iter_from_sample_mid,
    bench_decode_from_time_mid,
    bench_seek_to_time_mid,
    bench_decode_sample_range_middle_half,
    bench_frame_iter_sample_range_middle_half,
    bench_decode_time_range_middle_half,
    bench_decode_sample_range_full,
    bench_decode_sample_range_empty,
    bench_decode_sample_range_format2_middle_half,
    bench_total_duration,
);
criterion_main!(benches);
