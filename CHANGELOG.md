# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Stage-A 8-tap sign-LMS filter: round-2 calibration of the `dx[]`
  gradient regeneration. The position-`±{1,2,2,4}` step is now sourced
  from the *shifted-in* `dl[i]` values (i.e. before the per-iteration
  `dl[4..=7]` regeneration overwrites them) rather than the freshly
  regenerated `dl[i]`. This matches the encoder's gradient-vector
  ordering and pushes the first-divergence point on a 440 Hz / 16-bit
  sine roundtrip from sample 4 out to sample 17.

### Known limitations
- Stage-A still drifts after sample ~17 on non-silence signals —
  sub-LSB at first, growing linearly with signal slope. The remaining
  gap is at the round-half-up boundary of the predictor's `>> 9`
  output shift; the trace doc does not yet specify the exact formula
  variant that resolves it. Three sine roundtrip tests in
  `tests/ffmpeg_roundtrip.rs` remain `#[ignore]`'d pending.

## [0.0.1]

### Added
- Initial TTA decoder: file header (CRC-checked), seek table (CRC-checked),
  per-frame Rice entropy decode, two-stage predictor cascade
  (8-tap sign-LMS adaptive filter + fixed-order integer predictor),
  pairwise inter-channel decorrelation, per-frame CRC32 verification.
- 8-bit (U8), 16-bit (S16) and 24-bit (S32) sample-format paths.
- Round-trip tested against the `ffmpeg` TTA encoder for the silence
  fixture (44.1 kHz / 16-bit / mono); LMS state never adapts away
  from zero on this input, exercising the whole non-LMS pipeline
  end-to-end.
