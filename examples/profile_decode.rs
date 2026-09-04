//! Profiling driver + bit-identity harness for the decode hot path.
//!
//! Round 285 (depth mode: profile-opt). Synthesises the same
//! deterministic xorshift corpus the criterion benches use, encodes
//! each scenario once with the production `encode` /
//! `encode_with_password`, then decodes every stream `--iters` times
//! (default 40) in a tight sequential loop. Prints, per scenario, an
//! FNV-1a 64-bit hash over the decoded interleaved `i32` PCM (little-
//! endian byte order) plus the accumulated wall-clock decode time.
//!
//! Two jobs:
//!
//! 1. **Profiling target** — run under a sampling profiler
//!    (`sample <pid>` / Time Profiler) to rank decode hotspots:
//!    `CARGO_PROFILE_RELEASE_DEBUG=true cargo run --release
//!    --example profile_decode`.
//! 2. **Bit-identity oracle** — the printed hashes must be identical
//!    before and after any optimisation commit; the corpus covers
//!    mono/stereo/6ch, 16/24-bit, and format=2 password priming.
//!
//! No `docs/` fixtures or external files are read.

use std::time::Instant;

use oxideav_tta::{decode, decode_with_password, encode, encode_with_password};

/// Cheap deterministic xorshift32 — same generator as the criterion
/// benches so the corpus is reproducible across runs and machines.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Tone-plus-noise interleaved PCM, identical construction to
/// `benches/decode.rs` (`build_pcm`).
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

/// FNV-1a 64-bit over the decoded samples in little-endian byte order.
fn fnv1a(samples: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &s in samples {
        for b in s.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

struct Scenario {
    name: &'static str,
    bytes: Vec<u8>,
    password: Option<&'static [u8]>,
    /// PCM byte depth (2 or 3) — for the MiB/s figure over the
    /// decoded PCM footprint (same convention as the Criterion
    /// `Throughput::Bytes` cells).
    pcm_byte_depth: usize,
}

fn main() {
    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(40);

    let scenarios: Vec<Scenario> = vec![
        Scenario {
            name: "mono_16bit_44k1_1s",
            bytes: encode(&build_pcm(44_100, 1, 16), 1, 16, 44_100).expect("encode"),
            password: None,
            pcm_byte_depth: 2,
        },
        Scenario {
            name: "stereo_16bit_44k1_1s",
            bytes: encode(&build_pcm(44_100, 2, 16), 2, 16, 44_100).expect("encode"),
            password: None,
            pcm_byte_depth: 2,
        },
        Scenario {
            name: "stereo_24bit_48k_500ms",
            bytes: encode(&build_pcm(24_000, 2, 24), 2, 24, 48_000).expect("encode"),
            password: None,
            pcm_byte_depth: 3,
        },
        Scenario {
            name: "6ch_16bit_48k_250ms",
            bytes: encode(&build_pcm(12_000, 6, 16), 6, 16, 48_000).expect("encode"),
            password: None,
            pcm_byte_depth: 2,
        },
        Scenario {
            name: "stereo_16bit_44k1_format2_1s",
            bytes: encode_with_password(&build_pcm(44_100, 2, 16), 2, 16, 44_100, b"hunter2")
                .expect("encode_with_password"),
            password: Some(b"hunter2"),
            pcm_byte_depth: 2,
        },
    ];

    let mut total = std::time::Duration::ZERO;
    for sc in &scenarios {
        // Hash of the encoded stream itself: locks the ENCODER's
        // bit-identity across optimisation commits too (the encoder
        // shares the Stage-A LMS step with the decoder).
        let enc_hash = fnv1a_bytes(&sc.bytes);
        // Round 456: time every iteration individually and report the
        // minimum and median alongside the total. On a shared host a
        // single preemption inflates the 150-iteration total by far
        // more than any optimisation moves it, whereas the per-
        // iteration minimum is a stable estimate of the true cost. The
        // PCM digest is taken once, outside the timed region — the
        // byte-serial FNV chain used to be ~30% of the sampled time
        // and was diluting every measured delta.
        let mut per_iter: Vec<std::time::Duration> = Vec::with_capacity(iters);
        let mut last: Option<Vec<i32>> = None;
        for _ in 0..iters {
            let start = Instant::now();
            let (_info, pcm) = match sc.password {
                Some(pw) => decode_with_password(&sc.bytes, pw).expect("decode"),
                None => decode(&sc.bytes).expect("decode"),
            };
            per_iter.push(start.elapsed());
            last = Some(pcm);
        }
        let hash = fnv1a(last.as_deref().unwrap_or(&[]));
        let elapsed: std::time::Duration = per_iter.iter().sum();
        total += elapsed;
        per_iter.sort();
        let min = per_iter[0];
        let median = per_iter[per_iter.len() / 2];
        let pcm_bytes = last.as_ref().map_or(0, |p| p.len()) as f64 * sc.pcm_byte_depth as f64;
        let mib_s = pcm_bytes / (1024.0 * 1024.0) / min.as_secs_f64();
        println!(
            "{:<30} iters={} enc_hash={:016x} pcm_hash={:016x} elapsed={:?} per_iter={:?} min={:?} median={:?} min_mib_s={:.1}",
            sc.name,
            iters,
            enc_hash,
            hash,
            elapsed,
            elapsed / iters as u32,
            min,
            median,
            mib_s,
        );
    }
    println!("TOTAL decode wall time: {total:?}");
}
