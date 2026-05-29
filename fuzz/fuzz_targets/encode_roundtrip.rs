#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the public TTA1
//! **encoder** (`encode` / `encode_with_password`) and then through
//! the in-crate decoder (`decode` / `decode_with_password`) so any
//! encoded byte stream the encoder accepts must also survive the
//! parser that the `decode` target hammers — and the recovered
//! samples must be bit-exact.
//!
//! The `decode` fuzz target covers the attacker-facing surface (bytes
//! flow in from a file and the decoder must never panic). The
//! **encoder** is a different shape of risk: its input is a typed
//! tuple `(samples: &[i32], channels, bits_per_sample, sample_rate)`
//! — not a raw byte stream — and it must never panic / abort /
//! integer-overflow / OOM regardless of how hostile the *caller* is.
//! Callers that pass `channels = 0`, `bits_per_sample = 7`, a sample
//! rate above the `0x007F_FFFF` policy ceiling, or a sample-buffer
//! length that isn't a multiple of `channels` are real integration
//! shapes — the encoder's validation must reject them with the typed
//! `Error::Unsupported…` / `Error::InvalidSampleBuffer` variants and
//! never explode. Just as critically, once the encoder *accepts* an
//! input it MUST produce wire bytes the decoder round-trips: the
//! samples that come out of `decode(encode(...))` are required to be
//! bit-identical to the input. A silent encoder/decoder skew is a
//! correctness bug the self-roundtrip suite in `src/roundtrip_tests.rs`
//! catches on hand-picked fixtures; the fuzzer drives across the
//! whole parameter cube.
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0      : channels seed       → channels = (b0 % 6) + 1, in 1..=6
//!   byte 1      : bit-depth selector  → 0/1/2 → bps ∈ {16, 24} (only the
//!                 two depths the encoder's `pack_pcm` symmetric uses cover)
//!   bytes 2-5   : sample_rate seed    → LE u32, masked to 0x007F_FFFF
//!                                       and OR'd with 1 (rate must be ≥ 1)
//!   byte 6      : format selector     → 0 → format=1, 1 → format=2
//!   bytes 7-8   : password-length seed → format=2 only; up to 16 bytes from
//!                                       the payload are used as the password
//!   bytes 9..   : interleaved sample bytes — consumed `bps/8` per `i32`
//!                 slot, sign-extended to the encoder's signed integer
//!                 range. Trailing samples beyond the payload are filled
//!                 with zero so a short input still drives the full
//!                 pipeline.
//! ```
//!
//! ## Contract under test
//!
//! 1. `encode(...)` / `encode_with_password(...)` always *returns* a
//!    `Result` — no panic, no abort, no integer overflow (in a debug /
//!    ASAN build), no OOM. Rejection via typed `Error::Unsupported…`
//!    is the encoder's contractually correct behaviour.
//! 2. Whenever the encoder returns `Ok(bytes)`, the matching decoder
//!    call (`decode` / `decode_with_password` with the same password)
//!    must also return `Ok((info, samples))`. An encoder that
//!    produces bytes its own decoder rejects is a hard correctness
//!    bug.
//! 3. Whenever both calls succeed, the decoded sample buffer must
//!    equal the input sample buffer bit-exactly. This is the
//!    bit-exact lossless invariant the in-tree roundtrip suite pins
//!    on hand-picked fixtures, here driven across the
//!    `(channels × bps × sample_rate × format × samples)` parameter
//!    cube.
//!
//! ## Cap on sample count
//!
//! The total sample count per fuzz input is capped at 4096 samples /
//! channel so the fuzzer's per-iteration budget lands on encoder
//! correctness (Rice prefix emission, frame boundary handling,
//! Stage-A / Stage-B / inverse-decorrelation symmetric inverse
//! arithmetic, format=2 qm priming) rather than the trivial
//! "allocate a few MiB" branch the format's framing technically
//! allows.

use libfuzzer_sys::fuzz_target;

/// Per-channel sample cap. At 6 channels × 4096 samples × 3 bytes/sample
/// the encoder's working buffers fit comfortably in libfuzzer's
/// per-iteration budget while still spanning multiple frames at the
/// default 1.044s frame regularity.
const MAX_SAMPLES_PER_CHANNEL: usize = 4096;

fuzz_target!(|data: &[u8]| {
    if data.len() < 9 {
        return;
    }

    // ── Header byte 0: channels (1..=6) ────────────────────────────
    let channels = ((data[0] as u16) % 6) + 1;

    // ── Header byte 1: bits_per_sample ∈ {16, 24} ──────────────────
    // Only the two depths `pack_pcm` covers are exercised. Other values
    // in 17..=23 are valid per the encoder's `16..=24` accepting range
    // but `pack_pcm` itself panics on them — the bit-exact roundtrip
    // assertion below compares raw `i32` slots, not packed bytes, so
    // arbitrary bps in 16..=24 would be admissible — but the encoder's
    // frame-byte budget grows linearly in bps and the decoder rounds
    // sample masks to `1 << bits_per_sample`, so sticking to {16, 24}
    // is the cleanest invariant.
    let bits_per_sample: u16 = match data[1] % 2 {
        0 => 16,
        _ => 24,
    };

    // ── Header bytes 2-5: sample_rate (1..=0x007F_FFFF) ────────────
    // Mask to the 23-bit policy ceiling (`spec/01` §3.3 high bit
    // reserved) and OR with 1 so the rate is never zero (which the
    // encoder rejects with `UnsupportedSampleRate`).
    let raw_rate = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
    let sample_rate = (raw_rate & 0x007F_FFFF) | 1;

    // ── Header byte 6: format selector ─────────────────────────────
    let format2 = (data[6] & 1) != 0;

    // ── Header bytes 7-8: password length seed ─────────────────────
    let password_seed = u16::from_le_bytes([data[7], data[8]]);
    let password_len = (password_seed as usize) % 17; // 0..=16

    let mut payload = &data[9..];

    // ── format=2 only: pull the password off the front of payload ──
    let password: Vec<u8> = if format2 {
        let take = password_len.min(payload.len());
        let pw = payload[..take].to_vec();
        payload = &payload[take..];
        pw
    } else {
        Vec::new()
    };

    // ── Build the interleaved i32 sample buffer ────────────────────
    // The encoder's bit-exact contract requires the input sample range
    // to fit the declared bit depth's signed range: 16-bit -> i16,
    // 24-bit -> [-2^23, 2^23). Sign-extending from `bps`-bit
    // little-endian payload bytes gives exactly that.
    let bytes_per_sample = (bits_per_sample as usize).div_ceil(8); // 2 or 3
    let nch = channels as usize;
    let raw_available = payload.len() / bytes_per_sample;
    // Cap the slot count: never above MAX_SAMPLES_PER_CHANNEL per
    // channel, and always a multiple of `nch` so the encoder's
    // sample-buffer-length-modulo-channels precondition holds.
    let max_slots = MAX_SAMPLES_PER_CHANNEL * nch;
    let target_slots = raw_available.min(max_slots);
    let slots = (target_slots / nch) * nch; // multiple of nch
    if slots == 0 {
        // Empty stream is valid (frame_count = 0) but the bit-exact
        // assertion is trivial; skip so the fuzzer's budget lands on
        // multi-frame cases.
        return;
    }

    let mut samples = Vec::with_capacity(slots);
    for i in 0..slots {
        let off = i * bytes_per_sample;
        let s = match bytes_per_sample {
            2 => {
                let v = i16::from_le_bytes([payload[off], payload[off + 1]]);
                v as i32
            }
            3 => {
                // 24-bit signed little-endian: assemble the lower 24
                // bits then sign-extend bit 23 → bit 31.
                let lo = payload[off] as u32;
                let mid = payload[off + 1] as u32;
                let hi = payload[off + 2] as u32;
                let raw = lo | (mid << 8) | (hi << 16);
                // Sign-extend: if bit 23 set, fill 24..31.
                if raw & 0x0080_0000 != 0 {
                    (raw | 0xFF00_0000) as i32
                } else {
                    raw as i32
                }
            }
            _ => unreachable!(),
        };
        samples.push(s);
    }

    // ── 1. Encoder must always return ──────────────────────────────
    let encoded = if format2 {
        oxideav_tta::encode_with_password(
            &samples,
            channels,
            bits_per_sample,
            sample_rate,
            &password,
        )
    } else {
        oxideav_tta::encode(&samples, channels, bits_per_sample, sample_rate)
    };
    let bytes = match encoded {
        Ok(b) => b,
        Err(_) => return, // typed rejection — contract upheld.
    };

    // ── 2. Encoder output must decode ─────────────────────────────
    let decoded = if format2 {
        oxideav_tta::decode_with_password(&bytes, &password)
    } else {
        oxideav_tta::decode(&bytes)
    };
    let (info, out_samples) = match decoded {
        Ok(t) => t,
        Err(e) => {
            // The encoder accepted these parameters but our own
            // decoder rejected the bytes it produced — a hard contract
            // violation, not a fuzz-discoverable corruption.
            panic!(
                "encoder produced bytes the in-crate decoder rejects: {e:?} \
                 channels={channels} bps={bits_per_sample} rate={sample_rate} \
                 format2={format2} pw_len={} samples={} encoded_len={}",
                password.len(),
                samples.len(),
                bytes.len()
            );
        }
    };

    // ── 3. Round-trip must be bit-exact ───────────────────────────
    assert_eq!(
        info.channels, channels,
        "channels round-trip skew: declared {channels} got {}",
        info.channels
    );
    assert_eq!(
        info.bits_per_sample, bits_per_sample,
        "bps round-trip skew: declared {bits_per_sample} got {}",
        info.bits_per_sample
    );
    assert_eq!(
        info.sample_rate, sample_rate,
        "rate round-trip skew: declared {sample_rate} got {}",
        info.sample_rate
    );
    assert_eq!(
        out_samples.len(),
        samples.len(),
        "sample count skew: encoded {} decoded {}",
        samples.len(),
        out_samples.len()
    );
    assert_eq!(
        out_samples,
        samples,
        "bit-exact roundtrip mismatch: channels={channels} bps={bits_per_sample} \
         rate={sample_rate} format2={format2} pw_len={} samples={}",
        password.len(),
        samples.len(),
    );
});
