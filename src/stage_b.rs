//! Stage-B fixed-order recursive predictor per `spec/03-stage-b.md`.
//!
//! One per-channel signed 32-bit register `prev`, reset to `0` at every
//! frame boundary. The per-step update is:
//!
//! ```text
//! p_B = (prev * 31) >> 5     (arithmetic right shift, no rounding addend)
//! s_B = s_A + p_B
//! prev_post = s_B
//! ```
//!
//! `k = 5` is hard-coded for every supported bps per spec §2 / §10.

/// Per-channel Stage-B state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StageBState {
    pub prev: i32,
}

impl StageBState {
    /// Frame-entry init: spec §3.
    pub const fn frame_init() -> Self {
        Self { prev: 0 }
    }

    /// One Stage-B step. Returns `s_B` and stores it as the new
    /// `prev`.
    #[inline]
    #[allow(dead_code)] // direct callers vanish under `--features trace`.
    pub fn step(&mut self, s_a: i32) -> i32 {
        self.step_traced(s_a).sample_after_b
    }

    /// Same as [`Self::step`] but exposes the intermediate values
    /// required by the spec/06 `STAGE_B_PREDICT` trace event:
    /// `prev_in` (state at function entry), `predicted_b` (= `(prev *
    /// 31) >> 5`), `residual_b` (= the input `s_a`), and
    /// `sample_after_b` (= the output, also the new `prev`).
    #[inline]
    pub fn step_traced(&mut self, s_a: i32) -> StageBTrace {
        let prev_in = self.prev;
        let p_b = self.prev.wrapping_mul(31) >> 5;
        let s_b = s_a.wrapping_add(p_b);
        self.prev = s_b;
        StageBTrace {
            prev_in,
            predicted_b: p_b,
            residual_b: s_a,
            sample_after_b: s_b,
        }
    }
}

/// Side-channel return of one [`StageBState::step_traced`] call — the
/// values feeding the spec/06 `STAGE_B_PREDICT` event.
#[allow(dead_code)] // many fields only referenced by the trace emitter (cfg-gated).
pub struct StageBTrace {
    pub prev_in: i32,
    pub predicted_b: i32,
    pub residual_b: i32,
    pub sample_after_b: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §8: prev=1026 → predicted_b = 993, then s_A=1056 →
    /// s_B = 2049 (= 1056 + 993).
    #[test]
    fn positive_prev_walk() {
        let mut s = StageBState::frame_init();
        // Sample 0: s_A=0 with prev=0 → s_B=0.
        assert_eq!(s.step(0), 0);
        // Sample 1: s_A=1026, prev=0 → s_B=1026.
        assert_eq!(s.step(1026), 1026);
        // Sample 2: s_A=1056, prev=1026 → predicted_b = 1026*31>>5 =
        // 31806>>5 = 993; s_B = 2049.
        assert_eq!(s.step(1056), 2049);
    }

    /// Spec §8.1: negative `prev = -910` arithmetic-shift discriminator.
    /// `(-910 * 31) = -28210; >> 5 = -882` (floor toward -∞, not -881
    /// truncating-toward-zero).
    #[test]
    fn negative_prev_arithmetic_shift() {
        let mut s = StageBState::frame_init();
        s.prev = -910;
        let s_b = s.step(-1051);
        assert_eq!(s_b, -1933);
        assert_eq!(s.prev, -1933);
    }
}
