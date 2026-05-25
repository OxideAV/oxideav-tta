//! Criterion benchmarks for the TTA encoder hot paths.
//!
//! Round 127 (depth-mode benchmarks): the encoder mirrors the decoder
//! pipeline in reverse — forward channel decorrelation
//! (`spec/04` §3.1 / §4.1), Stage-B prediction subtract (`spec/03`),
//! Stage-A 8-tap sign-LMS subtract (`spec/02`), zigzag + adaptive
//! Rice emit (`spec/05`), then per-frame body padding + CRC32 +
//! seek-table assembly + stream header (`spec/01`). These benches
//! make the per-format / per-channel-count / per-bps cost visible so
//! future encoder rounds (e.g. a SIMD Rice emitter or a precomputed
//! qm-priming table) have a baseline to A/B against.
//!
//! Scenarios:
//!
//!   - **encode_mono_16bit_44k1_1s**: baseline single-channel cost
//!     (one Stage-A LMS, one Stage-B `prev`, no decorrelation).
//!   - **encode_stereo_16bit_44k1_1s**: stereo baseline — adds the
//!     forward pairwise decorrelation cascade.
//!   - **encode_stereo_24bit_48k_500ms**: highest-bps supported mode
//!     (24-bit) — residuals are wider so the Rice emit is more
//!     expensive per sample.
//!   - **encode_6ch_16bit_48k_250ms**: max channel count (6) — the
//!     `spec/04` §4.1 cascade fires across every channel pair.
//!   - **encode_stereo_16bit_44k1_format2_1s**: format=2
//!     (password-derived qm priming) variant of the stereo baseline.
//!     Cost should land within noise of format=1 — useful as a
//!     regression sentinel for the per-frame qm priming write.
//!
//! Run with:
//!     cargo bench -p oxideav-tta --bench encode

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_tta::{encode, encode_with_password};

fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

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

fn bench_encode_mono_16bit_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm(n, 1, 16);
    let mut g = c.benchmark_group("encode_mono_16bit_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("mono/16/44k1/1s"), |b| {
        b.iter(|| {
            let _bytes = encode(criterion::black_box(&pcm), 1, 16, 44_100).expect("encode");
        });
    });
    g.finish();
}

fn bench_encode_stereo_16bit_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm(n, 2, 16);
    let mut g = c.benchmark_group("encode_stereo_16bit_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2 * 2) as u64));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("stereo/16/44k1/1s"), |b| {
        b.iter(|| {
            let _bytes = encode(criterion::black_box(&pcm), 2, 16, 44_100).expect("encode");
        });
    });
    g.finish();
}

fn bench_encode_stereo_24bit_48k_500ms(c: &mut Criterion) {
    let n = 24_000;
    let pcm = build_pcm(n, 2, 24);
    let mut g = c.benchmark_group("encode_stereo_24bit_48k_500ms");
    g.throughput(Throughput::Bytes((n * 3 * 2) as u64));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("stereo/24/48k/500ms"), |b| {
        b.iter(|| {
            let _bytes = encode(criterion::black_box(&pcm), 2, 24, 48_000).expect("encode");
        });
    });
    g.finish();
}

fn bench_encode_6ch_16bit_48k_250ms(c: &mut Criterion) {
    let n = 12_000;
    let pcm = build_pcm(n, 6, 16);
    let mut g = c.benchmark_group("encode_6ch_16bit_48k_250ms");
    g.throughput(Throughput::Bytes((n * 2 * 6) as u64));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("6ch/16/48k/250ms"), |b| {
        b.iter(|| {
            let _bytes = encode(criterion::black_box(&pcm), 6, 16, 48_000).expect("encode");
        });
    });
    g.finish();
}

fn bench_encode_stereo_16bit_44k1_format2_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm(n, 2, 16);
    let password = b"bench-r127";
    let mut g = c.benchmark_group("encode_stereo_16bit_44k1_format2_1s");
    g.throughput(Throughput::Bytes((n * 2 * 2) as u64));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo/16/44k1/format2/1s"),
        |b| {
            b.iter(|| {
                let _bytes =
                    encode_with_password(criterion::black_box(&pcm), 2, 16, 44_100, password)
                        .expect("encode format=2");
            });
        },
    );
    g.finish();
}

criterion_group!(
    benches,
    bench_encode_mono_16bit_44k1_1s,
    bench_encode_stereo_16bit_44k1_1s,
    bench_encode_stereo_24bit_48k_500ms,
    bench_encode_6ch_16bit_48k_250ms,
    bench_encode_stereo_16bit_44k1_format2_1s,
);
criterion_main!(benches);
