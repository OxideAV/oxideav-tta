//! Stage-A LMS tables loaded from `docs/audio/tta-cleanroom/tables/`.
//!
//! Per `spec/02-stage-a-lms.md` §10 and the workspace's `tables/`
//! policy, the Implementer must `include_str!` the CSV-form tables
//! published by the Extractor rather than retyping the values into
//! Rust source. We do exactly that here, then parse the CSVs once at
//! startup into compact arrays via `OnceLock`.

use std::sync::OnceLock;

/// CSV body of `tables/lms-shift.csv` (3 rows, header `index,shift`).
/// Vendored snapshot of the cleanroom workspace's
/// `docs/audio/tta-cleanroom/tables/lms-shift.csv`; see `tables/README.md`.
const LMS_SHIFT_CSV: &str = include_str!("../tables/lms-shift.csv");

/// CSV body of `tables/lms-dx-magnitudes.csv` (4 rows, header
/// `dx_index,magnitude`). Vendored snapshot of the cleanroom workspace's
/// `docs/audio/tta-cleanroom/tables/lms-dx-magnitudes.csv`; see
/// `tables/README.md`.
const LMS_DX_MAGNITUDES_CSV: &str = include_str!("../tables/lms-dx-magnitudes.csv");

static LMS_SHIFT_TABLE: OnceLock<[i32; 3]> = OnceLock::new();
static LMS_DX_MAGNITUDES_TABLE: OnceLock<[i32; 4]> = OnceLock::new();

fn parse_lms_shift_table() -> [i32; 3] {
    let mut out = [0i32; 3];
    let mut seen = [false; 3];
    for line in LMS_SHIFT_CSV.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split(',');
        let idx: usize = it
            .next()
            .expect("lms-shift.csv: missing index field")
            .trim()
            .parse()
            .expect("lms-shift.csv: index field must parse as usize");
        let shift: i32 = it
            .next()
            .expect("lms-shift.csv: missing shift field")
            .trim()
            .parse()
            .expect("lms-shift.csv: shift field must parse as i32");
        assert!(idx < 3, "lms-shift.csv: index {idx} out of range");
        out[idx] = shift;
        seen[idx] = true;
    }
    assert!(
        seen.iter().all(|&s| s),
        "lms-shift.csv: not all indices 0..3 present"
    );
    out
}

fn parse_lms_dx_magnitudes_table() -> [i32; 4] {
    let mut out = [0i32; 4];
    let mut seen = [false; 4];
    for line in LMS_DX_MAGNITUDES_CSV.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split(',');
        let dx_index: usize = it
            .next()
            .expect("lms-dx-magnitudes.csv: missing dx_index")
            .trim()
            .parse()
            .expect("lms-dx-magnitudes.csv: dx_index field must parse as usize");
        let mag: i32 = it
            .next()
            .expect("lms-dx-magnitudes.csv: missing magnitude field")
            .trim()
            .parse()
            .expect("lms-dx-magnitudes.csv: magnitude field must parse as i32");
        // Spec §4.5: the table is indexed by dx_index in 4..=7 (taps
        // 4 through 7 of the filter); store contiguously as 0..=3.
        assert!(
            (4..=7).contains(&dx_index),
            "lms-dx-magnitudes.csv: dx_index {dx_index} out of expected 4..=7"
        );
        let local = dx_index - 4;
        out[local] = mag;
        seen[local] = true;
    }
    assert!(
        seen.iter().all(|&s| s),
        "lms-dx-magnitudes.csv: not all dx_indexes 4..=7 present"
    );
    out
}

/// Per-bps Stage-A right shift; index = `bytes_per_sample - 1`.
/// Per spec §3.2 only `bytes_per_sample ∈ {2, 3}` are reachable.
pub fn lms_shift(bytes_per_sample: usize) -> i32 {
    let table = LMS_SHIFT_TABLE.get_or_init(parse_lms_shift_table);
    let idx = bytes_per_sample
        .checked_sub(1)
        .expect("bytes_per_sample must be >= 1");
    table[idx]
}

/// dx-magnitude table for taps 4..7 (returned in tap order, i.e.
/// `[mag(tap4), mag(tap5), mag(tap6), mag(tap7)]`).
pub fn lms_dx_magnitudes() -> &'static [i32; 4] {
    LMS_DX_MAGNITUDES_TABLE.get_or_init(parse_lms_dx_magnitudes_table)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §3.2 — bps=16 has bytes_per_sample=2 → shift=9; bps=24 has
    /// bytes_per_sample=3 → shift=10. `bytes_per_sample=1` is the
    /// unreachable 8-bit slot tabulated as `10` for completeness.
    #[test]
    fn shifts_match_spec_3_2() {
        assert_eq!(lms_shift(2), 9);
        assert_eq!(lms_shift(3), 10);
        assert_eq!(lms_shift(1), 10);
    }

    /// Spec §4.5 / table — taps (4,5,6,7) carry magnitudes (1,2,2,4).
    #[test]
    fn dx_magnitudes_match_spec() {
        assert_eq!(lms_dx_magnitudes(), &[1, 2, 2, 4]);
    }
}
