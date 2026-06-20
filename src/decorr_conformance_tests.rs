//! Spec-anchored conformance suite for the channel-decorrelation stage
//! (`spec/04-decorrelation.md`).
//!
//! The unit tests in `decorr.rs` cover the cascade's structure and two
//! representative rows; this module pins the *reference-tape ground
//! truth* the spec records, so the decorrelation transform is asserted
//! bit-for-bit against externally-captured values rather than only
//! against its own algebraic inverse.
//!
//! Three distinct ground-truth sources are exercised:
//!
//! 1. **§7.1 — the 31-row pseudo-noise reference-tape table.** Each row
//!    is an observed `(raw_per_channel) -> (decorrelated_per_channel)`
//!    pair from the stereo noise tape. The noise fixture is the most
//!    discriminating in the corpus because its two channels are
//!    uncorrelated, so `dec_in[0]` spans the full sign / parity matrix
//!    (§7.1). These are *captured* values, not self-derived — pinning
//!    them is the strongest available statement of stereo correctness
//!    short of re-running the capture harness.
//!
//! 2. **§4.1 / §4.2 / §4.3 — the N>2 worked cascade examples.** The
//!    spec walks the encoder forward formula and the decoder inverse
//!    cascade for N=3, N=4, and the 6-channel 5.1 layout, giving exact
//!    intermediate values. These pin the >2-channel cascade directly
//!    (the corpus has only stereo tapes, so the spec's algebraic
//!    substitution is the ground truth for N>2 per §7.3).
//!
//! 3. **§6 — the truncating-divide sign-discipline matrix.** The half
//!    step uses C signed division toward zero, NOT arithmetic right
//!    shift; the two diverge by 1 LSB on odd-negative dividends. The
//!    §6 table enumerates six operands; this module asserts the cascade
//!    realizes `/2` and would visibly fail under `>>1`.

use crate::decorr::{forward, inverse};

/// Spec §6 truncating-divide table — `x / 2` (toward zero) for the six
/// operands the spec enumerates. The arithmetic-shift column is shown
/// in the spec only as the *wrong* answer; we assert Rust's `/` matches
/// the truncating column and diverges from `>>` exactly where the spec
/// says it must.
#[test]
fn sign_discipline_div2_matches_spec_table() {
    // (operand, truncating `/2`, flooring `>>1`)  — spec §6 table.
    let rows: [(i32, i32, i32); 6] = [
        (14_238, 7_119, 7_119),
        (14_239, 7_119, 7_119),
        (-14_238, -7_119, -7_119),
        (-14_239, -7_119, -7_120),
        (-22_783, -11_391, -11_392),
        (-8_367, -4_183, -4_184),
    ];
    for (x, trunc, shift) in rows {
        assert_eq!(x / 2, trunc, "Rust `/` must truncate toward zero for {x}");
        assert_eq!(x >> 1, shift, "Rust `>>` floors for {x} (the WRONG op)");
        // The two only agree on even and odd-positive operands; on
        // odd-negative operands they differ by exactly 1 LSB.
        if x < 0 && x % 2 != 0 {
            assert_eq!(trunc - shift, 1, "odd-negative {x} must diverge by 1 LSB");
        } else {
            assert_eq!(trunc, shift, "{x} must agree between `/2` and `>>1`");
        }
    }
}

/// Spec §7.1 reference-tape rows for the stereo pseudo-noise fixture
/// `noise-pseudo-2ch-16bit-44100-0.5s`. Each tuple is
/// `(dec_in[0], dec_in[1], expected_out0, expected_out1)` where the
/// expected pair is the tape's `decorrelated_per_channel` written in
/// PCM-interleave order (channel 0 first). All 31 rows (sample_idx
/// 0..30) the spec tabulates are present and asserted exactly.
const NOISE_TAPE_ROWS: &[(i32, i32, i32, i32)] = &[
    (-11_124, -5_429, 133, -10_991),
    (14_239, -3_787, -10_907, 3_332),
    (-110, -12_540, -12_485, -12_595),
    (-22_783, -3_670, 7_722, -15_061),
    (-14_685, 6_873, 14_216, -469),
    (1_239, 3_126, 2_506, 3_745),
    (-12_231, 4_814, 10_930, -1_301),
    (2_857, -5_046, -6_475, -3_618),
    (19_515, 867, -8_891, 10_624),
    (-6_776, -8_898, -5_510, -12_286),
    (5_462, -6_421, -9_152, -3_690),
    (-8_367, 8_711, 12_895, 4_528),
    (2_386, -7_757, -8_950, -6_564),
    (-15_037, -6_365, 1_154, -13_883),
    (7_438, 3_309, -410, 7_028),
    (18_869, -3_248, -12_683, 6_186),
    (-17_255, -1_740, 6_888, -10_367),
    (4_832, -3_216, -5_632, -800),
    (-13_012, 2_822, 9_328, -3_684),
    (-6_378, -754, 2_435, -3_943),
    (6_526, 9_375, 6_112, 12_638),
    (-19_046, -5_143, 4_380, -14_666),
    (-14_118, 6_123, 13_182, -936),
    (14_461, 1_431, -5_800, 8_661),
    (5_814, 4_124, 1_217, 7_031),
    (-3_454, 9_407, 11_134, 7_680),
    (11_601, 716, -5_085, 6_516),
    (3_614, 3_953, 2_146, 5_760),
    (3_648, 10_655, 8_831, 12_479),
    (-7_909, 741, 4_696, -3_213),
    (-12_302, 7_652, 13_803, 1_501),
];

/// The `inverse` cascade must reproduce every §7.1 tape row exactly.
/// This is the captured-ground-truth assertion (not a self-roundtrip):
/// the 31 `decorrelated_per_channel` pairs came from the instrumented
/// reference tape, so matching them pins the stereo decoder transform
/// against an external witness.
#[test]
fn inverse_reproduces_all_noise_tape_rows() {
    for &(in0, in1, out0, out1) in NOISE_TAPE_ROWS {
        let mut buf = [in0, in1];
        inverse(&mut buf);
        assert_eq!(
            buf,
            [out0, out1],
            "spec §7.1 row dec_in=({in0},{in1}) must decorrelate to ({out0},{out1})",
        );
    }
}

/// Every §7.1 row is also a valid encoder output (the tape's
/// `raw_per_channel` is exactly what the encoder must have produced for
/// the original PCM). So `forward(inverse(row))` is the identity, and
/// re-encoding the recovered PCM must reproduce the original raw pair —
/// a closed loop anchored on captured values rather than synthetic
/// input.
#[test]
fn forward_inverse_closes_on_noise_tape_rows() {
    for &(in0, in1, _out0, _out1) in NOISE_TAPE_ROWS {
        let raw = [in0, in1];
        let mut pcm = raw;
        inverse(&mut pcm); // raw -> PCM
        forward(&mut pcm); // PCM -> raw
        assert_eq!(pcm, raw, "forward(inverse(raw)) must restore raw {raw:?}");
    }
}

/// Spec §7.1 spot-check on the truncation discriminator. Row 11 has
/// `dec_in[0] = -8367` (odd-negative). The spec shows the tape matches
/// `/2 = -4183`; an arithmetic `>>1 = -4184` would yield `(12894,
/// 4527)` — off by 1 in both lanes. We assert the correct pair lands
/// and the wrong pair does not.
#[test]
fn noise_tape_row_11_pins_truncating_divide() {
    let mut buf = [-8_367i32, 8_711];
    inverse(&mut buf);
    assert_eq!(buf, [12_895, 4_528], "must use `/2`, not `>>1`");

    // What `>>1` would have produced — explicitly NOT equal.
    let wrong_half = -8_367i32 >> 1; // -4184
    let wrong_out1 = 8_711 + wrong_half; // 4527
    let wrong_out0 = wrong_out1 - (-8_367); // 12894
    assert_ne!([wrong_out0, wrong_out1], [12_895, 4_528]);
}
