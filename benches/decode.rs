//! Criterion benchmarks for the TTA decoder hot paths.
//!
//! Round 127 (depth-mode benchmarks): oxideav-tta hit saturation in
//! r5 (encoder + decoder feature-complete, format=1 + format=2 round-
//! trip bit-exact through the crate's own production encoder) and
//! gained a cargo-fuzz harness in r124. Per the workspace
//! "saturated -> fuzz/bench/profile" memo this round wires up
//! `criterion` benches so future optimisation rounds can A/B-test
//! their changes. This file covers the **decoder**; sibling files
//! cover `encode` (Stage-A LMS + Rice emit + per-frame CRC) and
//! `roundtrip` (back-to-back encode + decode).
//!
//! Each scenario is self-contained: a deterministic xorshift PCM
//! buffer is encoded on the fly with the production `encode` /
//! `encode_with_password` entry points, then iterated through the
//! decoder. No `docs/` fixtures or external files are read.
//!
//! Scenarios:
//!
//!   - **decode_mono_16bit_44k1_1s**: 1 second of synthesised mono 16-bit
//!     PCM at 44.1 kHz — the "single channel, single-frame-ish" baseline.
//!     Exercises one Stage-A LMS, one Stage-B `prev`, no decorrelation.
//!   - **decode_stereo_16bit_44k1_1s**: 1 second of stereo 16-bit PCM at
//!     44.1 kHz — adds the pairwise inverse decorrelation cascade
//!     (`spec/04`) on top of the mono baseline.
//!   - **decode_stereo_24bit_48k_500ms**: 0.5 s of stereo 24-bit PCM at
//!     48 kHz — higher bit depth means wider residuals and larger Rice
//!     tails, the most expensive supported sample format.
//!   - **decode_6ch_16bit_48k_250ms**: 0.25 s of 6-channel 16-bit PCM at
//!     48 kHz — `spec/04` §4.1 cascade across the max channel count.
//!   - **decode_stereo_16bit_44k1_format2_1s**: same payload as the
//!     stereo baseline but encoded as format=2 with a fixed password,
//!     so the per-frame qm priming path (`spec/07` §3.5) is exercised.
//!
//! Run with:
//!     cargo bench -p oxideav-tta --bench decode

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_tta::{decode, decode_with_password, encode, encode_with_password};

/// Cheap deterministic xorshift32 — synthesises "natural-ish" per-
/// sample values for bench inputs. A pure-DC fixture would compress
/// to almost nothing and hide the Rice + LMS cost, so we mix a low-
/// frequency sinusoidal envelope with small per-sample noise.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Build `n_samples * channels` interleaved i32 PCM with a low-
/// amplitude tone-plus-noise pattern that gives Stage-A LMS something
/// to predict and Rice something to compress, scaled to roughly half
/// of the available range for `bits_per_sample` so 16/24-bit modes
/// both produce realistic residuals.
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
        // Low-freq triangle envelope shared across channels.
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

fn encode_pcm(samples: &[i32], channels: u16, bits_per_sample: u16, sample_rate: u32) -> Vec<u8> {
    encode(samples, channels, bits_per_sample, sample_rate).expect("encode")
}

fn encode_pcm_format2(
    samples: &[i32],
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    password: &[u8],
) -> Vec<u8> {
    encode_with_password(samples, channels, bits_per_sample, sample_rate, password)
        .expect("encode format=2")
}

fn bench_decode_mono_16bit_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm(n, 1, 16);
    let tta = encode_pcm(&pcm, 1, 16, 44_100);
    let mut g = c.benchmark_group("decode_mono_16bit_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("mono/16/44k1/1s"), |b| {
        b.iter(|| {
            let (_info, _samples) = decode(criterion::black_box(&tta)).expect("decode");
        });
    });
    g.finish();
}

fn bench_decode_stereo_16bit_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm(n, 2, 16);
    let tta = encode_pcm(&pcm, 2, 16, 44_100);
    let mut g = c.benchmark_group("decode_stereo_16bit_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2 * 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("stereo/16/44k1/1s"), |b| {
        b.iter(|| {
            let (_info, _samples) = decode(criterion::black_box(&tta)).expect("decode");
        });
    });
    g.finish();
}

fn bench_decode_stereo_24bit_48k_500ms(c: &mut Criterion) {
    let n = 24_000; // 0.5 s @ 48 kHz
    let pcm = build_pcm(n, 2, 24);
    let tta = encode_pcm(&pcm, 2, 24, 48_000);
    let mut g = c.benchmark_group("decode_stereo_24bit_48k_500ms");
    g.throughput(Throughput::Bytes((n * 3 * 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("stereo/24/48k/500ms"), |b| {
        b.iter(|| {
            let (_info, _samples) = decode(criterion::black_box(&tta)).expect("decode");
        });
    });
    g.finish();
}

fn bench_decode_6ch_16bit_48k_250ms(c: &mut Criterion) {
    let n = 12_000; // 0.25 s @ 48 kHz
    let pcm = build_pcm(n, 6, 16);
    let tta = encode_pcm(&pcm, 6, 16, 48_000);
    let mut g = c.benchmark_group("decode_6ch_16bit_48k_250ms");
    g.throughput(Throughput::Bytes((n * 2 * 6) as u64));
    g.bench_function(BenchmarkId::from_parameter("6ch/16/48k/250ms"), |b| {
        b.iter(|| {
            let (_info, _samples) = decode(criterion::black_box(&tta)).expect("decode");
        });
    });
    g.finish();
}

fn bench_decode_stereo_16bit_44k1_format2_1s(c: &mut Criterion) {
    // Format=2 (password-derived qm priming, spec/07 §3.5). The per-
    // frame qm priming write is the only delta vs format=1, so this
    // bench should land within noise of the format=1 stereo baseline
    // — useful as a regression sentinel.
    let n = 44_100;
    let pcm = build_pcm(n, 2, 16);
    let password = b"bench-r127";
    let tta = encode_pcm_format2(&pcm, 2, 16, 44_100, password);
    let mut g = c.benchmark_group("decode_stereo_16bit_44k1_format2_1s");
    g.throughput(Throughput::Bytes((n * 2 * 2) as u64));
    g.bench_function(
        BenchmarkId::from_parameter("stereo/16/44k1/format2/1s"),
        |b| {
            b.iter(|| {
                let (_info, _samples) =
                    decode_with_password(criterion::black_box(&tta), password).expect("decode");
            });
        },
    );
    g.finish();
}

criterion_group!(
    benches,
    bench_decode_mono_16bit_44k1_1s,
    bench_decode_stereo_16bit_44k1_1s,
    bench_decode_stereo_24bit_48k_500ms,
    bench_decode_6ch_16bit_48k_250ms,
    bench_decode_stereo_16bit_44k1_format2_1s,
);
criterion_main!(benches);
