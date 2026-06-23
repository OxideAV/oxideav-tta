# oxideav-tta

Pure-Rust True Audio (TTA) lossless audio codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.
Encoder and decoder, in safe Rust. Encoder output round-trips
bit-exactly through the decoder.

## What works

* **Decode + encode of TTA1 format=1** (integer PCM) and **format=2**
  (password-derived `qm` priming). Signed PCM at every in-scope bit
  depth — 16..=24 bits (widths 17..23 packed MSB-aligned in 3 bytes
  per `spec/01` §3.2) — and 1..=6 channels.
* **Full decode pipeline** — bitstream framing with IEEE-802.3 CRC32
  verification at the header, seek table, and per-frame trailer;
  adaptive Rice entropy coding; the Stage-A 8-tap sign-LMS predictor;
  the Stage-B fixed-order recursive predictor; and inverse channel
  decorrelation. The encoder is the symmetric inverse of every stage.
* **Framework integration** (default-on `registry` feature) — a
  `Decoder` impl, an `Encoder` impl, and a raw-`.tta` `Demuxer`
  (codec id `"tta"`) wired through `oxideav_core::register!` and picked
  up by `oxideav-meta::register_all`. The demuxer parses the seek table
  at open, emits one self-contained packet per audio frame, and offers
  O(1) `seek_to(pts)`.
* **Streaming + random-access decode API** on `Decoder`:
  * Lazy iteration: `frame_iter`, `frame_iter_from(index)`.
  * Random access: `decode_frame_at(index)`, `seek_to_sample(sample)`.
  * Player sugar (sample-keyed): `decode_from_sample`,
    `frame_iter_from_sample`.
  * Player sugar (time-keyed): `total_duration`, `seek_to_time`,
    `decode_from_time`, `frame_iter_from_time`.
  * Half-open `[start, end)` ranges: `decode_sample_range`,
    `frame_iter_sample_range`, `decode_time_range`,
    `frame_iter_time_range`.
  All of these agree bit-exactly with the eager `decode_all`, and reach
  format=2 streams via `new_with_password`. Time/duration conversions
  use overflow-free integer arithmetic at nanosecond granularity.
* **ID3v1 / APEv2 trailer detection** — `scan_trailers` /
  `detect_trailers` locate optional out-of-stream metadata trailers and
  return absolute byte ranges without reading inside the frame region.
  Typed accessors (`Id3v1Range` / `ApeV2Range`, `TrailerInfo` sub-field
  views) are provided.
* **Typed validated header views** — `StreamHeader::typed()` returns a
  `TypedStreamHeader` with validated newtype fields and total derived
  projections (`requires_password`, `byte_depth`,
  `regular_frame_samples`, `frame_geometry`, `total_duration`,
  `pcm_byte_len`).
* **Optional debug trace** — a `trace` Cargo feature (off by default)
  emits one TSV event per state transition when
  `OXIDEAV_TTA_TRACE_FILE` is set, for clean-room lockstep diffing.
  Zero overhead when the feature is off.

## Not yet supported

* **Format=3** (IEEE float PCM).
* Bit-exact lockstep against externally-encoded reference fixtures —
  deferred until a sanctioned reference fixture lands in the clean-room
  workspace. Verification today is self-roundtrip plus spec worked-step
  hand-verifications (see below).

## Usage

```rust
use oxideav_tta::{encode, Decoder};

// `samples` is interleaved i32 PCM (S16 or S24 range per bits_per_sample).
let tta_bytes = encode(&samples, channels, bits_per_sample, sample_rate)?;
let mut dec = Decoder::new(&tta_bytes)?;
let pcm = dec.decode_all()?;
# Ok::<(), oxideav_tta::Error>(())
```

Standalone consumers can build with `default-features = false` to drop
the `oxideav-core` dependency and use the direct `encode` / `decode` /
`Decoder` API.

## Why clean-room

OxideAV ships under a permissive license and cannot incorporate
LGPL-licensed source, so every line of this crate is written without
reading any existing TTA implementation. The clean-room workspace at
[`docs/audio/tta-cleanroom/`](https://github.com/OxideAV/docs/tree/master/audio/tta-cleanroom)
is the wall: only `spec/`, `tables/`, and the reference docs are
consulted. Per-bps `shift`/`round` and dx-magnitude tables are loaded
from CSV in `tables/`.

## Verification

* Per-spec hand-verifications transcribed from the spec's worked-step
  examples (Stage-A samples, Stage-B positive/negative state, Rice).
  The Rice path now drives the reference-tape steps (§7.1..§7.5) that
  exercise the high-mode escape bias when `k0 != k1`, the
  STEP-A-before-STEP-B tracker ordering, the steady-state mid-frame
  regime and the first `k1` demotion (§7.3), and the first negative
  residual's zigzag sign branch — each asserted bit-for-bit against the
  spec, residual plus all four post-state trackers, with the §7.3
  step-16 post-state pinned to the §7.4 step-17 pre-state so the
  worked-step walk forms an unbroken chain. A continuous-stream chain
  test decodes the §7.1/§7.2/§7.3-step-2 codewords back-to-back through
  a single bit reader (one 37-bit body across 5 packed bytes, state
  bootstrapped once from `RICE_K_INIT`), exercising the cross-codeword
  bit-cache carry
  that the per-codeword tests — each starting at a fresh byte boundary —
  never reach.
* Channel-decorrelation conformance (`spec/04`) pinned against captured
  reference-tape ground truth, not only the cascade's own algebraic
  inverse. All 31 rows of the §7.1 stereo pseudo-noise table — the
  corpus's most discriminating fixture, with `dec_in[0]` spanning the
  full sign/parity matrix — are asserted bit-for-bit against the inverse
  cascade. The §6 truncating-divide table is pinned operand-by-operand,
  each odd-negative case verified to diverge from arithmetic `>>1` by
  exactly 1 LSB (row 11 lands `(12895, 4528)` under `/2`, `(12894,
  4527)` under the wrong shift). For N>2 (the corpus has only stereo
  tapes, so §7.3 makes the spec's algebraic substitution the ground
  truth) every published intermediate of the §4.1 N=3/N=4 encoder
  formulas and the §4.3 six-step 5.1 walk is pinned, the §9
  anti-patterns are guarded (no odd-N parity branch, bounds-safe mono,
  per-sample statelessness), and a dense 20 000-vector LCG grid drives
  `forward(inverse(.))` through thousands of odd-negative `/2` cases per
  channel count 2..=6. A trace-tape end-to-end check then confirms the
  *full decode pipeline* runs exactly that cascade: every per-sample
  `DECORR_PRE`→`DECORR_POST` transition on real codec-produced stereo /
  3-channel / 6-channel noise streams is reproduced by `inverse`, and
  `PCM_OUT` equals `DECORR_POST` per §1.
* Full encode→decode roundtrips across the parameter matrix: channel
  counts 1..=6 (every intermediate count, so the odd-N decorrelation
  cascade `spec/04` §4.3 warns must not be parity-special-cased is
  exercised at N=3 and N=5) and bit depths 16..=24 (including the
  non-multiple-of-8 widths 17..23, which share `byte_depth = 3` and the
  LMS `shift = 10` row with 24-bit per `spec/01` §3.2). Content spans
  sine / silence / pseudo-noise / DC+impulse / full-scale-impulse,
  including multi-frame streams that exercise the per-frame state-reset
  discipline.
* Encoder seek-table structural invariant (`spec/01` §4.2 / §4.3): the
  encoder's own bytes are re-parsed through the framing parser and each
  entry is asserted to equal the true on-disk frame footprint, the
  offsets to chain exactly, the entry-bytes CRC to validate, the
  per-frame sample counts to sum to `total_samples` (with only the last
  frame short, and a *full* last frame on the `raw == 0` exact-multiple
  case), and every per-frame trailing CRC to match its body. The
  encoder-produced table is then driven through the decoder's
  random-access API (`decode_frame_at` / `seek_to_sample` /
  `frame_iter`) and asserted bit-exact against eager `decode_all` on
  3-channel, 5-channel, and 19-bit-mono multi-frame streams.
* Encoder/decoder adaptive-Rice tracker lock-step properties: for an
  escalating-magnitude ramp that drives `k` far above the valid-stream
  regime, and for a 4096-step pseudo-random residual sweep, the
  encoder's per-step `(k0, k1, sum0, sum1)` is asserted bit-identical
  to the decoder's, the decoded residual equals the input, and neither
  side's `k` escapes the `MAX_K` ceiling — pinning the increment-cap
  symmetry between the two stages.
* Negative-path: corrupted frame CRC and unsupported header values are
  rejected with the correct `Error` variants.

## Fuzzing

`cargo-fuzz` targets under `fuzz/fuzz_targets/` cover the decoder,
demuxer, encode roundtrip, streaming/random-access decode, sample
ranges, password streaming, trailer scanning, the framework
`Decoder`-trait adapter (`registry_decode`: open_demuxer →
`first_decoder` → `send_packet`/`receive_frame`/`flush`, asserting the
`AudioFrame` packed-plane length equals `samples * channels *
bytes_per_sample`), and differential checks of the typed header and
trailer accessors. `encode_roundtrip` drives the bit-exact roundtrip
across the full `16..=24` bit-depth range (every in-scope width,
including the non-multiple-of-8 `17..=23`). `corrupt_decode` reaches
the *deep* decode machinery the header-CRC gate otherwise hides: it
synthesises a valid encoder-produced stream, then byte-corrupts the
region past the 22-byte header (seek table, frame bodies, per-frame
trailers) so the decoder's seek-table walk, per-frame CRC check, Rice
decoder, LMS / Stage-B predictors, and inverse decorrelation are all
exercised on inputs that parse past the header. The contract is
panic-freedom on arbitrary input.

```sh
cargo +nightly fuzz run decode -- -max_total_time=60
```

## Benchmarks

Criterion harnesses under `benches/` characterise the decode, encode,
roundtrip, streaming, range, and framework-demuxer hot paths on a
deterministic synthetic corpus (mono16 / stereo16 / stereo24 / 6ch16 /
format=2). The `demuxer` harness covers the registry `Demuxer` open /
`next_packet`-drain / O(1) `seek_to` paths. Numbers move with host
hardware; the value is the relative cost across scenarios.

```sh
cargo bench
```

## License

MIT. See [LICENSE](LICENSE).
