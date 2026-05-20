# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Round-4: ID3v1 / APEv2 trailer detection per `spec/01` §7. New
  public entry points `scan_trailers(bytes) -> Result<TrailerInfo>`
  (parses the optional ID3v2 prefix + stream header + seek table to
  compute the end-of-stream byte boundary, then signature-scans the
  post-stream region) and `detect_trailers(bytes, eos_off) ->
  TrailerInfo` (signature-scans a region given an explicit
  end-of-stream offset; never reads bytes inside the TTA1 frame
  region). `TrailerInfo` exposes `id3v1` / `apev2` as absolute
  `(start, len)` byte ranges. ID3v1 detection follows spec §7's
  "last 128 bytes start with `'T','A','G'`" rule; APEv2 detection
  reads the 32-byte footer's `tag_size` field (LE u32 at footer
  offset 12) plus the "has-header" flag (footer offset 20, bit 31)
  to recover the full APE region span. Bogus `tag_size` values that
  would overrun the post-stream region are silently rejected (the
  trailer is reported as "not present"). Out-of-stream metadata
  parsing itself remains host-application territory per spec §7.
- Round-3: production TTA1 encoder. New public entry points
  `encode(samples, channels, bits_per_sample, sample_rate)` and
  `encode_with_password(.., password)` produce complete TTA1 byte
  streams (header + seek table + frame blobs) from interleaved `i32`
  PCM. The encoder is the symmetric inverse of the existing decoder:
  forward channel decorrelation (`spec/04` §3.1), Stage-B prediction
  subtraction (`spec/03` §4.3), Stage-A LMS step with residual
  feedback (`spec/02` §4.2), zigzag + adaptive-Rice with the
  decoder's lock-stepped `(k0, k1, sum0, sum1)` trackers (`spec/05`
  §5.2 / §5.3), per-frame byte alignment + IEEE-802.3 CRC32
  (`spec/01` §5.3 / §5.4), then header + seek table assembly
  (`spec/01` §3 / §4). Output is bit-exactly round-trippable through
  the existing `decode` / `decode_with_password` entry points across
  every fixture in the existing test suite (16-bit / 24-bit,
  1..=6 channels, silence / sine / pseudo-noise / DC+impulse / multi-
  frame; format=1 + format=2). Replaces the previous `#[cfg(test)]`
  internal encoder.
- Round-3: framework `Encoder` impl wired through the existing
  `registry` feature. The same `CodecInfo::new("tta")` registration
  that already carried `decoder(make_decoder)` now also carries
  `encoder(make_encoder)`, so `CodecRegistry::first_encoder(&params)`
  returns a working TTA encoder. The adapter accepts interleaved
  S16/S24 audio frames, buffers the PCM, and emits one self-contained
  TTA1 file as a keyframe packet on `flush()`.
- Round-3: new `Error::InvalidSampleBuffer` variant raised when the
  encoder is handed an interleaved PCM buffer whose length is not a
  multiple of the requested channel count.
- Frame-boundary streaming demuxer + O(1) seek (`Demuxer::seek_to`)
  built on the TTA1 in-file seek table. Each demuxer packet is a
  self-contained mini-TTA1 file carrying exactly one audio frame
  (re-prefixed header + 1-entry seek table + that frame's body),
  emitted with monotonically increasing pts in samples per
  `time_base = 1/sample_rate`. `seek_to(pts)` is a constant-time
  lookup: `target_frame = min(pts.max(0) / regular_frame_samples,
  n_frames - 1)` per `spec/01-bitstream-framing.md` §4.1, with
  `regular_frame_samples = floor(sample_rate * 256 / 245)`.
  Sub-frame pts requests snap to the containing frame's first
  sample, negative pts clamp to 0, past-end pts clamp to the last
  frame. Decoder per-channel state (LMS / Stage-B / Rice) resets at
  every frame boundary by construction (`spec/02..05`), so the
  demuxer does not coordinate decoder reset — the next mini-file
  packet starts a fresh decoder run. Covered by five tests in
  `src/seek_tests.rs`: `seek_to_zero_resets_to_first_frame`,
  `seek_at_frame_boundary_lands_exact`,
  `seek_mid_frame_lands_at_containing_frame_start`,
  `seek_past_end_clamps_to_last_frame`, and
  `seek_pts_matches_decoder_output_after_seek` (encode → seek →
  decode → byte-identical PCM round-trip).
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
