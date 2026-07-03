//! Bit-exactness regression pins for performance work.
//!
//! Round 386 (depth-mode bench+profile): every optimisation this round
//! (and any future perf round) must leave BOTH the encoder's output
//! bytes and the decoder's output samples exactly as they are today.
//! The roundtrip suite already proves `decode(encode(x)) == x`, but a
//! lockstep bug that changed the *wire bytes* symmetrically (e.g. a
//! Rice tracker tweak mirrored on both sides) would still round-trip.
//! These pins freeze the current wire format and PCM output as 64-bit
//! FNV-1a digests over a deterministic multi-frame corpus, so any
//! byte-level drift — encoder or decoder — fails loudly.
//!
//! The corpus is synthesised in-test (no fixtures): the same
//! xorshift32 tone-plus-noise generator the Criterion benches use,
//! at parameter points chosen to cover mono / stereo / odd-N / max-N
//! channel layouts, 16 / 17 / 20 / 24-bit depths (including the
//! packed non-multiple-of-8 widths sharing `byte_depth = 3`), and the
//! format=2 password path. Stream lengths exceed one frame
//! (`sample_rate * 256 / 245` samples) so per-frame state-reset
//! discipline, seek-table layout, and the short last frame are all
//! under the pin.

use oxideav_tta::{decode, decode_with_password, encode, encode_with_password};

/// 64-bit FNV-1a over a byte stream — self-contained so the pin does
/// not depend on the crate's (private) CRC32 internals.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Digest interleaved i32 samples via their little-endian byte image.
fn digest_samples(samples: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &s in samples {
        for b in s.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Deterministic tone-plus-noise PCM, identical in shape to the bench
/// corpus generator: low-frequency triangle envelope + per-sample
/// noise + per-channel DC bias, scaled to ~25% of the bit depth's
/// range.
fn build_pcm(n_samples: usize, channels: u16, bits_per_sample: u16) -> Vec<i32> {
    let nch = channels as usize;
    let mut out = Vec::with_capacity(n_samples * nch);
    let mut state: u32 = 0xCAFE_F00D;
    let amp: i32 = if bits_per_sample <= 16 {
        1 << 13
    } else {
        1 << (bits_per_sample - 3)
    };
    for s in 0..n_samples {
        let phase = (s % 256) as i32 - 128;
        let env = (phase / 128) * amp + (phase % 128) * (amp / 128);
        for ch in 0..nch {
            let noise = (xorshift32(&mut state) as i32) >> 24; // -128..127
            let chan_bias = (ch as i32) * (amp / 16);
            out.push(env + chan_bias + noise);
        }
    }
    out
}

/// One pinned parameter point: encode digest + decode digest.
struct Pin {
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    n_samples: usize,
    enc_digest: u64,
    pcm_digest: u64,
}

/// Format=1 pins. `n_samples = 50_000` at 44.1 kHz exceeds the
/// regular frame length (44100 * 256 / 245 = 46 080), so every stream
/// is 2 frames with a short last frame.
const PINS: &[Pin] = &[
    Pin {
        channels: 1,
        bits_per_sample: 16,
        sample_rate: 44_100,
        n_samples: 50_000,
        enc_digest: 0x2a04979ab098b97c,
        pcm_digest: 0xfa20ebf6abc66b80,
    },
    Pin {
        channels: 2,
        bits_per_sample: 16,
        sample_rate: 44_100,
        n_samples: 50_000,
        enc_digest: 0xea655db122ac2dde,
        pcm_digest: 0xf44c2690e780903d,
    },
    Pin {
        channels: 2,
        bits_per_sample: 17,
        sample_rate: 44_100,
        n_samples: 50_000,
        enc_digest: 0x145ec0fe9aec07c7,
        pcm_digest: 0x4c2c52bdb6690927,
    },
    Pin {
        channels: 2,
        bits_per_sample: 24,
        sample_rate: 48_000,
        n_samples: 52_000,
        enc_digest: 0x4d2482b370b81d8b,
        pcm_digest: 0xa01342bd330388e7,
    },
    Pin {
        channels: 3,
        bits_per_sample: 20,
        sample_rate: 48_000,
        n_samples: 52_000,
        enc_digest: 0x809c42a5210fad32,
        pcm_digest: 0x8fcb888490c4dfb9,
    },
    Pin {
        channels: 6,
        bits_per_sample: 16,
        sample_rate: 48_000,
        n_samples: 52_000,
        enc_digest: 0xb70588252447c61c,
        pcm_digest: 0xd2f2cd420a2e1dce,
    },
];

#[test]
fn format1_wire_and_pcm_digests_are_pinned() {
    for pin in PINS {
        let pcm = build_pcm(pin.n_samples, pin.channels, pin.bits_per_sample);
        let tta = encode(&pcm, pin.channels, pin.bits_per_sample, pin.sample_rate)
            .expect("encode must succeed");
        assert_eq!(
            fnv1a64(&tta),
            pin.enc_digest,
            "encoder wire bytes drifted at ch={} bps={} — a perf change \
             altered the on-disk stream",
            pin.channels,
            pin.bits_per_sample,
        );
        let (info, samples) = decode(&tta).expect("decode must succeed");
        assert_eq!(info.channels, pin.channels);
        assert_eq!(info.bits_per_sample, pin.bits_per_sample);
        assert_eq!(samples, pcm, "lossless roundtrip must hold");
        assert_eq!(
            digest_samples(&samples),
            pin.pcm_digest,
            "decoded PCM drifted at ch={} bps={}",
            pin.channels,
            pin.bits_per_sample,
        );
    }
}

/// Format=2 pin — the password-derived qm priming path (`spec/07`
/// §3.5) must also stay wire-stable.
#[test]
fn format2_wire_and_pcm_digests_are_pinned() {
    const ENC_DIGEST: u64 = 0xfb0a5e29a3372787;
    const PCM_DIGEST: u64 = 0xf44c2690e780903d;
    let pcm = build_pcm(50_000, 2, 16);
    let password = b"pin-r386";
    let tta = encode_with_password(&pcm, 2, 16, 44_100, password).expect("encode format=2");
    assert_eq!(
        fnv1a64(&tta),
        ENC_DIGEST,
        "format=2 encoder wire bytes drifted"
    );
    let (info, samples) = decode_with_password(&tta, password).expect("decode format=2");
    assert_eq!(info.format, 2);
    assert_eq!(samples, pcm, "lossless roundtrip must hold");
    assert_eq!(
        digest_samples(&samples),
        PCM_DIGEST,
        "format=2 decoded PCM drifted"
    );
}
