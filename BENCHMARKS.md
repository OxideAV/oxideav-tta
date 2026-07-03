# oxideav-tta benchmarks

Round-386 depth-mode sweep (bench + profile). All numbers below were
taken on one Apple-Silicon (aarch64) macOS host with the workspace
release profile (`opt-level = 3`, `lto = "thin"`, `codegen-units = 1`),
Criterion `--warm-up-time 1 --measurement-time 3`. The host was shared
with other build jobs, so treat absolute values as indicative;
**relative** cost across scenarios and the per-commit deltas (measured
with interleaved min-of-5 wall-clock A/B runs, which are robust to
load drift) are the durable signal.

Every optimisation below is pinned byte-identical by
`tests/bitexact_pins.rs`: FNV-1a-64 digests over both the encoder's
wire bytes and the decoder's PCM for a deterministic multi-frame
corpus (mono/stereo/3ch/6ch × 16/17/20/24 bps + format=2), so a
symmetric encoder+decoder drift that still round-trips would fail
loudly.

## Round-386 optimisation ledger

Per-commit deltas from interleaved min-of-5 wall-clock A/B
(`profile_decode` / `profile_encode`, 150 iterations per run):

| Commit | Change | mono16 | stereo16 | stereo24 | 6ch16 | format2 |
|---|---|---|---|---|---|---|
| `5e6c966` | CRC32 slice-by-8 bulk path | (prereq — see below) | | | | |
| `755dfc8` | u64 bulk-refill bit reader + whole-body one-shot frame CRC (decode) | −5.6% | −16.9% | −14.0% | −19.3% | −15.8% |
| `2e26896` | 4-byte-flush BitWriter + single-put unary (encode) | −7.8% | −11.6% | −13.5% | −20.1% | −14.3% |

Notes:

* The three changes are one design: hoisting the per-frame CRC out of
  the bit reader unlocks the 8-byte bulk refill, and slice-by-8 makes
  the hoisted whole-body pass cheap (a Sarwate one-shot pass measured
  **+10% on mono decode** — the serial per-byte table-latency chain is
  that expensive; slice-by-8 folds 8 bytes per chain step).
* Decoder session-cumulative (round start → round end, same Criterion
  cells): mono16 −16.8%, stereo16 −17.5%, stereo24 −18.2%, 6ch16
  −25.0%, format2 −18.1%. Encoder: mono16 −17.9%, stereo16 −22.5%,
  stereo24 −31.2%, 6ch16 −26.9%, format2 −23.2%. (Start and end
  captured under different host load; the per-commit A/B rows above
  are the controlled measurements.)

### Experiments measured and rejected (kept out of the tree)

Each was A/B'd the same way and landed inside noise (±1.5%) or
negative on this host:

* Fusing the LMS STEP-1 `qm` update into the STEP-2 dot product — the
  existing two-loop form already compiles to fused NEON
  `mla.4s`/`mul.4s` (verified by disassembly), so the source-level
  fusion only perturbed codegen (+0.5..+2.9%).
* Stack scratch buffer + `extend_from_slice` frame output in place of
  the zero-filled positional-write `Vec` (+0.4..+2.8%).
* A fused `read_codeword` (unary + terminator + tail in one cache
  inspection) reader primitive (−1.2..+1.3%).

The decode inner loop measures ≈ 34 cycles per (sample × channel) on
this host with the Rice, LMS (NEON-vectorised), Stage-B, and
decorrelation stages all inlined into one loop body — further
source-level micro-surgery is saturated; the next level would be
architecture-specific intrinsics, which the crate's
`#![forbid(unsafe_code)]` + portability posture deliberately avoids.

## Criterion sweep (post-optimisation)

### decode (`benches/decode.rs`)

One-shot `decode()` / `decode_with_password()` of a full stream.

| Cell | Median | Notes |
|---|---|---|
| mono/16/44k1/1s | 425 µs | 1 ch, no decorrelation |
| stereo/16/44k1/1s | 724 µs | + inverse decorrelation |
| stereo/24/48k/500ms | 388 µs | widest residuals |
| 6ch/16/48k/250ms | 522 µs | max channel count |
| stereo/17/44k1/500ms | 340 µs | odd packed width (byte_depth 3) |
| 3ch/20/48k/250ms | 278 µs | odd-N cascade, mid width |
| stereo/16/44k1/format2/1s | 724 µs | qm re-prime per frame ≈ free |

### encode (`benches/encode.rs`)

| Cell | Median | Notes |
|---|---|---|
| mono/16/44k1/1s | 610 µs | |
| stereo/16/44k1/1s | 1.011 ms | |
| stereo/24/48k/500ms | 494 µs | |
| 6ch/16/48k/250ms | 745 µs | |
| stereo/17/44k1/500ms | 442 µs | odd packed width |
| 3ch/20/48k/250ms | 357 µs | odd-N cascade |
| stereo/16/44k1/format2/1s | 1.005 ms | |

### roundtrip (`benches/roundtrip.rs`)

encode → decode back-to-back; throughput is over the PCM byte
footprint.

| Cell | Median | Throughput |
|---|---|---|
| mono/16/44k1/1s | 1.041 ms | 80.8 MiB/s |
| stereo/16/44k1/1s | 1.772 ms | 94.9 MiB/s |
| stereo/24/48k/500ms | 899 µs | 152.8 MiB/s |
| 6ch/16/48k/250ms | 1.322 ms | 103.9 MiB/s |
| stereo/16/44k1/format2/1s | 1.774 ms | 94.8 MiB/s |

### streaming + random access (`benches/streaming.rs`)

3-second stereo16 stream unless noted.

| Cell | Median | Notes |
|---|---|---|
| frame_iter full drain (3 s) | 2.281 ms | lazy == eager cost |
| decode_frame_at middle frame | 764 µs | one frame only |
| seek_to_sample (middle) | 1.15 ns | O(1) table walk |
| frame_iter_from middle | 1.461 ms | tail decode only |
| frame_iter cube: mono16 1 s | 413 µs | |
| frame_iter cube: stereo24 500 ms | 381 µs | |
| frame_iter cube: 6ch16 250 ms | 535 µs | |
| frame_iter cube: format2 1 s | 715 µs | |
| decode_frame_at cube: mono16 1 s | 418 µs | |
| decode_frame_at cube: stereo24 500 ms | 387 µs | |
| decode_frame_at cube: 6ch16 250 ms | 540 µs | |
| decode_frame_at cube: format2 1 s | 712 µs | |

### range / player sugar (`benches/range.rs`)

3-second stereo16 stream.

| Cell | Median |
|---|---|
| decode_from_sample (mid) | 1.471 ms |
| frame_iter_from_sample (mid) | 1.440 ms |
| decode_from_time (mid) | 1.472 ms |
| seek_to_time (mid) | 2.23 ns |
| decode_sample_range [25%, 75%) | 2.268 ms |
| frame_iter_sample_range [25%, 75%) | 2.264 ms |
| decode_time_range [25%, 75%) | 2.248 ms |
| decode_sample_range [0, total) | 2.288 ms |

(The half-open middle-range cells decode ~2 of the 3 frames plus
trim; the full-range cell is the eager decode through the range API.)

### framework demuxer (`benches/demuxer.rs`, `registry` feature)

| Cell | open | full drain | seek_to |
|---|---|---|---|
| mono16 44k1 1 s | 3.04 µs | 6.11 µs | 1.35 ns |
| stereo16 44k1 1 s | 6.53 µs | 8.37 µs | 1.34 ns |
| stereo24 48k 500 ms | 5.16 µs | 6.80 µs | 1.32 ns |
| 6ch16 48k 250 ms | 4.93 µs | 6.34 µs | 1.32 ns |
| stereo16 format2 1 s | 6.72 µs | 8.21 µs | 1.33 ns |

Demuxing is header + seek-table parsing and packet slicing only (no
entropy decode), hence microseconds; `seek_to` is a table lookup.

## Reproducing

```sh
# Full Criterion sweep (six harnesses):
cargo bench -p oxideav-tta

# Low-noise wall-clock A/B drivers (also bit-identity oracles — the
# printed FNV-1a hashes must not change across optimisation commits):
cargo run --release --example profile_decode -- 150
cargo run --release --example profile_encode -- 150

# Byte-exactness regression pins:
cargo test -p oxideav-tta --test bitexact_pins
```
