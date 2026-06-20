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

// ---------------------------------------------------------------------
// N > 2 worked cascade examples (spec §4.1 / §4.2 / §4.3).
//
// The reference corpus contains only stereo tapes (§7.3), so for N > 2
// the spec's algebraic substitution is the ground truth. The spec
// works the encoder forward formula and the decoder inverse cascade for
// N=3, N=4, and the 6-channel 5.1 layout with exact intermediate
// values; these tests pin every published intermediate, not just the
// roundtrip endpoints.
// ---------------------------------------------------------------------

/// Spec §4.1 — N=3 with PCM `(A, B, C)`. The encoder produces:
/// `enc[0] = B-A`, `enc[1] = C-B`, `enc[2] = C - (C-B)/2`. The spec
/// works this verbatim; pin the exact encoder output, then confirm the
/// §4.2 inverse cascade walks it back to `(A, B, C)`.
#[test]
fn cascade_n3_forward_intermediates_match_spec_4_1() {
    // PCM (A,B,C) = (10, 25, 41); spec uses symbolic (A,B,C).
    let pcm = [10i32, 25, 41];
    let mut enc = pcm;
    forward(&mut enc);
    // enc[0] = B - A           = 15
    // enc[1] = C - B           = 16
    // enc[2] = C - (C-B)/2     = 41 - 8 = 33   (16/2 = 8, even)
    assert_eq!(enc, [15, 16, 33], "spec §4.1 N=3 encoder intermediates");
    // §4.2 inverse cascade: dec_out[2] = enc[2] + enc[1]/2;
    //   dec_out[1] = dec_out[2] - enc[1]; dec_out[0] = dec_out[1]-enc[0]
    inverse(&mut enc);
    assert_eq!(enc, pcm, "spec §4.2 inverse must restore (A,B,C)");
}

/// Spec §4.1 worked example — N=4 with PCM `(A, B, C, D)` produces
/// `(B-A, C-B, D-C, D - (D-C)/2)`. Pin every intermediate, then the
/// §4.2 step-by-step inverse substitution.
#[test]
fn cascade_n4_forward_intermediates_match_spec_4_1() {
    let pcm = [4i32, 9, 17, 30];
    let mut enc = pcm;
    forward(&mut enc);
    // enc[0]=B-A=5  enc[1]=C-B=8  enc[2]=D-C=13  enc[3]=D-(D-C)/2=30-6=24
    assert_eq!(enc, [5, 8, 13, 24], "spec §4.1 N=4 encoder intermediates");

    // §4.2 worked inverse substitution (re-derive each dec_out step):
    let (e0, e1, e2, e3) = (5i32, 8, 13, 24);
    let out3 = e3 + e2 / 2; // = 24 + 6 = 30 = D
    let out2 = out3 - e2; // = 30 - 13 = 17 = C
    let out1 = out2 - e1; // = 17 - 8 = 9 = B
    let out0 = out1 - e0; // = 9 - 5 = 4 = A
    assert_eq!([out0, out1, out2, out3], pcm);

    inverse(&mut enc);
    assert_eq!(enc, pcm, "spec §4.2 inverse must restore (A,B,C,D)");
}

/// Spec §4.3 — the 6-channel 5.1 worked walk. The spec lists the six
/// inverse steps explicitly for `nch = 6` (the cascade walks left across
/// the entire array with one anchor at index 5). Pin the full step
/// sequence against `inverse` and confirm there is no parity branch.
#[test]
fn cascade_n6_inverse_walk_matches_spec_4_3() {
    // Choose dec_in so the §4.3 six-step walk is non-trivial in every
    // lane (mix of signs, including an odd-negative at index 4 to keep
    // the anchor's `/2` on the discriminating path).
    let dec_in = [7i32, -3, 11, -5, -9, 40];
    // Spec §4.3 explicit walk:
    //   out[5] = in[5] + in[4]/2   = 40 + (-9/2 = -4) = 36
    //   out[4] = out[5] - in[4]    = 36 - (-9) = 45
    //   out[3] = out[4] - in[3]    = 45 - (-5) = 50
    //   out[2] = out[3] - in[2]    = 50 - 11   = 39
    //   out[1] = out[2] - in[1]    = 39 - (-3) = 42
    //   out[0] = out[1] - in[0]    = 42 - 7    = 35
    let mut buf = dec_in;
    inverse(&mut buf);
    assert_eq!(
        buf,
        [35, 42, 39, 50, 45, 36],
        "spec §4.3 six-step 5.1 cascade walk",
    );
    // -9/2 is the discriminator: `/2 = -4` (toward zero) vs `>>1 = -5`.
    // With `>>1` the anchor would be 35 and the whole walk would shift.
    assert_eq!(-9i32 / 2, -4);
    assert_eq!(-9i32 >> 1, -5);
}

/// Spec §4.3 anti-pattern #4 — odd channel counts have NO special case.
/// N=3 and N=5 must use the same uniform chain walk as even counts; a
/// parity-conditional path would corrupt them. We verify the forward
/// transform of a deliberately asymmetric input round-trips identically
/// at N=3 and N=5, and that the N=3 result is not some "mono-center +
/// stereo pair" alternative (which would differ from the chain).
#[test]
fn odd_channel_counts_have_no_special_case() {
    // N=3: a "mono center + L/R pair" misreading would treat ch1 as a
    // passthrough center; the real cascade chains all three. The chain
    // result is the forward()/inverse() pair below.
    let pcm3 = [100i32, -40, 70];
    let mut e3 = pcm3;
    forward(&mut e3);
    // Chain: e0=-140, e1=110, e2=70-(110/2)=70-55=15.
    assert_eq!(e3, [-140, 110, 15]);
    inverse(&mut e3);
    assert_eq!(e3, pcm3);

    // N=5: same uniform chain, no leftover handling.
    let pcm5 = [3i32, -7, 12, -1, 25];
    let mut e5 = pcm5;
    forward(&mut e5);
    inverse(&mut e5);
    assert_eq!(e5, pcm5, "N=5 uniform chain must round-trip");
}

/// Spec §9 anti-pattern #7 — the `nch == 1` branch must be explicit and
/// bounds-safe. A length-1 (or length-0) buffer must pass through the
/// identity without indexing `buffer[N-2] = buffer[-1]`.
#[test]
fn mono_and_empty_are_passthrough_and_bounds_safe() {
    let mut mono = [-12_345i32];
    inverse(&mut mono);
    assert_eq!(mono, [-12_345], "mono is identity");
    forward(&mut mono);
    assert_eq!(mono, [-12_345], "mono forward is identity");

    let mut empty: [i32; 0] = [];
    inverse(&mut empty); // must not panic
    forward(&mut empty); // must not panic
}

/// Exhaustive forward/inverse roundtrip over a dense, sign-balanced
/// input grid for every supported channel count `2..=6`. This drives
/// the cascade through thousands of odd-negative `/2` cases (the
/// truncation discriminator) per spec §6/§7.1, on the actual cascade
/// rather than a single hand-picked operand.
#[test]
fn forward_inverse_roundtrip_dense_grid_n2_to_n6() {
    // A small deterministic LCG, sign-balanced around zero.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        // 17-bit signed range straddling zero, biased to hit many
        // odd-negative values.
        ((state >> 24) as i32 & 0x1_FFFF) - 0x1_0000
    };
    for nch in 2usize..=6 {
        for _ in 0..4000 {
            let pcm: Vec<i32> = (0..nch).map(|_| next()).collect();
            let mut buf = pcm.clone();
            forward(&mut buf);
            inverse(&mut buf);
            assert_eq!(buf, pcm, "roundtrip failed at nch={nch} pcm={pcm:?}");
        }
    }
}

/// Spec §4.4 / §8.3 — the cascade carries NO state across samples. Two
/// independent sample slots fed the same `dec_in` must produce the same
/// `dec_out` regardless of what was decorrelated before them. This
/// guards anti-pattern #6 (carrying yesterday's `dec_out`).
#[test]
fn cascade_is_stateless_across_sample_slots() {
    let probe = [9_001i32, -1_234, 5_678, -90, 42, -7];
    let mut first = probe;
    inverse(&mut first);

    // Run an unrelated decorrelation in between, then re-run the probe.
    let mut noise = [-1i32, 2, -3, 4, -5, 6];
    inverse(&mut noise);

    let mut second = probe;
    inverse(&mut second);
    assert_eq!(first, second, "decorrelation must be per-sample stateless");
}

// ---------------------------------------------------------------------
// End-to-end decode-pipeline verification (spec §1).
//
// The tests above pin the isolated `inverse` / `forward` functions. The
// trace-tape tests below prove the *decode pipeline itself* runs exactly
// that cascade: they parse every per-sample DECORR_PRE / DECORR_POST /
// PCM_OUT event from a real codec-produced multichannel stream and
// assert (a) `inverse(raw_per_channel) == decorrelated_per_channel` for
// every sample slot, and (b) `final_per_channel == decorrelated_per_
// channel` (spec §1: PCM_OUT equals DECORR_POST for N>1). Because the
// raw values are produced by the full Rice + Stage-A + Stage-B pipeline
// on pseudo-noise content, they span the sign/parity matrix the §7.1
// stereo table samples — now exercised at N=3 and N=6.
// ---------------------------------------------------------------------

/// Parse `key=v0,v1,...` array field out of one trace line; returns the
/// signed values for the requested key, or `None` if absent.
#[cfg(feature = "trace")]
fn parse_arr(line: &str, key: &str) -> Option<Vec<i32>> {
    for field in line.split('\t') {
        if let Some(rest) = field.strip_prefix(key) {
            if let Some(vals) = rest.strip_prefix('=') {
                return Some(vals.split(',').map(|v| v.parse::<i32>().unwrap()).collect());
            }
        }
    }
    None
}

/// Drive a multichannel pseudo-noise stream through encode -> decode
/// with the trace tape on, then verify that every DECORR_PRE ->
/// DECORR_POST transition the live decoder emitted is reproduced by
/// `inverse`, and that PCM_OUT equals DECORR_POST (spec §1).
#[cfg(feature = "trace")]
fn assert_pipeline_decorr_matches_inverse(nch: u16, n_per_ch: usize, tag: &str) {
    use crate::{decode, encode};

    let tmp = std::env::temp_dir().join(format!("oxideav-tta-decorr-{tag}.tsv"));
    if tmp.exists() {
        std::fs::remove_file(&tmp).unwrap();
    }
    crate::trace::set_thread_trace_path(Some(tmp.clone()));

    // Pseudo-noise content per channel (uncorrelated draws) so the
    // per-sample raw buffer hits the full sign/parity matrix, like the
    // §7.1 noise tape but at this channel count.
    let mut state: u64 = 0xD1B5_4A32_D192_ED03 ^ (nch as u64);
    let total = n_per_ch * nch as usize;
    let samples: Vec<i32> = (0..total)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as i32 & 0x3FFF) - 0x2000
        })
        .collect();

    let tta = encode(&samples, nch, 16, 44_100).expect("encode");
    let (_info, decoded) = decode(&tta).expect("decode");
    crate::trace::set_thread_trace_path(None);
    assert_eq!(decoded, samples, "lossless roundtrip must hold for {tag}");

    let tape = std::fs::read_to_string(&tmp).expect("tape written");
    let mut pre_count = 0usize;
    let mut last_pre: Option<Vec<i32>> = None;
    for line in tape.lines() {
        if line.starts_with("ev=DECORR_PRE\t") {
            last_pre = parse_arr(line, "raw_per_channel");
            assert_eq!(
                last_pre.as_ref().unwrap().len(),
                nch as usize,
                "DECORR_PRE arity must equal nch"
            );
        } else if line.starts_with("ev=DECORR_POST\t") {
            let raw = last_pre
                .take()
                .expect("DECORR_POST without a preceding DECORR_PRE");
            let post = parse_arr(line, "decorrelated_per_channel").unwrap();
            let mut buf = raw.clone();
            inverse(&mut buf);
            assert_eq!(
                buf, post,
                "pipeline DECORR_POST must equal inverse(raw) at {tag}; raw={raw:?}"
            );
            pre_count += 1;
        }
    }
    assert_eq!(
        pre_count, n_per_ch,
        "one DECORR pair per PCM sample slot for {tag}"
    );

    // Spec §1: PCM_OUT.final_per_channel == DECORR_POST for N>1. Pair the
    // two event streams by sample_idx order (both are per-sample).
    let posts: Vec<Vec<i32>> = tape
        .lines()
        .filter(|l| l.starts_with("ev=DECORR_POST\t"))
        .map(|l| parse_arr(l, "decorrelated_per_channel").unwrap())
        .collect();
    let pcm_outs: Vec<Vec<i32>> = tape
        .lines()
        .filter(|l| l.starts_with("ev=PCM_OUT\t"))
        .map(|l| parse_arr(l, "final_per_channel").unwrap())
        .collect();
    assert_eq!(pcm_outs.len(), n_per_ch, "one PCM_OUT per sample slot");
    assert_eq!(
        posts, pcm_outs,
        "spec §1: PCM_OUT must equal DECORR_POST for N>1 at {tag}"
    );

    let _ = std::fs::remove_file(&tmp);
}

/// End-to-end: the stereo decode pipeline's per-sample cascade matches
/// `inverse` on real codec-produced data.
#[cfg(feature = "trace")]
#[test]
fn pipeline_decorr_matches_inverse_stereo() {
    assert_pipeline_decorr_matches_inverse(2, 300, "stereo");
}

/// End-to-end: the odd-N (N=3) decode pipeline's per-sample cascade
/// matches `inverse` — pinning that the §4.3 "no parity special case"
/// holds through the *full* decode path, not just the isolated function.
#[cfg(feature = "trace")]
#[test]
fn pipeline_decorr_matches_inverse_three_channel() {
    assert_pipeline_decorr_matches_inverse(3, 300, "3ch");
}

/// End-to-end: the 6-channel (5.1) decode pipeline's per-sample cascade
/// matches `inverse` over a noise stream spanning the sign/parity
/// matrix — the corpus's §7.3 N>2 gap, now closed against the live
/// decoder rather than only algebraic substitution.
#[cfg(feature = "trace")]
#[test]
fn pipeline_decorr_matches_inverse_six_channel() {
    assert_pipeline_decorr_matches_inverse(6, 300, "6ch");
}
