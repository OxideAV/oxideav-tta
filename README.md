# oxideav-tta

Pure-Rust True Audio (TTA) lossless audio codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.
Encoder and decoder, in safe Rust. Encoder output round-trips
bit-exactly through the decoder.

## What works

* **Decode + encode of TTA1 format=1** (integer PCM) and **format=2**
  (password-derived `qm` priming). Signed 16-bit and 24-bit LE PCM,
  1..=6 channels.
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
* Full encode→decode roundtrips on mono / stereo / six-channel
  fixtures, 16-bit and 24-bit, with sine / silence / pseudo-noise /
  DC+impulse content, including multi-frame streams that exercise the
  per-frame state-reset discipline.
* Negative-path: corrupted frame CRC and unsupported header values are
  rejected with the correct `Error` variants.

## Fuzzing

`cargo-fuzz` targets under `fuzz/fuzz_targets/` cover the decoder,
demuxer, encode roundtrip, streaming/random-access decode, sample
ranges, password streaming, trailer scanning, and differential checks
of the typed header and trailer accessors. The contract is
panic-freedom on arbitrary input.

```sh
cargo +nightly fuzz run decode -- -max_total_time=60
```

## Benchmarks

Criterion harnesses under `benches/` characterise the decode, encode,
roundtrip, streaming, and range hot paths on a deterministic synthetic
corpus (mono16 / stereo16 / stereo24 / 6ch16 / format=2). Numbers move
with host hardware; the value is the relative cost across scenarios.

```sh
cargo bench
```

## License

MIT. See [LICENSE](LICENSE).
