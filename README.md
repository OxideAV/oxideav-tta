# oxideav-tta

Pure-Rust **True Audio (TTA)** lossless audio decoder. Zero C
dependencies, no FFI, no `*-sys` crates.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## What works

- 22-byte `TTA1` stream header parser, with CRC32 verification.
- Seek-table parser and CRC32 verification.
- Per-frame decoder:
  - Two-mode adaptive Rice entropy decoder (k0/k1 with `1 << k0`
    escape threshold and per-sample `sum/k` updates).
  - Stage-A 8-tap sign-LMS adaptive filter (per-channel state).
    Round-2 calibration: `dx[]` gradient is now sourced from the
    *shifted-in* `dl[]` values BEFORE the per-iteration `dl[4..=7]`
    regen overwrites them, matching the encoder's gradient ordering.
    First ~17 samples of a 440 Hz / 16-bit sine reproduce bit-exactly
    against the ffmpeg-encoded reference; sub-LSB drift follows once
    the coefficient vector saturates. See the "Gaps" section.
  - Stage-B fixed-order integer predictor (`(prev × ((1<<k)-1)) >> k`,
    `k = 4` for 8-bit, `k = 5` for 16/24-bit).
  - Pairwise inter-channel decorrelation (decoder direction).
  - Per-frame CRC32 verification.
- Output sample formats:
  - 8-bit  → `SampleFormat::U8` (decoder adds the `+0x80` bias).
  - 16-bit → `SampleFormat::S16`.
  - 24-bit → `SampleFormat::S32`, expanded by left-shift 8 (low byte
    always zero), matching the FFmpeg-side convention noted in the
    spec.

## Tested

- **Bit-exact lossless round-trip on digital silence** (44.1 kHz /
  16-bit mono) via `tests/silence.rs`: the source is encoded by the
  system `ffmpeg` binary into a `.tta` file, decoded by this crate,
  and the recovered PCM is asserted byte-identical to the source.
  The LMS filter never adapts away from its all-zero initial state
  on this input, so this test is a clean check of the file-layout +
  Rice + Stage-B + decorrelation paths.
- Header / seek-table / per-frame CRC32 verification, against
  hand-built bitstreams (`tests/handcrafted.rs`).
- Adaptive Rice decoder unit tests (depth-0 and depth-1 paths with
  fixed `k`).

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-tta = "0.0"
```

## Quick use

The decoder takes its setup from a 22-byte TTA stream header passed in
`CodecParameters::extradata`. Each `Packet` carries one frame body
*including* its trailing 4-byte CRC32; `parse_file` will split a
complete `.tta` file into header + per-frame slices.

```rust,no_run
use oxideav_core::{CodecId, CodecParameters, Frame, MediaType, Packet};
use oxideav_tta::container::parse_file;

let bytes = std::fs::read("song.tta")?;
let parsed = parse_file(&bytes).unwrap();

let mut params = CodecParameters::audio(CodecId::new("tta"));
params.extradata = bytes[..22].to_vec();
params.sample_rate = Some(parsed.header.sample_rate);
params.channels = Some(parsed.header.channels);

let mut dec = oxideav_tta::decoder::make_decoder(&params)?;
for fr in parsed.frames {
    let body = &bytes[fr.offset..fr.offset + fr.size];
    dec.send_packet(&Packet { data: body.to_vec(), pts: None, ..Default::default() })?;
    if let Frame::Audio(_af) = dec.receive_frame()? {
        // _af.data[0] is interleaved PCM in the format dictated by the
        // header bit-depth (U8 / S16 / S32-expanded-from-S24).
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Gaps / remaining round-2 work

- **Stage-A 8-tap LMS filter is now bit-exact for the first ~17
  samples** of a 440 Hz / 16-bit sine, then accumulates a sub-LSB
  drift that grows linearly with the signal slope. The dx[]-source
  ordering (shifted-in dl[] values rather than freshly-regenerated
  ones) was the dominant fix and lifted the first-divergence point
  from sample 4 to sample 17, which is roughly the point where the
  cumulative LMS update saturates against the integer rounding
  threshold of the predictor's `>> 9` shift. The remaining gap is
  a single off-by-one at the round-half-up boundary that has not
  yielded to any of the variations of {dl[]-regen telescoping,
  dx[] magnitude pattern, LMS update timing, round-bias selection}
  exercised by `tests/inspect.rs`. The trace doc covers this stage
  at the conceptual level only ("4-deep telescoping pattern of the
  current prediction and recent sample differences"); the final
  formula needs to be added to the doc before the round-3 patch
  can be made bit-exact. Three sine round-trip tests in
  `tests/ffmpeg_roundtrip.rs` stay `#[ignore]`'d for now; the
  silence round-trip in `tests/silence.rs` exercises the full
  CRC + Rice + Stage-B + decorrelation pipeline and runs by default.
- **Encoder**: not yet implemented. Round-trip tests use the `ffmpeg`
  binary as a black-box encoder.
- **`format == 2` (encrypted)**: gated on a 64-bit ECMA-182 password
  CRC seed for the LMS weights. Not implemented; decoder rejects.
- **Bit-depth 32**: code paths exist in the spec (filter shift 12,
  Stage-B `k = ∞`); not exercised by any extant TTA file because no
  shipping encoder writes them. Would need a separate test corpus.
- **Native demuxer**: only a thin file walker (`container::parse_file`)
  is provided. APE-tag / ID3v1 trailer skipping is implicit (frames
  past the seek table are read by absolute offset).
- **Channels above 8**: the spec doc notes the format permits up to
  16; this implementation refuses anything above 8 to match the
  channel-layout table covered in the spec.

## Spec reference

Behavioural specification:
[`docs/audio/tta/tta-trace-reverse-engineering.md`](https://github.com/OxideAV/oxideav-workspace/blob/master/docs/audio/tta/tta-trace-reverse-engineering.md).
This crate is a **clean-room** implementation: no third-party TTA
source code (libavcodec, libtta, …) was consulted while writing it.

## License

MIT. See `LICENSE`.
