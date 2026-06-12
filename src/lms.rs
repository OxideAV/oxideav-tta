//! Stage-A 8-tap sign-LMS adaptive predictor per `spec/02-stage-a-lms.md`.
//!
//! Per-channel state `(dl[8], dx[8], qm[8], error)` is reset to all
//! zeros at every frame boundary; `(shift, round)` are loaded from the
//! per-bps `LMS_SHIFT` table at frame init and stay constant for the
//! rest of the frame.
//!
//! The five-step update procedure of spec §4.2 is followed exactly:
//!
//! 1. Sign-LMS qm-update gated on the previous step's residual.
//! 2. Dot-product prediction with rounding bias and arithmetic right
//!    shift.
//! 3. Head→tail shift of `dx[0..3]` and `dl[0..3]`.
//! 4. Regenerate `dx[4..7]` from the sign of the pre-update
//!    `dl[4..7]` (zero maps to the positive branch).
//! 5. Save the residual into `error` for the next step, form the
//!    Stage-A output, and overwrite `dl[4..7]` from `s_A` and the
//!    pre-update `dl[5..7]`.

use crate::tables;

/// Per-channel Stage-A filter state. See spec §2.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LmsState {
    pub dl: [i32; 8],
    pub dx: [i32; 8],
    pub qm: [i32; 8],
    pub error: i32,
    pub shift: i32,
    pub round: i32,
    /// STEP 4 regeneration magnitudes for taps 4..7 (spec §4.5),
    /// copied out of the lazily-parsed `tables/lms-dx-magnitudes.csv`
    /// snapshot ONCE at frame init. Round-285 profiling showed the
    /// previous per-step `tables::lms_dx_magnitudes()` call paying a
    /// synchronised lazy-init check on every sample × channel inside
    /// the hottest decode function; caching the four constants here
    /// keeps the CSV-sourced table policy while moving that check out
    /// of the inner loop.
    pub(crate) dx_mags: [i32; 4],
}

impl LmsState {
    /// Frame-entry init for `bytes_per_sample` (`(bps + 7) / 8`).
    /// Per spec §3.1, every per-channel field resets to zero;
    /// `shift` / `round` are loaded from the LMS_SHIFT table.
    pub fn frame_init(bytes_per_sample: usize) -> Self {
        let shift = tables::lms_shift(bytes_per_sample);
        let round = 1i32 << (shift - 1);
        Self {
            dl: [0; 8],
            dx: [0; 8],
            qm: [0; 8],
            error: 0,
            shift,
            round,
            dx_mags: *tables::lms_dx_magnitudes(),
        }
    }

    /// One Stage-A step: consume residual `e`, return the
    /// reconstructed sample `s_A = e + p_A`. Updates the state in
    /// place per spec §4.2 STEPs 1..5.
    ///
    /// This is the single authoritative implementation of the
    /// five-step update; [`Self::step_traced`] wraps it with pre/post
    /// state snapshots for the spec/06 trace emitter.
    #[inline]
    pub fn step(&mut self, e: i32) -> i32 {
        // STEP 1 — sign-LMS qm update gated on the previous step's
        // residual, currently held in `self.error`. Implemented
        // branch-free as `qm[i] += sign(error) * dx[i]`: residual
        // signs on real audio are close to random, so the spec's
        // three-way gate costs a hard-to-predict branch per sample —
        // multiplying by sign ∈ {-1, 0, +1} computes the identical
        // wrapping result (`+dx`, `-dx`, or `+0`) and vectorises with
        // the STEP 2 dot product.
        let sgn = (self.error > 0) as i32 - (self.error < 0) as i32;
        for i in 0..8 {
            self.qm[i] = self.qm[i].wrapping_add(sgn.wrapping_mul(self.dx[i]));
        }
        // STEP 2 — prediction (dot product with rounding addend, then
        // arithmetic right shift).
        let mut sum: i32 = self.round;
        for i in 0..8 {
            sum = sum.wrapping_add(self.dl[i].wrapping_mul(self.qm[i]));
        }
        let p_a = sum >> self.shift;

        // STEP 3 — head→tail shift of dx[0..3] and dl[0..3].
        for i in 0..4 {
            self.dx[i] = self.dx[i + 1];
            self.dl[i] = self.dl[i + 1];
        }
        // Snapshot pre-update dl[4..7] for use in STEPs 4-5.
        let dl_pre = [self.dl[4], self.dl[5], self.dl[6], self.dl[7]];

        // STEP 4 — regenerate dx[4..7] from sign(dl_pre[4..7]); zero
        // maps to the positive branch (spec §4.5).
        let mags = self.dx_mags;
        for ((d, mag), dlp) in self.dx[4..].iter_mut().zip(mags).zip(dl_pre) {
            *d = if dlp < 0 { -mag } else { mag };
        }

        // STEP 5 — save residual feedback, form output, regenerate
        // dl[4..7] as cumulative finite differences. The closed form
        // (spec §5.3) is used here for clarity.
        self.error = e;
        let s_a = e.wrapping_add(p_a);
        self.dl[7] = s_a;
        self.dl[6] = s_a.wrapping_sub(dl_pre[3]);
        self.dl[5] = s_a.wrapping_sub(dl_pre[2]).wrapping_sub(dl_pre[3]);
        self.dl[4] = s_a
            .wrapping_sub(dl_pre[1])
            .wrapping_sub(dl_pre[2])
            .wrapping_sub(dl_pre[3]);
        s_a
    }

    /// Same as [`Self::step`] but also returns the intermediate values
    /// needed to populate `LMS_PRE`, `STAGE_A_PREDICT`, and `LMS_POST`
    /// trace events per `spec/06-trace-contract.md` §5.4.
    ///
    /// `LmsTrace.dl_pre` / `dx_pre` / `qm_pre` are the snapshots taken
    /// **before** the step modifies the state; `dl_post` / `dx_post` /
    /// `qm_post` are the post-update state copied straight off `self`.
    /// `error_pre` is the value of `self.error` at function entry (=
    /// the previous step's residual, which gates the STEP 1 sign-LMS
    /// update). `predicted_a` is recovered as
    /// `sample_after_a - e` — exact under wrapping arithmetic since
    /// [`Self::step`] forms `s_A = e + p_A` the same way.
    #[allow(dead_code)] // only called by the cfg-gated trace pipeline.
    pub fn step_traced(&mut self, e: i32) -> LmsTrace {
        let dl_pre = self.dl;
        let dx_pre = self.dx;
        let qm_pre = self.qm;
        let error_pre = self.error;

        let s_a = self.step(e);

        LmsTrace {
            dl_pre,
            dx_pre,
            qm_pre,
            error_pre,
            predicted_a: s_a.wrapping_sub(e),
            sample_after_a: s_a,
            dl_post: self.dl,
            dx_post: self.dx,
            qm_post: self.qm,
        }
    }
}

/// Side-channel return of one [`LmsState::step_traced`] call — the
/// raw intermediate values required by the spec/06 trace emitter.
#[allow(dead_code)] // many fields only referenced by the trace emitter (cfg-gated).
pub struct LmsTrace {
    pub dl_pre: [i32; 8],
    pub dx_pre: [i32; 8],
    pub qm_pre: [i32; 8],
    pub error_pre: i32,
    pub predicted_a: i32,
    pub sample_after_a: i32,
    pub dl_post: [i32; 8],
    pub dx_post: [i32; 8],
    pub qm_post: [i32; 8],
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §7.1 — sample 0: zero residual against zero state stays
    /// zero, except dx[4..7] regenerates to (1, 2, 2, 4) via the
    /// positive branch.
    #[test]
    fn sample_zero_matches_spec_7_1() {
        let mut s = LmsState::frame_init(2);
        assert_eq!(s.shift, 9);
        assert_eq!(s.round, 256);
        let out = s.step(0);
        assert_eq!(out, 0);
        assert_eq!(s.dl, [0; 8]);
        assert_eq!(s.dx, [0, 0, 0, 0, 1, 2, 2, 4]);
        assert_eq!(s.qm, [0; 8]);
        assert_eq!(s.error, 0);
    }

    /// Spec §7.2 — sample 1: residual 1026 with previous state from
    /// the §7.1 step yields qm unchanged, p_A = 0, output 1026,
    /// `dx_post = (0,0,0,1,1,2,2,4)`, `dl_post = [0,0,0,0,1026,1026,1026,1026]`.
    #[test]
    fn sample_one_matches_spec_7_2() {
        let mut s = LmsState::frame_init(2);
        let _ = s.step(0); // sample 0
        let out = s.step(1026);
        assert_eq!(out, 1026);
        assert_eq!(s.dx, [0, 0, 0, 1, 1, 2, 2, 4]);
        assert_eq!(s.dl, [0, 0, 0, 0, 1026, 1026, 1026, 1026]);
        assert_eq!(s.qm, [0; 8]);
        assert_eq!(s.error, 1026);
    }

    /// Spec §7.3 — sample 2: first non-trivial qm update; residual
    /// 1038, prediction 18, output 1056. Validates the updated
    /// qm = (0,0,0,1,1,2,2,4) and `dl_post = [0,0,0,1026,-2022,-996,30,1056]`.
    #[test]
    fn sample_two_matches_spec_7_3() {
        let mut s = LmsState::frame_init(2);
        let _ = s.step(0);
        let _ = s.step(1026);
        let out = s.step(1038);
        assert_eq!(out, 1056);
        assert_eq!(s.qm, [0, 0, 0, 1, 1, 2, 2, 4]);
        assert_eq!(s.dl, [0, 0, 0, 1026, -2022, -996, 30, 1056]);
        assert_eq!(s.dx, [0, 0, 1, 1, 1, 2, 2, 4]);
        assert_eq!(s.error, 1038);
    }
}
