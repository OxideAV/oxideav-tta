//! Profiling driver + bit-identity harness for the encode hot path.
//!
//! Round 386 (depth mode: bench+profile). Mirror of
//! `profile_decode.rs` for the encoder: synthesises the same
//! deterministic xorshift corpus the criterion benches use, then
//! encodes every scenario `--iters` times (default 40) in a tight
//! sequential loop. Prints, per scenario, an FNV-1a 64-bit hash over
//! the encoded TTA1 byte stream plus the accumulated wall-clock
//! encode time.
//!
//! Two jobs:
//!
//! 1. **Profiling target** — run under a sampling profiler
//!    (`sample <pid>` / Time Profiler) to rank encode hotspots:
//!    `CARGO_PROFILE_RELEASE_DEBUG=true cargo run --release
//!    --example profile_encode`.
//! 2. **Bit-identity oracle** — the printed hashes must be identical
//!    before and after any optimisation commit; the corpus covers
//!    mono/stereo/6ch, 16/24-bit, and format=2 password priming.
//!
//! No `docs/` fixtures or external files are read.

use std::time::Instant;

use oxideav_tta::{encode, encode_with_password};

/// Cheap deterministic xorshift32 — same generator as the criterion
/// benches so the corpus is reproducible across runs and machines.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Tone-plus-noise interleaved PCM, identical construction to
/// `benches/encode.rs` (`build_pcm`).
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

/// FNV-1a 64-bit over a raw byte slice.
fn fnv1a_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

struct Scenario {
    name: &'static str,
    pcm: Vec<i32>,
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    password: Option<&'static [u8]>,
}

fn main() {
    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(40);

    let scenarios: Vec<Scenario> = vec![
        Scenario {
            name: "mono_16bit_44k1_1s",
            pcm: build_pcm(44_100, 1, 16),
            channels: 1,
            bits_per_sample: 16,
            sample_rate: 44_100,
            password: None,
        },
        Scenario {
            name: "stereo_16bit_44k1_1s",
            pcm: build_pcm(44_100, 2, 16),
            channels: 2,
            bits_per_sample: 16,
            sample_rate: 44_100,
            password: None,
        },
        Scenario {
            name: "stereo_24bit_48k_500ms",
            pcm: build_pcm(24_000, 2, 24),
            channels: 2,
            bits_per_sample: 24,
            sample_rate: 48_000,
            password: None,
        },
        Scenario {
            name: "6ch_16bit_48k_250ms",
            pcm: build_pcm(12_000, 6, 16),
            channels: 6,
            bits_per_sample: 16,
            sample_rate: 48_000,
            password: None,
        },
        Scenario {
            name: "stereo_16bit_44k1_format2_1s",
            pcm: build_pcm(44_100, 2, 16),
            channels: 2,
            bits_per_sample: 16,
            sample_rate: 44_100,
            password: Some(b"hunter2"),
        },
    ];

    let mut total = std::time::Duration::ZERO;
    for sc in &scenarios {
        let mut enc_hash: u64 = 0;
        let start = Instant::now();
        for _ in 0..iters {
            let bytes = match sc.password {
                Some(pw) => encode_with_password(
                    &sc.pcm,
                    sc.channels,
                    sc.bits_per_sample,
                    sc.sample_rate,
                    pw,
                )
                .expect("encode_with_password"),
                None => encode(&sc.pcm, sc.channels, sc.bits_per_sample, sc.sample_rate)
                    .expect("encode"),
            };
            enc_hash = fnv1a_bytes(&bytes);
        }
        let elapsed = start.elapsed();
        total += elapsed;
        println!(
            "{:<30} iters={} enc_hash={:016x} elapsed={:?} per_iter={:?}",
            sc.name,
            iters,
            enc_hash,
            elapsed,
            elapsed / iters as u32
        );
    }
    println!("TOTAL encode wall time: {total:?}");
}
