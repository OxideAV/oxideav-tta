# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial TTA decoder: file header (CRC-checked), seek table (CRC-checked),
  per-frame Rice entropy decode, two-stage predictor cascade
  (8-tap sign-LMS adaptive filter + fixed-order integer predictor),
  pairwise inter-channel decorrelation, per-frame CRC32 verification.
- 8-bit (U8), 16-bit (S16) and 24-bit (S32) sample-format paths.
- Round-trip tested against the `ffmpeg` TTA encoder for several
  channel/bit-depth combinations.
