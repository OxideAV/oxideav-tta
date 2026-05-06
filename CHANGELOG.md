# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Round-2: `spec/06-trace-contract.md` debug trace emitter behind
  the new `trace` Cargo feature (off by default). With the feature
  on AND `OXIDEAV_TTA_TRACE_FILE=<path>` set, the decoder writes
  one TSV event line per state transition to that path,
  implementing all 18 events (`FILE_HEADER`, `HEADER_CRC`,
  `SEEK_TABLE_*`, `LMS_INIT`, `RICE_K_INIT`, `FRAME_BEGIN`/`_END`,
  per-step `RICE_DECODE` / `RICE_K_UPDATE` / `LMS_PRE` /
  `STAGE_A_PREDICT` / `LMS_POST` / `STAGE_B_PREDICT`, per-sample
  `DECORR_PRE` / `DECORR_POST` / `PCM_OUT`) with the field schemas
  from spec/06 §5. Zero overhead at runtime when the feature is
  off.
- Round-2: `oxideav-core` framework integration behind the
  default-on `registry` feature: a `Decoder` impl (codec id
  `"tta"`, capability flags `with_lossless / with_intra_only`,
  S16/S24 output), a raw `.tta` `Demuxer` (`tta` extension +
  TTA1-magic probe, ID3v2 prefix tolerated), and the
  `register(ctx)` entry point that `oxideav-meta::register_all`
  reaches via `oxideav_core::register!("oxideav-tta", register)`.
  Standalone (no-`oxideav-core`) consumers can opt out with
  `default-features = false`.
- Round-2: format=2 (password-derived qm priming) per `spec/07`.
  New `decode_with_password(bytes, password)` entry point computes
  an ECMA-182 CRC-64 digest of the password (forward / unreflected
  polynomial `0x42F0E1EBA9EA3693`, init / output XOR
  `0xFFFFFFFFFFFFFFFF`), unpacks the digest into eight signed-int8
  bytes per spec/07 §3.4, and primes Stage-A's `qm[0..7]` (sign-
  extended to int32) at every per-channel frame init. Plain
  `decode()` surfaces `Error::PasswordRequired` for format=2
  streams. Empty-password edge case (spec/07 §9 item 2) produces
  an all-zero priming, bit-identical to format=1.
- Round-1: TTA1 format=1 (integer PCM) decoder built against the
  clean-room workspace at `docs/audio/tta-cleanroom/`. Covers framing
  (`spec/01` header + seek table + per-frame CRC32), adaptive Rice
  entropy decoder (`spec/05`), 8-tap sign-LMS Stage-A predictor
  (`spec/02`), fixed-order Stage-B predictor (`spec/03`), and
  pairwise inverse channel decorrelation (`spec/04`) for all
  in-scope channel counts (1..=6) and bit depths (16, 24).
- Public surface: `decode`, `decode_with_password`, `pack_pcm`,
  `Decoder`, `decode_frame`, `StreamHeader` / `StreamInfo`,
  `FrameDescriptor`, and the crate's `Error` / `Result` types.
- `tables/lms-shift.csv` and `tables/lms-dx-magnitudes.csv` are
  loaded via `include_str!` and parsed once at startup, per the
  workspace's "no retyping numeric tables" policy.
- Crate-internal test-only encoder (`#[cfg(test)] mod encoder`) that
  manufactures self-consistent TTA1 streams (format=1 and format=2)
  for roundtrip testing, since no reference TTA fixtures are
  checked in.

### Changed

- Clean-room rebuild from a fresh orphan `master`. The previous
  implementation was retired by the OxideAV docs audit dated
  2026-05-06 (FFmpeg source cited as the writeup's basis, not merely
  as the trace-instrumentation host); the prior history is preserved
  on the `old` branch.
- The new code is being written against the strict-isolation
  clean-room workspace at `docs/audio/tta-cleanroom/` (Specifier /
  Extractor / Implementer / Auditor roles, with explicit allow-list
  and forbidden-input list per role). The Implementer reads only
  `spec/` + `tables/` + `reference/docs/`; libtta and
  FFmpeg `libavcodec/tta*` are forbidden inputs.
