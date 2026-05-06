# oxideav-tta

Pure-Rust True Audio (TTA) lossless audio codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 1 — clean-room decoder.** Decodes TTA1 format=1 (integer PCM)
streams in pure safe Rust against the strict-isolation clean-room
workspace at
[`docs/audio/tta-cleanroom/`](https://github.com/OxideAV/docs/tree/master/audio/tta-cleanroom).

The fresh orphan `master` is the starting point; the previous
implementation, retired alongside the OxideAV docs audit dated
2026-05-06 (see
[AUDIT-2026-05-06.md](https://github.com/OxideAV/docs/blob/master/AUDIT-2026-05-06.md)),
is preserved on the `old` branch for reference but is **not** used as
input to this rebuild.

## What round 1 covers

- **Bitstream framing** (`spec/01`): TTA1 header parse, optional ID3v2
  prefix skip, seek table consumer, frame iterator, IEEE-802.3 CRC32
  verification at all three sites (header, seek table, per-frame
  trailer).
- **Adaptive Rice entropy decoder** (`spec/05`): LSB-first bit reader
  with the unary fast path; per-channel `(k0, k1, sum0, sum1)`
  trackers reset to `(10, 10, 0x4000, 0x4000)` per frame; depth-1
  escape bias `(1 << k0)`; zigzag de-zigzag (odd → positive, even →
  negative).
- **Stage-A 8-tap sign-LMS predictor** (`spec/02`): `(dl[8], dx[8],
  qm[8], error)` per channel, all reset to zero per frame; per-bps
  `shift`/`round` loaded from `tables/lms-shift.csv` via
  `include_str!`; `dx[4..7]` magnitudes loaded from
  `tables/lms-dx-magnitudes.csv`; spec §4.2 five-step update sequence.
- **Stage-B fixed-order recursive predictor** (`spec/03`): single
  `prev` register per channel, reset to zero per frame; `(prev * 31)
  >> 5` with arithmetic right shift (no rounding addend).
- **Inverse channel decorrelation** (`spec/04`): mono passthrough;
  stereo and N>2 cascade walking from highest channel index downward;
  C signed truncating `/2` (NOT `>>1`) per spec §6.
- **PCM packing**: signed 16-bit LE and signed 24-bit LE output via
  `pack_pcm` per spec §3.2.

Out of scope for round 1 (deferred to later rounds): format=2
(encrypted), format=3 (IEEE float), production encoder, container
demuxer beyond what's needed to feed the decoder, the Stage-06 trace
contract debug instrumentation.

## Why clean-room

libtta is the canonical TTA reference (Aleksander Djuric / Pavel
Zhilin, en.true-audio.com, LGPL-2.1). oxideav cannot ship LGPL code,
so every line of this crate is written without reading libtta or any
FFmpeg-derived TTA source. The clean-room workspace at
`docs/audio/tta-cleanroom/` is the wall: the Implementer reads only
`spec/`, `tables/`, and `reference/docs/`.

## Verification

The Implementer round 1 deliverable is decoder-only, but verification
requires fixtures the workspace itself sanctions. The
`audit/reference-tapes/**` and `reference/inputs/**` trees are
gitignored, so round-1 verification is performed via a crate-internal
test-only encoder (`#[cfg(test)] mod encoder`) that mirrors the
decoder's state machines. Tests exercise:

- Per-spec hand-verifications transcribed from `spec/02..05` §7
  worked-step examples (Stage-A samples 0..2, Stage-B positive +
  negative `prev`, Rice step 0).
- Full encode-decode roundtrips on mono / stereo / six-channel
  fixtures, 16-bit and 24-bit, with sine / silence / pseudo-noise /
  DC+impulse content.
- Multi-frame roundtrip (2.5 s at 44.1 kHz spans three TTA frames)
  exercising the per-frame state-reset discipline.
- Negative-path: corrupted frame CRC and unsupported header values
  are rejected with the correct `Error` variants.

Bit-exact lockstep against libtta-encoded fixtures is deferred to a
future Auditor round once a sanctioned reference fixture lands in the
clean-room workspace.
