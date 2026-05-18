//! Inverse channel decorrelation per `spec/04-decorrelation.md`.
//!
//! For `nch == 1` this is the identity. For `nch >= 2`, the cascade
//! walks from the highest channel index downward:
//!
//! ```text
//! dec_out[N-1] = dec_in[N-1] + dec_in[N-2] / 2
//! for i = N-2 down to 0:
//!     dec_out[i] = dec_out[i+1] - dec_in[i]
//! ```
//!
//! The `/2` is C signed truncating division (toward zero) — NOT
//! arithmetic right shift. See spec §6 for the sign-discipline
//! discussion.

/// Apply the inverse decorrelation cascade in place.
pub fn inverse(buffer: &mut [i32]) {
    let n = buffer.len();
    if n < 2 {
        return;
    }
    // Step 1: anchor the highest channel using the still-pre value of
    // buffer[N-2].
    let dec_in_n_minus_2 = buffer[n - 2];
    // Truncating-toward-zero division per spec §6 — Rust `/` on signed
    // integers matches C's signed `/`.
    buffer[n - 1] = buffer[n - 1].wrapping_add(dec_in_n_minus_2 / 2);
    // Step 2: walk down. For each i from N-2 down to 0, snapshot the
    // pre value before overwriting.
    for i in (0..(n - 1)).rev() {
        let dec_in_i = buffer[i];
        buffer[i] = buffer[i + 1].wrapping_sub(dec_in_i);
    }
}

/// Encoder-side forward decorrelation per `spec/04` §3.1.
///
/// Symmetric inverse of [`inverse`]: walks the channel buffer from
/// low to high index forming successive differences, then anchors
/// the highest channel with the truncating `/2` of the last
/// difference. The composition `forward → inverse` is the identity
/// over the supported `i32` range.
pub fn forward(pcm: &mut [i32]) {
    let n = pcm.len();
    if n < 2 {
        return;
    }
    let mut last_delta: i32 = 0;
    for i in 0..(n - 1) {
        let delta = pcm[i + 1].wrapping_sub(pcm[i]);
        pcm[i] = delta;
        last_delta = delta;
    }
    pcm[n - 1] = pcm[n - 1].wrapping_sub(last_delta / 2);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mono is the identity.
    #[test]
    fn nch_1_passthrough() {
        let mut buf = [42i32];
        inverse(&mut buf);
        assert_eq!(buf, [42]);
    }

    /// Forward + inverse must roundtrip on every channel count we
    /// support (2..=6) for arbitrary inputs, including odd-negative
    /// dividends (the truncating-vs-flooring discriminator).
    #[test]
    fn forward_inverse_roundtrip_random() {
        let inputs: Vec<Vec<i32>> = vec![
            vec![100, 200],
            vec![-11_124, -5_429], // spec §7.1 sample 0
            vec![-8_367, 8_711],   // spec §7.1 sample 11 (odd-negative)
            vec![1, 2, 3],
            vec![-3, -2, -1],
            vec![1, 2, 3, 4, 5, 6],
            vec![100, -100, 50, -50, 25, -25],
        ];
        for input in inputs {
            let mut buf = input.clone();
            forward(&mut buf);
            inverse(&mut buf);
            assert_eq!(buf, input);
        }
    }

    /// Spec §7.1 — stereo `(dec_in[0]=-8367, dec_in[1]=8711)` →
    /// `(12895, 4528)` with `/2 = -4183` (truncating). Arithmetic
    /// shift would give `-4184` and produce `(12894, 4527)`.
    #[test]
    fn stereo_truncating_divide_discriminator() {
        let mut buf = [-8_367i32, 8_711];
        inverse(&mut buf);
        assert_eq!(buf, [12_895, 4_528]);
    }

    /// Spec §4.3 — N=4 with PCM (A,B,C,D) round-trips.
    #[test]
    fn cascade_inverse_n_4_walk() {
        let pcm = [10i32, 20, 35, 50];
        let mut enc = pcm;
        forward(&mut enc);
        // Encoder formula:
        //   enc[0] = B - A = 10
        //   enc[1] = C - B = 15
        //   enc[2] = D - C = 15
        //   enc[3] = D - (D-C)/2 = 50 - 7 = 43
        assert_eq!(enc, [10, 15, 15, 43]);
        inverse(&mut enc);
        assert_eq!(enc, pcm);
    }
}
