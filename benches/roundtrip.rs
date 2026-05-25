//! Criterion benchmarks for the TTA encode + decode roundtrip — the
//! realistic "encode a clip, decode every sample back" path.
//!
//! Round 127 (depth-mode benchmarks): the decoder and encoder land in
//! lockstep against each other (every per-channel state — Rice
//! tracker, Stage-A LMS, Stage-B `prev`, decorrelation cascade — has
//! to roundtrip bit-exactly per `audit/07` §6.2-5). These benches
//! measure the realistic consumer cost of encoding a clip and
//! decoding every sample back, the way a transcoder or a
//! "compress + verify" pipeline would.
//!
//! Each iteration also asserts the decoded sample count matches the
//! input so a state-machine drift would show up as a `panic!` in the
//! bench output rather than silent miscompression.
//!
//! Scenarios:
//!
//!   - **roundtrip_mono_16bit_44k1_1s**: single channel baseline.
//!   - **roundtrip_stereo_16bit_44k1_1s**: stereo baseline — adds the
//!     pairwise decorrelation forward + inverse cascade.
//!   - **roundtrip_stereo_24bit_48k_500ms**: highest-bps supported
//!     mode (24-bit), wider residuals through Rice + LMS.
//!   - **roundtrip_6ch_16bit_48k_250ms**: max channel count (6),
//!     `spec/04` §4.1 cascade across every pair.
//!   - **roundtrip_stereo_16bit_44k1_format2_1s**: format=2
//!     (password-derived qm priming, `spec/07` §3.5) full roundtrip.
//!
//! Run with:
//!     cargo bench -p oxideav-tta --bench roundtrip

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_tta::{decode, decode_with_password, encode, encode_with_password};

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

fn bench_roundtrip_mono_16bit_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm(n, 1, 16);
    let mut g = c.benchmark_group("roundtrip_mono_16bit_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("mono/16/44k1/1s"), |b| {
        b.iter(|| {
            let tta = encode(criterion::black_box(&pcm), 1, 16, 44_100).expect("encode");
            let (_info, samples) = decode(&tta).expect("decode");
            assert_eq!(samples.len(), pcm.len());
        });
    });
    g.finish();
}

fn bench_roundtrip_stereo_16bit_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm(n, 2, 16);
    let mut g = c.benchmark_group("roundtrip_stereo_16bit_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2 * 2) as u64));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("stereo/16/44k1/1s"), |b| {
        b.iter(|| {
            let tta = encode(criterion::black_box(&pcm), 2, 16, 44_100).expect("encode");
            let (_info, samples) = decode(&tta).expect("decode");
            assert_eq!(samples.len(), pcm.len());
        });
    });
    g.finish();
}

fn bench_roundtrip_stereo_24bit_48k_500ms(c: &mut Criterion) {
    let n = 24_000;
    let pcm = build_pcm(n, 2, 24);
    let mut g = c.benchmark_group("roundtrip_stereo_24bit_48k_500ms");
    g.throughput(Throughput::Bytes((n * 3 * 2) as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("stereo/24/48k/500ms"), |b| {
        b.iter(|| {
            let tta = encode(criterion::black_box(&pcm), 2, 24, 48_000).expect("encode");
            let (_info, samples) = decode(&tta).expect("decode");
            assert_eq!(samples.len(), pcm.len());
        });
    });
    g.finish();
}

fn bench_roundtrip_6ch_16bit_48k_250ms(c: &mut Criterion) {
    let n = 12_000;
    let pcm = build_pcm(n, 6, 16);
    let mut g = c.benchmark_group("roundtrip_6ch_16bit_48k_250ms");
    g.throughput(Throughput::Bytes((n * 2 * 6) as u64));
    g.sample_size(20);
    g.bench_function(BenchmarkId::from_parameter("6ch/16/48k/250ms"), |b| {
        b.iter(|| {
            let tta = encode(criterion::black_box(&pcm), 6, 16, 48_000).expect("encode");
            let (_info, samples) = decode(&tta).expect("decode");
            assert_eq!(samples.len(), pcm.len());
        });
    });
    g.finish();
}

fn bench_roundtrip_stereo_16bit_44k1_format2_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm(n, 2, 16);
    let password = b"bench-r127";
    let mut g = c.benchmark_group("roundtrip_stereo_16bit_44k1_format2_1s");
    g.throughput(Throughput::Bytes((n * 2 * 2) as u64));
    g.sample_size(20);
    g.bench_function(
        BenchmarkId::from_parameter("stereo/16/44k1/format2/1s"),
        |b| {
            b.iter(|| {
                let tta = encode_with_password(criterion::black_box(&pcm), 2, 16, 44_100, password)
                    .expect("encode format=2");
                let (_info, samples) =
                    decode_with_password(&tta, password).expect("decode format=2");
                assert_eq!(samples.len(), pcm.len());
            });
        },
    );
    g.finish();
}

criterion_group!(
    benches,
    bench_roundtrip_mono_16bit_44k1_1s,
    bench_roundtrip_stereo_16bit_44k1_1s,
    bench_roundtrip_stereo_24bit_48k_500ms,
    bench_roundtrip_6ch_16bit_48k_250ms,
    bench_roundtrip_stereo_16bit_44k1_format2_1s,
);
criterion_main!(benches);
