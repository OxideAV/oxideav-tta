# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Round-362 (fuzz coverage): a new `corrupt_decode` cargo-fuzz target
  that drives **valid encoder-produced streams that have been
  byte-corrupted past their 22-byte header** through every public
  decode entry point (`decode`, the `Decoder::new` /
  `decode_all` / `frame_iter` / `decode_frame_at` / `seek_to_sample`
  streaming surface, and `scan_trailers`), asserting panic-freedom.
  The existing `decode` / `streaming_decode` / `demuxer` targets feed
  raw fuzz bytes into `Decoder::new`, but the header CRC32 (`spec/01`
  §3.5) rejects essentially every mutated input before a frame body is
  read — so they rarely reach the seek-table walk, the per-frame
  trailing-CRC check (`spec/01` §4.3 / §5), the adaptive Rice decoder
  (`spec/05`), the Stage-A/B predictors (`spec/02`/`spec/03`), or the
  inverse decorrelation cascade (`spec/04`). The new target synthesises
  a structurally valid stream via the in-crate encoder (consistent
  header / seek-table / per-frame CRCs), then XOR-mutates the region at
  or after offset 22 from a fuzz-driven `(offset, mask)` script, so the
  header still parses and the mutations land squarely on the deep
  decode machinery. Seeded with three corpus inputs. Smoke-verified
  panic-free across 200 000 pseudo-random inputs before landing.

### Changed

- Round-362 (fuzz coverage): the `encode_roundtrip` cargo-fuzz target
  now drives the encoder/decoder bit-exact roundtrip across the **full
  `16..=24` `bits_per_sample` range** (`spec/01` §3.2) instead of only
  the byte-aligned `{16, 24}`. The non-multiple-of-8 widths `17..=23`
  share `byte_depth == 3` and the LMS `shift = 10` row with 24-bit; the
  whole pipeline (LMS / Stage-B / Rice / decorrelation) is
  `byte_depth`-keyed, so every i32 inside the `byte_depth`-wide signed
  storage range round-trips regardless of the declared
  `bits_per_sample`. The fuzz input now derives samples per
  `byte_depth` and selects the depth as `16 + (b1 % 9)`, so the fuzzer
  exercises the same packing/unpacking and predictor cascade the
  `17..=23`-bit roundtrip suite pins on hand-picked sines, but across
  the whole `(channels × bps × sample_rate × format × samples)` cube.
  The target compares raw `i32` slots, not packed bytes, so the
  `pack_pcm` convenience packer's `16/24`-only restriction (the source
  of the prior limitation) does not apply.

### Fixed

- Round-327: the encoder's adaptive-Rice tracker now clamps its three
  `k` increment branches at the same `MAX_K` (= 31) ceiling the
  decoder's `decode_one` already enforced (`src/rice.rs`). The decoder
  was capped in r124 (an arbitrary stream can chain enough high-mode
  escapes to drive `k` past 31, after which a binary-tail read would
  request more than 32 bits), but `src/encoder.rs`'s `rice_encode_one`
  left all three `state.k{0,1} += 1` branches uncapped. On a residual
  stream that drove a tracker to 31, the encoder would advance `k` to
  32+ while the decoder pinned it at 31 — the two trackers would then
  diverge and every subsequent residual's bit layout would be computed
  against a different `k` on each side, silently corrupting the
  lossless roundtrip from the first post-cap step onward (and tripping
  a `1u32 << 32` shift on the bias path in debug builds). Valid
  16/24-bit streams never reach `k >= ~14`, so this was latent, but the
  encoder/decoder lock-step is now total across the full `k` range.

### Added

- Round-354 (decorrelation conformance, captured-tape ground truth):
  a spec §04-anchored conformance suite that pins the channel-
  decorrelation transform against *externally captured* reference-tape
  values rather than only its own algebraic inverse. (1) All 31 rows of
  the `spec/04` §7.1 stereo pseudo-noise table — the most discriminating
  fixture in the corpus, with `dec_in[0]` spanning the full sign /
  parity matrix — are asserted bit-for-bit against `inverse`; previously
  only 2 of the 31 captured rows were tested. (2) The `spec/04` §6
  truncating-divide table (six operands) is pinned, with each
  odd-negative operand verified to diverge from arithmetic `>>1` by
  exactly 1 LSB, so the half-step's toward-zero rounding is asserted
  structurally and at the cascade output (row 11, `-8367`, lands
  `(12895, 4528)` under `/2` and would land `(12894, 4527)` under the
  wrong `>>1`).

- Round-354 (decorrelation conformance, N>2 worked cascade): the
  reference corpus has only stereo tapes (`spec/04` §7.3), so for N>2
  the spec's algebraic substitution is the ground truth. New tests pin
  every published intermediate of the §4.1 N=3 and N=4 encoder formulas
  and the §4.3 six-step 5.1 (`nch=6`) inverse walk against `forward` /
  `inverse` — not just the roundtrip endpoints — including an
  odd-negative anchor delta so the cascade's `/2` stays on the §6
  discriminating path. Adds §9 anti-pattern guards: odd channel counts
  (N=3, N=5) use the same uniform chain walk with no parity special case
  (#4); the `nch <= 1` branch is bounds-safe with no `buffer[-1]` index
  (#7); the cascade is per-sample stateless (#6, verified by replaying a
  probe across an intervening unrelated decorrelation). A dense
  sign-balanced LCG grid drives `forward(inverse(.))` through 4000
  vectors per channel count `2..=6` (20 000 total), exercising thousands
  of odd-negative `/2` cases on the live cascade rather than a single
  hand-picked operand.

- Round-354 (decorrelation conformance, end-to-end pipeline): trace-tape
  tests (gated on the `trace` feature) that prove the *full decode
  pipeline* runs exactly the isolated `inverse` cascade. A pseudo-noise
  stream is encoded then decoded with the tape on; every per-sample
  `DECORR_PRE` → `DECORR_POST` transition the live decoder emits is
  parsed back and asserted to equal `inverse(raw_per_channel)`, and
  `PCM_OUT.final_per_channel` is asserted to equal
  `DECORR_POST.decorrelated_per_channel` per `spec/04` §1 (PCM_OUT ==
  DECORR_POST for N>1). Run at stereo, 3-channel, and 6-channel — the
  last two close the §7.3 N>2 corpus gap against the live decoder rather
  than only the spec's algebraic substitution, on noise content that
  spans the full sign/parity matrix the §7.1 stereo table samples.

- Round-345 (encode + seek parity, seek-table structural invariant):
  three tests that re-parse the encoder's own bytes through the framing
  parser and pin the `spec/01` §4.2 seek-table contract directly,
  rather than only relying on the decoder tolerating the table. For
  every frame they assert: the §4.3 entry-bytes CRC validates; each
  `disk_size` equals the true on-disk gap to the next frame (or to
  end-of-stream for the last); `body_size() == disk_size - 4` with the
  trailing 4 bytes being the frame's own CRC over its body (§5.4);
  per-frame sample counts sum to `total_samples` with only the last
  frame allowed short (§4.1); and the frames tile the file exactly with
  no trailer. Covers mono-16, 4-channel-24, and the §4.1 `raw == 0`
  exact-multiple case where the last frame must be a *full* regular
  frame (verified both frames report the full `regular_frame_samples`).

- Round-345 (encode + seek parity, edge-case coverage): self-roundtrip
  tests for the bit-depth and channel-count shapes the prior suite left
  unexercised. (1) Bit depths `17..=23` (`spec/01` §3.2 derives
  `byte_depth = (bps + 7) / 8`, so each shares `byte_depth == 3` with
  24-bit and the LMS `shift = 10` table row) now have mono / stereo /
  multi-frame / full-scale-impulse round-trips at 17, 18, 19, 20, 21,
  22, 23 bps — the codec's i32 pipeline was already bit-depth agnostic
  past the byte-depth-keyed `shift`, and these pin it. (2) Channel
  counts `3`, `4`, `5` (the intermediate cascade the prior `{1, 2, 6}`
  fixtures skipped) now round-trip, including the odd-N walk `spec/04`
  §4.3 warns must NOT be parity-special-cased, with a 24-bit
  uncorrelated-noise N=3 case exercising the truncating-`/2` sign
  discipline (`spec/04` §6) and an N=5 multi-frame case pinning the
  per-frame reset. (3) Encoder-produced seek tables are verified valid
  for the decoder's random-access API (`decode_frame_at` /
  `seek_to_sample` / `frame_iter`, each bit-exact against eager
  `decode_all`) on multi-frame 3-channel, 5-channel, and 19-bit-mono
  streams — driving encode + seek toward parity with decode on shapes
  the prior seek suite (mono/stereo at 16/24) never covered. All paths
  pass bit-exactly; no implementation change was needed (the gaps were
  untested, not broken).

- Round-336 (saturated-crate depth mode, fuzz): a `registry_decode`
  `cargo-fuzz` target (`fuzz/fuzz_targets/registry_decode.rs`) that
  drives the framework `oxideav_core::Decoder` trait adapter — the
  surface `oxideav-meta::register_all` hands the host. The existing
  `decode` target hammers the free function `oxideav_tta::decode` and
  the round-299 `demuxer` target hammers the registered `Demuxer`, but
  neither runs the registered `Decoder` adapter (`TtaDecoder`): the
  `send_packet` → `receive_frame` → `flush` state machine, the channel /
  bits-per-sample sanity rails it checks against the demuxer-configured
  `CodecParameters`, and the `pcm_pack_for_format` repack that turns the
  decoder's interleaved `i32` samples into the `AudioFrame` byte layout
  (S16 → 2 bytes LE, S24 → 3 bytes LE). The target stitches both halves
  of the framework pipeline together exactly as a host does
  (`open_demuxer` → `first_decoder` from the stream's `CodecParameters`
  → `send_packet`/`receive_frame`/`flush`) and, on every frame the
  decoder produces, asserts the packed-plane length equals
  `samples * channels * bytes_per_sample` so the packer can neither drop
  nor duplicate a sample. 7.9M iterations against the seeded
  demuxer/decode corpora ran panic-free. Registered as the tenth fuzz
  bin in `fuzz/Cargo.toml`.
- Round-331: a continuous-stream chained Rice-decode test
  (`chained_stream_steps_0_1_2_matches_spec_7_1_7_3` in `src/rice.rs`).
  The existing spec/05 §7 hand-verifications each construct a fresh body
  and read exactly one codeword from a byte-aligned start, so the
  cross-codeword bit-cache carry — the production decode path where the
  second and subsequent `read_unary`/`read_bits` calls begin from a
  partially-consumed cache rather than an empty one — was never directly
  asserted. This test concatenates the steps 0/1/2 codewords (§7.1 →
  §7.2 → §7.3-step-2) into one 37-bit LSB-first body
  (`[0x00, 0xD8, 0x80, 0xDB, 0x00]`), bootstraps the tracker state once
  from the `RICE_K_INIT` constants `(10, 10, 0x4000, 0x4000)` per §4.2,
  and decodes all three codewords back-to-back through a single
  `BitReader`, threading each step's post-state unmodified into the next
  step's pre-state exactly as the §7.6 bulk-verification methodology
  prescribes. Codeword boundaries fall mid-byte at bit 11 (inside byte
  1, 5 carried cache bits) and at the byte-3 boundary (bit 24), so the
  cache-carry path is exercised twice. Residuals `(0, 1026, 1038)` and
  every post-state field are asserted bit-for-bit against the spec tape,
  and `bytes_consumed() == 5` pins the exact bit-budget.

- Round-327: two encoder/decoder lock-step property tests in
  `src/encoder.rs` (`rice_tracker_lockstep_drives_k_high` and
  `rice_tracker_lockstep_pseudo_random`). Both assert that, for every
  step of a residual stream, the encoder's post-step tracker state
  equals the decoder's post-step tracker state for the bytes the
  encoder produced, the decoded residual equals the input, and neither
  side's `k0`/`k1` escapes `[0, MAX_K]`. The first drives a doubling
  magnitude ramp that pushes `k` to 23/22 — well above the `k <= 13`
  any valid corpus reaches — exercising a long run of the increment
  branch the fix above clamps; the second sweeps 4096 deterministic
  pseudo-random residuals spanning the small / medium / large
  magnitude regimes so both the low-mode and high-mode tracker paths
  fire. Magnitudes are bounded to keep the post-bias `value` inside the
  `< 2^31` domain where the zigzag decode is unambiguous (spec §3.5 /
  anti-pattern §11).

- Round-323: two reference-tape hand-verification tests for spec/05
  §7.3 (the mid-frame Rice regime), `step_two_matches_spec_7_3` and
  `step_sixteen_matches_spec_7_3_first_k1_demotion`. The §7 worked-step
  walk previously had unit tests only at the boundary samples 0, 1, 17,
  and 33; the §7.3 block (steps 2..16) — the steady-state interior of
  the 2x adaptation window and the first `k1` demotion (10→9) of the
  canonical fixture — was unasserted. The step-16 test additionally
  pins that its post-state `(10, 9, 26219, 16229)` equals the pre-state
  `step_seventeen_matches_spec_7_4` assumes, closing the only break in
  the §7.1→§7.7 tracker chain. Both decode through the public
  `decode_one` and assert residual plus all four post-state trackers
  bit-for-bit against the spec tape. Test-only; no `src/` hot-path or
  public-API change.

- Round-319: `benches/demuxer.rs` Criterion harness for the framework
  raw-`.tta` demuxer (`src/registry.rs`). The demuxer parses the TTA1
  header + seek table at open, emits one self-contained mini-TTA1
  packet per audio frame, and answers `seek_to(pts)` in O(1) off the
  cumulative seek-table offsets (`spec/01` §4.1); it was fuzzed in
  r299 but had no benchmark. Three scenarios — `demuxer_open` (header
  + seek-table parse + open-time per-frame byte-window bounds check),
  `demuxer_drain` (open + `next_packet` to EOF, exercising the
  per-frame `build_single_frame_file` mini-file re-prefix), and
  `demuxer_seek_to` (the O(1) random-access lookup) — each run across
  the same mono16 / stereo16 / stereo24 / 6ch16 / format=2 parameter
  cube as `benches/decode.rs`. Streams are synthesised in-bench from a
  deterministic xorshift seed and encoded via the production `encode`
  / `encode_with_password`; no checked-in fixtures, no `docs/` reads.
  The demuxer is driven through the public
  `RuntimeContext` + `ContainerRegistry::open_demuxer` path. Adds a
  dev-only `oxideav-core = "0.1"` dependency (`required-features =
  ["registry"]`). No impl change.
- Round-313: three Rice-decoder hand-verification unit tests in
  `src/rice.rs` reproducing the spec/05 §7 reference-tape walk-through
  beyond the lone §7.1 step-0 case already covered. `step_one_matches_spec_7_2`
  drives the first high-mode step with the depth-1 escape bias taken
  from `k0_pre = 9` (anti-pattern §9.2) under STEP-A-before-STEP-B
  ordering (§9.3); `step_seventeen_matches_spec_7_4` is the canonical
  `k0 != k1` witness that the bias uses `k0` while the tail width uses
  `k1`; `step_thirtythree_matches_spec_7_5` exercises the first negative
  residual (even-magnitude zigzag sign branch, §3.5) and confirms the
  low-mode path leaves `sum1` untouched. Each builds the exact LSB-first
  body the tape consumes via a `push_codeword` / `pack_lsb_first` test
  helper and asserts the residual plus all four post-state trackers
  bit-for-bit against the spec. No impl change.

## [0.0.3](https://github.com/OxideAV/oxideav-tta/compare/v0.0.2...v0.0.3) - 2026-06-14

### Other

- r307 trailer_typed differential target over the §7 trailer typed-accessor lift
- tta r299: demuxer fuzz target + seek-table byte-window bounds fix
- r292 fuzz — password_streaming differential libFuzzer target
- Round-285 (profile-opt): Stage-A LMS hotspot, -18% decode wall time, bit-identical
- typed_header differential fuzz target on the spec/01 §3 header surface
- aggregate TypedStreamHeader validated view per spec/01 §3
- typed TrailerInfo sub-field accessors
- typed SeekPoint sub-field accessors
- typed FrameGeometry projection on StreamHeader
- drop release-plz.toml — use release-plz defaults across the workspace
- typed FrameDescriptor sub-field accessors
- typed TotalSamples accessor for StreamHeader::total_samples
- scrub pre-existing decorative external-impl attribution
- typed StreamHeader sub-field accessors
- range + duration-keyed player-API Criterion bench
- sample_range cargo-fuzz target — r209/r215/r219 player-API surface
- half-open sample/time range quartet on Decoder
- duration-keyed player-API sugar on Decoder
- decode_from_sample / frame_iter_from_sample player-API sugar
- public Decoder::new_with_password — format=2 streaming reach
- streaming bench parameter-cube extension

### Added

- Round-307 (depth mode: fuzz): new `trailer_typed` libFuzzer harness
  (`fuzz/fuzz_targets/trailer_typed.rs`) — the `spec/01` §7 analogue of
  the §3 `typed_header` target. The round-188 `scan_trailers` target
  drives the byte-level trailer scanner for panic-freedom but discards
  its `TrailerInfo`; the typed-accessor lift layered on that result
  (`TrailerInfo::id3v1_typed` / `apev2_typed` / `combined_byte_range`
  plus the `Id3v1Range` / `ApeV2Range` projection accessors) had no
  fuzz coverage. This target asserts the lift-totality contract the
  accessor docs promise: a `TrailerInfo` produced by the scanner
  (`scan_trailers` and the direct `detect_trailers` path with a
  fuzz-derived end-of-stream offset) ALWAYS lifts cleanly at its own
  `file_len` and carries the §7 structural invariants — ID3v1 = 128
  bytes anchored at file end, APEv2 >= the 32-byte footer and within
  the file, "APE immediately before ID3v1" adjacency when both are
  present, and `combined_byte_range` = the min-start / max-end hull. It
  separately drives the `from_raw` rejection side through
  hand-constructed `TrailerInfo` literals with fuzz-chosen raw
  `(start, len)` tuples — the only path to `Error::InvalidId3v1Range` /
  `Error::InvalidApeV2Range` per the accessor docs — pinning the typed
  lifts to their exact §7 predicate windows (`len == 128 &&
  start + len == file_len` for ID3v1; `len >= 32 && start + len <=
  file_len`, overflow-safe, for APEv2). Panic-free / typed-error
  contract, no reference-implementation oracle; no new crate bug. 400K
  randomized local execs (incl. spliced `TAG` / `APETAGEX` magics)
  clean.
- Round-299 (depth mode: fuzz): new `demuxer` libFuzzer harness
  (`fuzz/fuzz_targets/demuxer.rs`) over the framework raw-`.tta`
  demuxer reached through the `registry` feature — the only decode-
  adjacent surface not yet under a fuzzer. The other targets exercise
  the single-shot decode entry points (whose seek table is built
  internally); the demuxer instead consumes the on-wire seek table
  verbatim and slices the file buffer over each frame's
  `[file_offset, file_offset + disk_size)` window to assemble one
  self-contained mini-TTA1 packet per frame (`spec/01` §4.2 / §5.1).
  The target opens the demuxer via the public
  `ContainerRegistry::open_demuxer` path, drains `next_packet` to EOF,
  and re-drains after a battery of fuzz-derived `seek_to` probes.
  Panic-free / typed-error contract, no reference-implementation
  oracle. Seed corpus under `fuzz/corpus/demuxer/` (small mono16 /
  mono24 + three multi-frame streams). 11M mutated execs clean
  post-fix.
- Round-292 (depth mode: fuzz): new `password_streaming` libFuzzer
  harness (`fuzz/fuzz_targets/password_streaming.rs`) over the
  format=2 (password-protected) streaming + random-access decode
  surface reached through `Decoder::new_with_password` — the only
  password streaming path not yet under a fuzzer (the round-190
  `streaming_decode` target reaches the streaming battery only via
  the format=1 `Decoder::new` constructor). The target threads a
  fuzz-derived password through `new_with_password` and asserts the
  `spec/07` §3.5–§3.6 per-frame `qm[0..7]` re-prime is observed
  identically across the eager `decode_with_password` path and the
  lazy / random-access password surface: eager output equals the
  `frame_iter` concatenation bit-exactly (both must also agree on
  rejection), `decode_frame_at` matches the eager slice,
  `seek_to_sample` returns an in-range seek point under format=2
  geometry, and `frame_iter_from` matches the eager suffix. Seven
  seed inputs under `fuzz/corpus/password_streaming/`. 1.77M execs in
  121 s, zero crashes / panics / OOM (cov 561, ft 1555). No decode
  bug surfaced; no production code changed.

### Changed

- Round-285 (depth mode: profile-opt): profile-guided optimization of
  the ranked-#1 decode hotspot, the Stage-A LMS step (`spec/02` §4.2)
  — −18.2% total decode wall time on the five-scenario corpus
  (interleaved A/B medians, 604.4 ms → 494.4 ms), with decoded PCM
  AND encoded streams proven bit-identical via FNV-1a-64 hashes
  before/after (the encoder shares the Stage-A step). Three changes,
  all wrapping-exact: (1) `LmsState::step` is now the lean
  authoritative implementation and `step_traced` the snapshotting
  wrapper (trace builds only pay the snapshot cost); (2) the
  `spec/02` §4.5 dx-regeneration magnitudes are cached on the state
  at frame init (new private `LmsState::dx_mags`) instead of paying
  the lazy-table synchronisation check every sample × channel — the
  CSV-sourced table policy is preserved; (3) STEP 1's sign-gated qm
  update is branch-free (`qm[i] += sign(error) · dx[i]`,
  `sign ∈ {−1, 0, +1}`), removing a steady per-sample branch
  misprediction on noise-like residuals; the encoder's symmetric
  `lms_step_encode` gets the same rewrite. Post-change re-profile:
  the LMS step fell from rank #1 (~36% of in-decode samples) to
  rank #5 (−3.6×); the new #1 is `BitReader::read_unary`.

### Fixed

- Round-299 (found by the new `demuxer` fuzz target): the raw-`.tta`
  demuxer's `open_demuxer` accepted a malformed seek table whose
  per-frame `disk_size` (or cumulative `file_offset`) overran the file,
  then panicked on the out-of-bounds slice
  `all[file_offset..file_offset + disk_size]` inside
  `build_single_frame_file` at packet-emit time. `open_demuxer` now
  validates every frame's `[file_offset, file_offset + disk_size)` byte
  window against the file length at open time (`spec/01` §4.2 / §5.1: a
  frame block is `body + 4-byte CRC` and must lie within the on-disk
  stream region) and rejects an overrun with a typed
  `Error::InvalidData`, so `next_packet` is panic-free by construction.

### Added

- Round-285: `examples/profile_decode.rs` — sampling-profiler target
  + bit-identity oracle for optimization rounds. Synthesises the same
  deterministic xorshift corpus as the Criterion benches (mono16 /
  stereo16 / stereo24 / 6ch16 / format=2), decodes each stream N
  times, and prints FNV-1a-64 hashes of both the encoded bytes and
  the decoded interleaved PCM plus wall-clock timings; the hashes
  must be identical across any optimization commit.

- Round-276: `typed_header` cargo-fuzz target — a differential check
  between the two independent `spec/01` §3 validators: the byte-level
  parser behind `Decoder::new` (magic + CRC32 + inline field-range
  checks) and the round-240..262 typed-accessor lift
  (`StreamHeader::typed()` + the per-field `*_typed` accessors). The
  harness synthesizes a 22-byte on-wire header (valid magic + valid
  IEEE-802.3 CRC32 per `spec/01` §6, so field validation is the only
  rejection path) from attacker-chosen raw fields and asserts: (i)
  `typed()` is `Ok` iff every per-field lift is `Ok`, and on `Err`
  carries the FIRST per-field error in §3 table order; (ii) the
  byte-level parser surfaces exactly the same error variant for the
  same raw values, and on a field-valid header proceeds past field
  validation (format=2 → `PasswordRequired`, with the
  password-lifted constructor reaching the seek table; format=1 →
  `Truncated` at the absent seek table); (iii) every derived
  projection on the aggregate view agrees with its raw-header
  sibling (`to_header()` round-trip, `byte_depth`,
  `regular_frame_samples`, 3-way `total_duration` agreement,
  `pcm_byte_len` §3.4 product rule) plus the full `FrameGeometry`
  §4.1 invariant set (`1 ≤ last ≤ regular`, closed-form
  `(fc−1)·regular + last == total_samples`, `frame_samples_at` at
  first/interior/last/past-end, `seek_table_size_bytes == 4·fc + 4`,
  exact-multiple predicate, empty-stream degradation); (iv) the
  `FrameDescriptor` lifts (`disk_size_typed` / `sample_count_typed`)
  and `SeekPoint` lifts (`frame_index_typed` / `sample_offset_typed`)
  accept exactly their documented windows against a geometry derived
  from in-range fields, with round-trip / `is_last` /
  `is_frame_boundary` / `interleaved_skip` agreement; (v) the
  infallible accessors stay total (no panic) for ANY raw field
  combination, including the `sample_rate == 0` degenerate. Every
  iteration runs both a raw (mostly out-of-range) pass and a
  folded-into-valid-windows pass so the typed-Ok projection set is
  exercised on every input. Three seeds under
  `fuzz/corpus/typed_header/` cover the canonical stereo16-44.1k
  shape, the `(u32::MAX, 6ch, 24-bit, 0x7FFFFF Hz)` format=2 upper
  envelope, and an all-fields-invalid header. 34.5M execs in 90 s
  clean.

- Round-262: aggregate `TypedStreamHeader` validated view per
  `spec/01` §3 — the capstone of the round-240 → round-261
  typed-accessor arc. New `StreamHeader::typed()` lifts all five §3
  meta-data sub-fields into the existing validated newtypes
  (`Format` / `ChannelCount` / `BitsPerSample` / `SampleRate` /
  `TotalSamples`) behind a single `Result`, in the header's on-wire
  field order (`format`, `channels`, `bits_per_sample`,
  `sample_rate` per the §3 table — the same order the byte-level
  parser checks), so an ad-hoc `StreamHeader` literal with several
  out-of-range fields surfaces the same first error variant
  `parse_stream_header_with_crc` would have produced for the same
  raw values. Because every field of the aggregate is in-range by
  construction, the derived projections are total:
  `requires_password()` (`spec/07` §3 gate), `byte_depth()` (§3.2),
  `regular_frame_samples()` (§4.1), `frame_geometry()` (typed §4.1
  — delegates to the round-251 projection on the round-tripped
  header, single source of arithmetic), `total_duration()`
  (§3.3/§3.4 nanosecond-grain integer arithmetic), and the new
  `pcm_byte_len()` — the `total_samples × channels × byte_depth`
  raw-PCM-buffer size per `spec/01` §3.4's product rule, computed
  in `u64` because the product overflows `u32` at the
  `(total_samples = u32::MAX, channels = 6, byte_depth = 3)`
  envelope. `to_header()` round-trips losslessly back to the raw
  on-wire data model. No new `Error` variants — the aggregate
  reuses the per-field rejection variants, so the
  `tests/malformed_props.rs` exhaustive match is unchanged. Five
  new unit tests in `header::tests` pin the field-by-field
  agreement with the individual round-240/243 lifts plus every
  derived projection, the rejection-order chain against the
  byte-level parser on a five-case multi-invalid grid (including
  the format=2-accepted branch), the `pcm_byte_len` product rule at
  the `spec/01` §8.1 / §8.2 fixture shapes + the empty stream + the
  u64-widening envelope canary, the format=2 `requires_password`
  gate on a parsed header, and the empty-stream degradation
  (`is_empty` propagation, 4-byte seek table per §4.4, zero
  duration / zero PCM bytes). One new integration test in
  `roundtrip_tests` (`typed_stream_header_matches_parsed_stream`)
  walks the same three-shape encoded-stream grid as the round-246 /
  round-251 / round-254 cross-checks and confirms the aggregate
  view agrees with the raw fields it lifts, the decoder's own
  `total_duration` / frame-table walk, and the §3.4 product rule
  against the actual interleaved input PCM length. Lib tests: 188
  (default features) / 193 (all-features) / 179
  (no-default-features). Integration tests unchanged at 9.

- Round-261: typed `TrailerInfo` sub-field accessors per `spec/01` §7.
  Lifts the two `pub` `Option<(usize, usize)>` byte-tuple fields on
  the existing round-4 `TrailerInfo` (`id3v1`, `apev2`) into validated
  `Id3v1Range` / `ApeV2Range` newtypes so a caller hand-constructing a
  trailer-info literal (e.g. a host-application synthetic fixture, or
  a different-source detector that bypasses `detect_trailers`) gets
  the same `spec/01` §7 invariants the parser enforces at construction:
  ID3v1 is exactly 128 bytes anchored at file end
  (`start + len == file_len`) per the spec §7 "file's last 128 bytes
  start with `'TAG'`" detection rule, and APEv2 is at least the
  32-byte footer minimum per the APE tags header spec referenced from
  spec §7, with `start + len <= file_len` bounding. New
  `Id3v1Range` newtype carries `start()`, `len()`, `is_empty()` (always
  `false` by the spec §7 fixed-128 invariant), `end()`, `byte_range()`
  (half-open `Range<usize>` ready for direct buffer slicing), and
  `is_at_file_end(file_len)`; new `ApeV2Range` newtype carries the
  same shape plus `header_and_body_size()` (= `len - 32` per spec §7,
  isolating the body + optional 32-byte header from the fixed footer)
  and the `FOOTER_SIZE` / `HEADER_SIZE` associated constants per the
  APE tags header spec. Two new `Error` variants —
  `InvalidId3v1Range(usize, usize)` and `InvalidApeV2Range(usize, usize)`
  — surface the rejection at lift time so an ad-hoc literal gets the
  same `spec/01` §7 discipline the parser enforces; both are slotted
  into the `tests/malformed_props.rs` exhaustive panic-when-leaked
  match because they surface only from typed-accessor invocation,
  never from `decode()`. New `TrailerInfo::id3v1_typed(file_len)` /
  `TrailerInfo::apev2_typed(file_len)` `Result<Option<...>>`-returning
  lifting accessors and `TrailerInfo::combined_byte_range()` (smallest
  contiguous `(start, len)` window covering every detected trailer —
  the bytes a host application slices to preserve out-of-stream
  metadata on round-trip re-encode per spec §7) complete the surface;
  the raw fields stay public so the addition is purely additive.
  Eighteen new unit tests in `trailers::tests` pin the boundary cases
  (anchored-at-file-end / not-anchored / past-file-end / overflow /
  wrong-length-grid for ID3v1; footer-only / body+footer /
  header+body+footer / not-anchored / sub-footer-grid / past-file-end
  / overflow for APEv2; parser-output cross-check on id3v1-only /
  apev2-only / both; combined window over empty / single / both;
  hand-built literal rejection cycle), plus one new integration test
  in `roundtrip_tests` (`scan_trailers_typed_accessors_match_parser_output`)
  that walks `scan_trailers` over a real encoded TTA1 stream
  augmented with both an APEv2 footer-only region and an ID3v1
  trailer and confirms the typed views agree with the parser output
  bit for bit, the half-open byte ranges yield the right magic bytes
  out of the buffer, and decode is still bit-exact end-to-end past
  the trailers per spec/01 §7. Lib tests: 182 (default features) /
  187 (all-features) / 173 (no-default-features). Integration tests
  unchanged at 9.

- Round-254: typed `SeekPoint` sub-field accessors per `spec/01` §4.1
  / §4.2. Lifts the two `pub` fields on the existing round-187
  `SeekPoint` (`frame_index: usize`, `sample_offset_in_frame: u32`)
  into validated newtypes so a caller hand-constructing a seek point
  against an ad-hoc seek table gets the same window discipline
  `Decoder::seek_to_sample` enforces at construction. New
  `FrameIndex` newtype validated against the stream's `frame_count`
  carries `index()` and `is_last(frame_count)` (matching the
  parser's per-`FrameDescriptor.is_last` discrimination per
  `spec/01` §4.1); new `InFrameSampleOffset` newtype validated
  against the regular per-frame sample count derived per `spec/01`
  §4.1 (`floor(sample_rate * 256 / 245)`) carries `offset()`,
  `is_frame_boundary()` (the `offset == 0` "no prefix skip needed"
  player-API predicate), and `interleaved_skip(channels)` (the
  prefix-entry count `Decoder::frame_iter_from_sample` discards
  from the `frame_index` frame's PCM buffer per `spec/01` §4.1 /
  §3.2). Two new `Error` variants — `InvalidFrameIndex(usize)` and
  `InvalidInFrameSampleOffset(u32)` — surface the rejection at lift
  time so an ad-hoc literal gets the same discipline the
  random-access path enforces; both are slotted into the
  `tests/malformed_props.rs` exhaustive panic-when-leaked match
  because they surface only from typed-accessor invocation, never
  from `decode()`. New `SeekPoint::frame_index_typed(frame_count)`
  and `SeekPoint::sample_offset_typed(regular_frame_samples)`
  `Result`-returning lifting accessors complete the surface; the
  raw fields stay public so the addition is purely additive. Five
  new unit tests in `decoder::seek_point_typed_tests` pin the
  boundary cases (empty-stream / single-frame / three-frame /
  upper-end `usize`, the regular-frame ceiling rejection at
  `46_080`, the `interleaved_skip` projection across mono / stereo
  / 6ch / defensive-zero, the typed-accessor vs raw-field
  cross-check, and the frame-boundary predicate). One new
  integration test in `roundtrip_tests`
  (`seek_point_typed_accessors_match_parsed_stream`) walks the same
  three-shape encoded-stream grid the round-246 / round-251 tests
  use and confirms cross-API agreement on every
  `Decoder::seek_to_sample` probe: first sample, last sample, every
  frame boundary, every mid-frame offset. The typed `frame_index`
  lift agrees with the raw field and reports `is_last` consistent
  with the seek table's last-frame discrimination; the typed
  `sample_offset` lift agrees with the raw field and
  `is_frame_boundary` matches the source-side
  `sample_offset_in_frame == 0` gate; and the `interleaved_skip`
  projection equals the `(offset * channels)` arithmetic
  `frame_iter_from_sample` uses internally. Lib tests: 164 (default
  features) / 169 (all-features) / 155 (no-default-features).
  Integration tests unchanged at 9.

- Round-251: typed projection of the per-stream frame geometry per
  `spec/01` §4.1 — the `(frame_count, regular_frame_samples,
  last_frame_samples)` triple that `StreamHeader::frame_geometry`
  has been returning as a bare `(u32, u32)` tuple since round 1.
  New public newtype `FrameGeometry` threads the triple together so
  callers do not have to re-derive `regular_frame_samples` separately
  when they already have the geometry in hand: `frame_count()`,
  `regular_frame_samples()`, `last_frame_samples()`, `is_empty()`
  (short-circuit for `total_samples == 0` per `spec/01` §3.4),
  `is_exact_multiple()` (predicate matching `spec/01` §4.1's
  exact-multiple branch, false for the empty-stream case),
  `frame_samples_at(frame_index)` (per-frame sample-count lookup
  matching the per-`FrameDescriptor.sample_count` assignment made
  by `parse_seek_table`, `None` for past-end indices),
  `seek_table_size_bytes()` (`4 * frame_count + 4` per `spec/01`
  §4.2 — `4` for an empty stream per `spec/01` §4.4), and
  `total_samples()` (back-derivation to the source
  `StreamHeader::total_samples` field per `spec/01` §3.4 in `u64`
  arithmetic so it stays overflow-free across the full
  `(total_samples = u32::MAX, sample_rate = MAX_SAMPLE_RATE)`
  envelope). `StreamHeader` gains a new `frame_geometry_typed()`
  accessor that projects the existing bare-tuple `frame_geometry()`
  return into the typed newtype — the bare tuple is kept for
  backward compatibility (every existing caller in `src/` and
  `benches/` continues to destructure `(frame_count, last_samples)`
  verbatim; the typed projection is purely additive). Five new unit
  tests in `header::tests` pin the boundary cases: the three-shape
  round-trip (`(1, 44_100)` single-frame, `(3, 18_090)` three-frame,
  `(2, 46_080)` exact-multiple) walking every accessor; the
  empty-stream case at `total_samples = 0` confirming `is_empty`,
  the `4`-byte seek-table size from `spec/01` §4.4, and the `None`
  past-end `frame_samples_at`; the bare-tuple-vs-typed-projection
  agreement across a six-shape parameter grid (including the empty
  stream + the 24-bit / multi-channel cases) confirming the typed
  accessor is sugar over the existing `frame_geometry` return; the
  `(total_samples = u32::MAX, sample_rate = MAX_SAMPLE_RATE)`
  envelope canary against a future regression that drops the `u64`
  back-derivation widening; and an end-to-end parsed-header
  round-trip confirming the typed projection's `seek_table_size_bytes`
  matches the `spec/01` §4.2 closed form and `frame_samples_at`
  matches the parser's per-frame `is_last` discrimination. One new
  integration test in `roundtrip_tests` confirms cross-API agreement
  on a real encoded multi-frame stream: the same three independent
  shapes from the round-246 cross-check (mono 16-bit @ 44.1k / 2.5
  s — three frames, stereo 16-bit @ 48k / 2 s — exact-multiple
  two frames, mono 24-bit @ 44.1k / 1 s — one frame), with the
  typed projection's `frame_count` / `last_frame_samples` agreeing
  with the bare-tuple return, the projection's
  `regular_frame_samples` agreeing with
  `StreamHeader::regular_frame_samples`, the projection's
  `total_samples` round-tripping back to the header field, the
  projection's `seek_table_size_bytes` matching `4 * frame_count +
  4`, the `is_exact_multiple` predicate matching the
  `total_samples mod regular == 0` source-side gate, and the
  per-frame `frame_samples_at` agreeing with every parsed
  `FrameDescriptor::sample_count`. Lib tests: 158 (default
  features); integration tests unchanged at 9.

- Round-246: typed accessors for the two constrained `FrameDescriptor`
  sub-fields per `spec/01` §4.2 / §5.1 / §5.5. New public newtypes
  `FrameByteLength` (validated `>= 4` per `spec/01` §5.1 — the minimum
  on-disk frame footprint that still has room for the trailing
  per-frame CRC32; carries `total_size()` and `body_size()` derived as
  `total_size - 4`, where the subtraction is safe by construction
  rather than `saturating_sub` as on the raw `FrameDescriptor::body_size`)
  and `FrameSampleCount` (validated `>= 1` per `spec/01` §4.1 / §5.5 —
  every parser-produced descriptor describes at least one sample, with
  the empty-stream `total_samples = 0` case producing zero descriptors
  instead; carries `count()` and `is_within_regular_bound(regular)`,
  the `<= floor(sample_rate * 256 / 245)` regular-frame ceiling gate of
  `spec/01` §4.1 / §5.5). Two new `Error` variants —
  `InvalidFrameByteLength(u32)` and `InvalidFrameSampleCount(u32)` —
  surface the rejection at lift time so an ad-hoc `FrameDescriptor`
  literal (e.g. an encode-side fixture) gets the same discipline the
  per-frame decoder hot path enforces (`decode_frame` already rejects
  `disk_size < 4` with `Error::Truncated`). `FrameDescriptor` gains
  `disk_size_typed()` and `sample_count_typed()` (each returning a
  `Result`) — the raw `u32` fields are kept for backward compatibility;
  the typed accessors are purely additive. Five new unit tests pin the
  boundary cases at each end of every range (`FrameByteLength` at `0` /
  `1` / `3` / `4` / `22_189` / `u32::MAX`; `FrameSampleCount` at `0` /
  `1` / `46_080` / `u32::MAX`; the regular-bound gate at the boundary
  `46_080` versus `46_081` derived for `sample_rate = 44_100`), the
  ad-hoc `FrameDescriptor` round-trip on the canonical-fixture
  `(disk_size = 22_189, sample_count = 44_100)` shape from
  `spec/01` §8.1, and the end-to-end agreement on a parsed three-frame
  seek table for a 2.5 s @ 44.1 kHz stream. One new integration test in
  `roundtrip_tests` confirms cross-API agreement on a real encoded
  multi-frame stream: for every shape in a three-case parameter grid
  (mono 16-bit @ 44.1k 2.5 s producing three frames `(regular, regular,
  shorter-last)`, stereo 16-bit @ 48k 2 s producing two regular frames
  via the exact-multiple case, mono 24-bit @ 44.1k 1 s producing a
  single shorter-last frame), every descriptor's typed `disk_size_typed`
  / `sample_count_typed` lift agrees bit-for-bit with the raw field it
  lifts and every frame's sample count satisfies the regular-bound gate
  with the expected per-frame split from `header.frame_geometry()`.

- Round-243: typed accessor for the `StreamHeader::total_samples`
  sub-field per `spec/01` §3.4 — the remaining raw `u32` after the
  round-240 four-sub-field lift. New public newtype `TotalSamples`
  (`from_raw` is infallible because every `u32` is structurally legal
  per spec §3.4 — zero is permitted as a valid empty-stream marker),
  carrying `count()`, `is_empty()`, and `duration_at(sample_rate)` that
  computes the playback length in `core::time::Duration` using the
  same nanosecond-grain integer arithmetic as
  `Decoder::total_duration` (`floor(remainder * 1e9 / sample_rate)`
  with a `u128`-widened intermediate to avoid overflow at the upper
  end of the `(total_samples = u32::MAX, sample_rate = 0x7FFFFF)`
  envelope). `StreamHeader` gains `total_samples_typed()` (the
  infallible projection) plus a `total_duration()` convenience that
  threads through the typed accessor — the latter is the
  header-side mirror of `Decoder::total_duration`, reachable without
  constructing a full `Decoder` (e.g. for a player UI that wants to
  display the stream duration before committing to a decode). Five
  new unit tests pin the boundary cases (`TotalSamples` at `0` /
  `44_100` / `u32::MAX`; `duration_at` at exact 1 s / zero samples /
  zero rate / 0.5 s sub-second precision; the upper-bound envelope
  `(total_samples = u32::MAX, sample_rate = MAX_SAMPLE_RATE)` against
  a future regression that drops the `u128` widening; the
  parsed-header round-trip on `(1, 2, 16, 48_000, 96_000)` confirming
  the typed accessor count matches the raw field and the convenience
  `total_duration` matches the typed `duration_at(sample_rate)` call;
  the zero-payload header at `total_samples = 0` confirming both the
  typed accessor's `is_empty` flag and the zero duration round-trip).
  One new integration test in `roundtrip_tests` confirms cross-API
  agreement: for every shape in a six-case parameter grid (exact 1 s
  / 2.5 s / 3 s at the typical rates, single-sample at 192 kHz, 1 s
  plus one sample at 44.1 kHz, and an empty-stream literal), the
  header-level `StreamHeader::total_duration`,
  the typed `TotalSamples::duration_at(sample_rate)`, and the
  decoder-level `Decoder::total_duration` agree bit-for-bit. Raw
  `u32` field on `StreamHeader` is kept for backward compatibility;
  the typed accessor is purely additive.

- Round-240: typed accessors for the four constrained `StreamHeader`
  sub-fields per `spec/01` §3.1 / §3.2 / §3.3. New public items
  `Format` (non-exhaustive enum with `Simple` / `Encrypted` variants
  + `from_raw` / `as_raw` / `requires_password`), `BitsPerSample`
  (newtype validated `16..=24` + `bits` / `byte_depth`), `ChannelCount`
  (newtype validated `1..=6` + `count` / `is_multichannel`), and
  `SampleRate` (newtype validated `1..=0x7FFFFF` + `hz` /
  `regular_frame_samples`). `StreamHeader` gains four matching
  accessors (`format_typed` / `bits_per_sample_typed` /
  `channel_count_typed` / `sample_rate_typed`), each returning a
  `Result` so an ad-hoc `StreamHeader` literal built by a caller
  (e.g. a round-trip test) gets the same validation discipline as a
  parser-emitted header rather than silently propagating an
  out-of-range raw value. The raw `u16` / `u32` fields on
  `StreamHeader` are kept for backward compatibility; the typed
  accessors are additive. Five new unit tests in `header::tests` pin
  the boundary cases at each end of every range (`Format::from_raw`
  rejection on `0` / `3` / `255`; `BitsPerSample` at 15/16/24/25/32;
  `ChannelCount` at 0/1/2/6/7/255; `SampleRate` at 0 / 1 / 44 100 /
  `MAX_SAMPLE_RATE` / `MAX_SAMPLE_RATE + 1` / `u32::MAX`), the
  `byte_depth` derivation for each `bits_per_sample` in scope, the
  `is_multichannel` gate at `nch == 1` / `nch == 2`, the
  `regular_frame_samples` 64-bit-widening canary at
  `sample_rate = MAX_SAMPLE_RATE` (against a future regression that
  drops the `(... as u64) * 256 / 245` widening per `spec/01` §4.1),
  and a "parsed header → typed accessor matches raw field" agreement
  test that confirms a successfully-parsed `(1, 2, 16, 44100, 88200)`
  stereo header round-trips bit-for-bit through every accessor.

- Round-234: new `range` Criterion bench harness under
  `benches/range.rs` covering the round-209 / round-215 / round-219
  player-API range surface on `Decoder` —
  `decode_from_sample` / `frame_iter_from_sample` (r209),
  `decode_from_time` / `seek_to_time` / `total_duration` (r215),
  and the half-open `[start, end)` range quartet
  `decode_sample_range` / `frame_iter_sample_range` /
  `decode_time_range` / `frame_iter_time_range` (r219). Eleven
  scenarios run against the same 3 s stereo 16-bit 44.1 kHz anchor
  the `streaming.rs` harness uses, so the per-API cost diffs
  against the existing baselines: tail-from-mid-stream cost (eager
  vs lazy via `range_decode_from_sample_mid` /
  `range_frame_iter_from_sample_mid`); duration-keyed surface cost
  (`range_decode_from_time_mid` / `range_seek_to_time_mid`); the
  half-open range quartet across the middle 50 % of the stream
  (`range_decode_sample_range_middle_half` /
  `range_frame_iter_sample_range_middle_half` /
  `range_decode_time_range_middle_half`); the full-stream boundary
  case `[0, total_samples)` (`range_decode_sample_range_full`); the
  empty-range short-circuit sentinel
  (`range_decode_sample_range_empty`); the format=2 reach at the
  same parameter point (`range_decode_sample_range_format2_middle_half`)
  so the per-frame qm re-prime (`spec/07` §3.5 / §3.6) cost is
  comparable against the format=1 anchor; and the
  `range_total_duration` sub-nanosecond sentinel against
  accidentally promoting the integer-arithmetic helper to a heavier
  computation. PCM is synthesised via the same xorshift-driven
  `build_pcm` helper the four sibling benches use so the workload
  is identical across all five harnesses, and the compressed stream
  is built once per bench via the production `encode` /
  `encode_with_password` entry points (no checked-in fixtures). Run
  with `cargo bench -p oxideav-tta --bench range`.

- Round-226: new `sample_range` cargo-fuzz target under
  `fuzz/fuzz_targets/sample_range.rs` that drives the round-209 /
  round-215 / round-219 player-API sugar on `Decoder` —
  `decode_from_sample`, `frame_iter_from_sample`,
  `decode_sample_range(start, end)`, `frame_iter_sample_range`,
  `decode_time_range`, and `frame_iter_time_range`. Folds
  attacker-chosen `(start, end)` `u64` seeds against
  `total_samples + 1` (admitting `start == total` and
  `end == total` per the half-open contract) and routes the pair
  through a 4-mode bias byte covering the canonical
  `start ≤ end` agreement branch, the swapped `start > end`
  rejection branch, and the empty-range boundary cases
  `(0, 0)` / `(total, total)`. The harness asserts the round-209 /
  r215 / r219 invariants on every fuzz-constructed stream: (i)
  `decode_from_sample(s)` equals `decode_all()[s × channels..]`;
  (ii) `decode_sample_range(s, e)` equals
  `decode_all()[s × channels .. e × channels]` bit-exactly; (iii)
  lazy `frame_iter_*` concatenations equal their eager siblings
  on both the sample- and duration-keyed pairs; (iv) boundary
  collapses `(0, total) ⇔ decode_all`,
  `(s, total) ⇔ decode_from_sample(s)`,
  `(s, s) ⇔ Ok(vec![])` for `s ∈ [0, total]`; (v) `start > end`
  and `end > total_samples` surface
  `Error::SampleIndexOutOfRange`. The `decode_time_range(ZERO,
  total_duration())` boundary is intentionally NOT asserted equal
  to `decode_all` — the duration round-trip
  `samples → Duration → samples` floor-arithmetic is lossy by one
  sample when `total_samples * 1e9 / sample_rate` lacks an exact
  integer-nanosecond representation, and the pre-existing
  `roundtrip_tests::decode_time_range_full_duration_equals_decode_all`
  hand-fixture only covers the rate-aligned case (44 100 samples
  at 44 100 Hz). Seed corpus under `fuzz/corpus/sample_range/`:
  20 small fixtures (mono16 ramp / mono16 pw / mono24 / stereo16 /
  tiny-silent replicated across the four range-mode prefixes) +
  4 multi-frame fixtures (mono16-3s, stereo16-3s, stereo24-2.5s,
  stereo16-pw-3s) at the canonical mode for the cross-API
  agreement path. Bin block registered in `fuzz/Cargo.toml` so
  the `.github/workflows/fuzz.yml` shim discovers it
  automatically. 200K iters clean from a cold start under
  `cargo +nightly fuzz run sample_range`; the seeded corpus
  achieves 7K+ iters per 60 s (per-iteration cost is heavy
  because every input forces multiple eager `decode_all` passes
  for the agreement assertion).

- Round-219: half-open `[start, end)` sample- and duration-keyed range
  quartet on `Decoder`, extending the round-209 / round-215 player-API
  surface from "seek and play the tail" to "seek and play a bounded
  segment". `Decoder::decode_sample_range(start, end)` returns the
  interleaved `i32` PCM for per-channel samples `[start, end)`;
  `Decoder::frame_iter_sample_range(start, end)` is the lazy analogue
  returning a new `SampleRangeIter`; `Decoder::decode_time_range(start,
  end)` / `Decoder::frame_iter_time_range(start, end)` are the
  duration-keyed analogues that pre-floor both endpoints via the
  existing `floor(time_ns × sample_rate / 1e9)` conversion. The
  trailing frame is trimmed in-place via `Vec::truncate` so the
  returned PCM is exactly `(end - start) × channels` interleaved
  entries; frames past `end` are never decoded. The half-open
  convention allows `end == total_samples` (equivalent to
  `decode_from_sample(start)`) and `start == end` (returns
  `Ok(vec![])` without touching the bitstream). `start > end` and
  `end > total_samples` surface `Error::SampleIndexOutOfRange`. Format=2
  (password-protected) reach is automatic through
  `Decoder::new_with_password` — the per-frame qm re-prime discipline
  of `spec/07` §3.5–§3.6 propagates unchanged through the new surface.
  Sixteen new tests in `roundtrip_tests` lock the invariants:
  - `decode_sample_range_matches_eager_slice_{mono16,stereo16,stereo24,
    6ch16}_format1` — bit-exact agreement with
    `decode_all()[start * nch .. end * nch]` across the parameter
    cube on format=1.
  - `decode_sample_range_matches_eager_slice_format2_password_stereo16`
    — same on format=2 password-protected streams.
  - `decode_sample_range_full_stream_equals_decode_all` —
    `decode_sample_range(0, total)` equals `decode_all`.
  - `decode_sample_range_to_total_equals_decode_from_sample` —
    `decode_sample_range(s, total)` equals `decode_from_sample(s)`
    for every starting `s`.
  - `decode_sample_range_empty_at_every_boundary` — `s == e` returns
    `Ok(vec![])` for `s ∈ [0, total_samples]`, including the upper
    boundary `s == total_samples` that the half-open convention
    permits.
  - `frame_iter_sample_range_concat_matches_decode_sample_range` —
    the lazy concatenation equals the eager materialisation.
  - `frame_iter_sample_range_trailing_trim_lands_at_boundary` — when
    `end` falls mid-frame, the final yielded `Vec<i32>` has exactly
    `(end - frame_start) × channels` entries, not the full
    regular-frame width.
  - `decode_time_range_matches_decode_sample_range_at_endpoints` —
    duration- and sample-keyed surfaces agree at exact-round-trip
    `(sample_index, duration)` boundaries (rate-aligned indices).
  - `decode_time_range_full_duration_equals_decode_all` —
    `decode_time_range(Duration::ZERO, total_duration)` equals
    `decode_all`.
  - `frame_iter_time_range_concat_matches_decode_time_range` — lazy
    duration-keyed iterator equals eager duration-keyed materialisation.
  - `decode_sample_range_rejects_start_greater_than_end` and
    `decode_sample_range_rejects_end_past_total_samples` — the typed
    error shape on the sample-keyed surface.
  - `decode_time_range_rejects_end_past_total_duration` — the typed
    error shape on the duration-keyed surface (start > end and end
    floored past `total_samples`).
  - `decode_sample_range_format2_password_seek_and_clip_bit_exact` —
    format=2 (password-protected) bounded segment under per-frame qm
    re-prime discipline.

- Round-215: duration-keyed player-API quartet on top of the round-209
  sample-keyed sugar. `Decoder::total_duration()` returns the stream's
  per-channel playback length as a `core::time::Duration` (integer
  arithmetic at nanosecond granularity from `(total_samples,
  sample_rate)` per `spec/01` §3.3 / §3.4 — no floating-point);
  `Decoder::seek_to_time(d)` resolves a clock `Duration` from stream
  start to the same `SeekPoint` that `seek_to_sample(s)` would return
  for the corresponding floor-rounded sample index;
  `Decoder::frame_iter_from_time(d)` returns the trace-silent
  `SampleSkipIter` from `frame_iter_from_sample` against the same
  resolved index; `Decoder::decode_from_time(d)` is the eager analogue
  (interleaved `i32` tail). The `Duration → sample_index` conversion
  is `floor(time_ns × sample_rate / 1e9)` widened to `u128` so the
  multiplication is overflow-free for the full
  `(sample_rate ≤ 0x7FFFFF, Duration ≤ Duration::MAX)` envelope; the
  floor is monotone non-decreasing and never overshoots the true
  sample boundary. Out-of-range times surface
  `Error::SampleIndexOutOfRange` (no panic on `Duration::MAX`). Nine
  integration tests in `roundtrip_tests` lock the public surface:
  - `total_duration_matches_total_samples_over_sample_rate` —
    `total_samples / sample_rate` arithmetic at sample-aligned and
    one-sample-past endpoints (`110 250 / 44 100 = 2.5 s` exact;
    `44 101 / 44 100 = 1 s + 22 675 ns`).
  - `seek_to_time_zero_lands_at_first_sample` — `Duration::ZERO`
    resolves to `SeekPoint { frame_index: 0, sample_offset_in_frame: 0 }`.
  - `seek_to_time_matches_seek_to_sample_at_equivalent_time` —
    millisecond timestamps at sample-rate-aligned boundaries resolve
    to the same SeekPoint as the corresponding `seek_to_sample`
    call.
  - `seek_to_time_at_total_duration_rejects` — `time == total_duration`
    and `time >= total_duration + 1 s` and `Duration::MAX` all
    surface `Error::SampleIndexOutOfRange` without panicking.
  - `decode_from_time_matches_decode_from_sample_bit_exact` —
    multi-frame format=1 mono `decode_from_time(ms)` equals
    `decode_from_sample(ms × sample_rate / 1000)` bit-exactly across
    a sweep.
  - `frame_iter_from_time_concat_matches_eager_tail` — lazy iterator's
    concatenation equals `decode_all`'s tail from the resolved
    sample cursor.
  - `frame_iter_from_time_rejects_past_end` —
    `frame_iter_from_time(total_duration)` and
    `decode_from_time(total_duration)` both error.
  - `time_apis_format2_seek_and_resume_bit_exact` — format=2
    (password-protected) eager + lazy duration-keyed seek-and-resume
    match `decode_with_password`'s tail; the per-frame qm re-prime
    discipline of `spec/07` §3.5–§3.6 propagates through the sugar
    unchanged.
  - `seek_to_time_sub_sample_period_resolves_to_same_sample` —
    boundary discipline: two timestamps within the same sample
    period at 48 kHz collapse to the same SeekPoint; the
    `target_sample + 1` boundary advances by exactly one sample.

  Plus eight unit tests in `decoder::duration_helpers_tests` walking
  the `duration_to_sample_index` / `samples_to_duration` primitives
  at the rate × sample-index cube
  (`{44 100, 48 000, 96 000, 0x7FFFFF}` × `{0, 1, 2, rate,
  rate × 5 + 17}`) and the boundary-rounding properties (monotone,
  floor-floor round-trip within one sample period, sample-rate-zero
  short-circuit). Pre-existing test surface: 106 lib + 9 integration
  (the round-209 CHANGELOG's `100 lib + 9` undercounted by 6 — the
  delta sat in non-roundtrip modules like `tables`, `trailers`,
  `stage_b`, etc.); r215 adds 9 + 8 = 17 lib tests, total 123 + 9.

- Round-209: public `Decoder::decode_from_sample(sample_index)` and
  `Decoder::frame_iter_from_sample(sample_index)` — player-API sugar
  that combines the round-187 `seek_to_sample` + `frame_iter_from`
  pair with the in-frame prefix skip into a single entry point.
  `decode_from_sample(s)` eagerly returns the suffix of `decode_all`
  starting at the per-channel sample boundary `s` (length
  `(total_samples - s) * channels`); `frame_iter_from_sample(s)`
  returns a trace-silent `SampleSkipIter` that lazily yields the
  same suffix one frame at a time, with the leading
  `sp.sample_offset_in_frame × channels` entries trimmed from the
  first decoded frame. Both reuse the existing
  `seek_to_sample` arithmetic verbatim (per `spec/01` §4.1) and
  inherit the per-frame state-reset discipline of `spec/01` §5.1 +
  `spec/02..05` §3.1 that makes mid-stream resume bit-exact against
  the eager baseline. Format=1 and format=2 (password-protected) are
  both reachable. Ten new tests in `roundtrip_tests` lock the
  invariants:
  - `decode_from_sample_matches_eager_tail_*` (5 cells: mono16 /
    stereo16 / stereo24 / 6ch16 in format=1, stereo16 in format=2):
    `decode_from_sample(s)` equals `decode_all()[s × channels..]`
    bit-exactly at multiple `s` per cell (start, multiple
    fractions, `total_samples - 1`).
  - `frame_iter_from_sample_concat_matches_eager_tail`: the lazy
    iterator's concatenation equals the eager tail AND equals the
    by-hand `seek_to_sample + frame_iter_from + manual skip`
    composition (pinning that the new API is exactly sugar — no
    semantic drift).
  - `frame_iter_from_sample_zero_equals_full_decode`: boundary
    `s = 0` round-trips via the iterator to the full
    `decode_all` output.
  - `decode_from_sample_last_sample_returns_one_frame_of_one_sample`:
    boundary `s = total_samples - 1` returns exactly `channels`
    interleaved entries (one per-channel sample at the very end).
  - `decode_from_sample_rejects_out_of_range`: `s >= total_samples`
    surfaces `Error::SampleIndexOutOfRange` from both APIs (and
    `u64::MAX` does not panic).
  - `frame_iter_from_sample_format2_seek_and_resume_bit_exact`:
    format=2 (password-protected) lazy seek-and-resume matches the
    eager `decode_with_password` tail bit-exactly, verifying the
    per-frame qm re-prime discipline of `spec/07` §3.5–§3.6
    propagates through the new iterator unchanged.
  Pre-existing test count: 100 lib + 9 integration; r209 adds 10
  lib tests, total 110 + 9.

- Round-204: public `Decoder::new_with_password(bytes, password)`
  constructor that brings the round-187 streaming + random-access
  decode surface (`frame_iter`, `decode_frame_at`, `seek_to_sample`,
  `frame_iter_from`) onto format=2 (password-protected, `spec/07`)
  streams. Until r204 the streaming surface was reachable only via
  `Decoder::new`, which rejects format=2 with
  `Error::PasswordRequired`; format=2 streams therefore had to go
  through the eager `decode_with_password` path and could not take
  advantage of bounded-memory iteration or random-access by
  seek-table index. The new constructor derives the eight-byte
  ECMA-182 CRC-64 digest of the password per `spec/07` §3.2 and
  applies it as the Stage-A `qm[0..7]` priming at every per-channel
  frame init per `spec/07` §3.5–§3.6, then exposes the resulting
  `Decoder` as a public API. A format=1 stream constructed via the
  same call is a transparent alias for `Decoder::new`: the priming is
  computed but dropped on the constructed decoder via the existing
  crate-internal `clear_priming` path so the format=1 zero-init
  invariant of `spec/02` §3.1 is preserved (audit/07 §6.2-2,
  same shape as the eager `decode_with_password`). Six new tests in
  `roundtrip_tests` lock the new surface:
  - `new_with_password_format2_streaming_matches_eager_stereo_16bit`
    — frame_iter / decode_frame_at / frame_iter_from on a 2 s stereo16
    44.1 kHz format=2 stream all match the eager
    `decode_with_password` PCM bit-exactly across every frame.
  - `new_with_password_seek_to_sample_format2_lands_in_right_frame`
    — `seek_to_sample` on a 2.5 s format=2 stream lands in the right
    frame at samples 0, mid, end and `sample_offset_in_frame` matches
    the residue.
  - `new_with_password_format2_seek_and_resume_bit_exact` — the
    integration property: seek to ~75 % through a 2.5 s format=2
    stream, decode via `frame_iter_from`, skip the in-frame prefix,
    compare against the eager `decode_with_password` tail. Bit-exact.
  - `new_with_password_format1_stream_drops_unused_priming` — a
    format=1 stream constructed via `new_with_password` decodes
    bit-identically to `Decoder::new` (both `decode_all` and
    `frame_iter`); the unused digest is dropped per audit/07 §6.2-2.
  - `new_with_password_format2_wrong_password_decodes_but_corrupts`
    — `spec/07` §11 (no MAC): a wrong password produces a
    successfully-decoded stream of corrupt PCM (no panic, no spurious
    `Crc32Mismatch` — the CRC is over the bitstream, not over
    post-Stage-A samples). Right-password decode bit-exactly
    round-trips the originals; wrong-password decode preserves the
    shape but produces distinct PCM.
  - `new_with_password_format2_out_of_range_index_errors` — the
    same `FrameIndexOutOfRange` / `SampleIndexOutOfRange` rejection
    shape as the format=1 surface, on a format=2 stream.

  Round-204 also extends `benches/streaming.rs` with a
  `stereo16_44k1_1s_format2` cell in both `streaming_frame_iter_cube`
  and `streaming_decode_frame_at_cube`, closing the prior cube's
  format=2 omission. The cell uses the new
  `Decoder::new_with_password` constructor at the same parameter
  point as the format=1 anchor (`stereo16` × `16-bit` × `44.1 kHz` ×
  `1 s`), so the marginal cost of the per-frame qm re-prime is
  directly comparable against the format=1 baseline. Reference
  numbers on the development machine (`--bench --quick`):
  `streaming_frame_iter_cube/stereo16_44k1_1s_format2` ≈ 1.22 ms
  (~138 MiB/s); `streaming_decode_frame_at_cube/stereo16_44k1_1s_format2`
  ≈ 1.20 ms (~140 MiB/s) — within noise of the format=1 sibling at
  the same shape. README `## Status` + `## Benchmarks` sections grew
  the r204 entries; bench file-head documentation gains a "Round
  204 (format=2 streaming reach)" paragraph; `lib.rs` `## Public API`
  section flags the new constructor on `Decoder`. Lib-test count:
  90 → 96 (+6 in `roundtrip_tests`); integration tests unchanged at
  9. No
  changes to the existing decoder hot path; the new constructor is a
  thin wrapper over the existing `Decoder::new_with_priming` plus
  the existing `clear_priming` invariant restoration.

- Round-198: parameter-cube extension of `benches/streaming.rs`. Two
  new Criterion groups walk the format=1 `(channels × bps ×
  sample_rate)` cube already covered by the sibling `decode.rs`
  baseline (mono16-44k1-1s, stereo24-48k-500ms, 6ch16-48k-250ms):
  `streaming_frame_iter_cube` measures lazy `frame_iter` cost per
  cell, and `streaming_decode_frame_at_cube` measures the middle-
  frame (or frame 0 for single-frame cells) random-access decode
  cost per cell. The original four scenarios anchored at the
  stereo16-44k1 point are preserved unchanged as the per-API
  comparison anchor; the cube is additive so future optimisation
  rounds get A/B baselines across the actual TTA parameter space
  rather than only the original single cell. Format=2 is
  intentionally omitted from the cube: the public streaming surface
  (`Decoder::new` → `frame_iter` / `decode_frame_at`) is format=1
  only, and the eager `decode_with_password` path is already
  covered by `decode.rs::decode_stereo_16bit_44k1_format2_1s`. PCM
  inputs reuse the existing in-bench `build_pcm` helper (xorshift32
  envelope + per-sample noise) so the workload is identical to the
  other three benches; no checked-in fixtures.

### Changed

- Round-209: paraphrased the pre-existing reference-encoder oracle
  attribution in `src/roundtrip_tests.rs` (module-level docstring
  lines 18–22, "What this does NOT verify (deferred to Auditor)"
  block) to neutral wording. The deferred-verification semantics
  are unchanged — the clean-room wall still bars reference-encoder
  source as an input — but the cited tool name has been replaced
  with a description of its role ("reference-encoder-produced TTA1
  byte stream", "reference-encoded fixture") so the prose no longer
  carries the third-party-implementation identifier.

## [0.0.2](https://github.com/OxideAV/oxideav-tta/compare/v0.0.1...v0.0.2) - 2026-05-30

### Other

- streaming_decode cargo-fuzz target
- streaming + random-access decode API on `Decoder`
- scan_trailers + encode_roundtrip cargo-fuzz targets
- Round 156: malformed-input property tests (tests/malformed_props.rs)
- Round 127: criterion bench harnesses (decode / encode / roundtrip)
- add cargo-fuzz decode harness; cap Rice k at 31
- multi-frame format=2 trace coverage + audit/07 cleanups
- drop one last external-reference citation in src/trailers.rs module head
- drop external-reference cross-references and forbidden reference/source/ path citation
- ID3v1 + APEv2 trailer detection per spec/01 §7
- Round 3 — production TTA1 encoder + framework Encoder impl
- O(1) seek via TTA1 in-file seek table
- Round 2 — spec/06 trace + oxideav-core integration + format=2 password
- vendor lms-shift.csv + lms-dx-magnitudes.csv into the crate
- Round 1 — TTA1 format=1 decoder against the clean-room workspace
- Round 0 — clean-room rebuild scaffold (orphan master)

### Added

- Round-193: `benches/streaming.rs` Criterion harness covering the
  round-187 streaming + random-access decode surface on `Decoder`
  (`frame_iter`, `decode_frame_at`, `seek_to_sample`,
  `frame_iter_from`). All four scenarios run against the same 3 s
  stereo 16-bit 44.1 kHz stream (three frames under
  `regular_frame_samples = 46_073` per `spec/01` §4.1), so the lazy
  `frame_iter` cost is directly comparable to the eager `decode`
  baseline (`decode.rs::decode_stereo_16bit_44k1_1s × 3`),
  `decode_frame_at(1)` measures a single mid-stream random-access
  decode worth of per-frame work (Rice + Stage-A LMS + Stage-B +
  decorr cascade + CRC32 verify), `seek_to_sample` is the constant-
  time `spec/01` §4.1 `sample_index / regular_frame_samples`
  arithmetic (regression sentinel against accidentally turning it
  into a linear walk of `self.frames`), and `frame_iter_from(1)`
  is the resume-from-seek cost (= what an interactive seek-and-play
  path actually pays on top of the constant-time
  `seek_to_sample` lookup). The new bench follows the existing
  decode/encode/roundtrip pattern: no checked-in fixture files,
  per-bench `Throughput::Bytes` accounting against the PCM size,
  same `xorshift32`-driven PCM generator the sibling benches use so
  cross-bench numbers are directly comparable, and reuse of one
  `Decoder<'a>` per scenario across iterations (legitimate because
  every frame resets its trackers per `spec/01` §5.1 +
  `spec/02..05` §3.1, so the decoder carries no decode state
  between calls). Reference numbers on the development machine:
  `frame_iter` 3 s ≈ 3.72 ms (~135 MiB/s),
  `decode_frame_at(1)` ≈ 1.24 ms (~142 MiB/s),
  `seek_to_sample` ≈ 1.07 ns (constant-time sentinel),
  `frame_iter_from(1)` ≈ 2.74 ms (= ~2/3 of full-stream cost).
  README `## Benchmarks` section grew the streaming entry; new
  `[[bench]] name = "streaming"` block in `Cargo.toml`. Run with
  `cargo bench -p oxideav-tta --bench streaming`.

- Round-190: `streaming_decode` cargo-fuzz target under
  `fuzz/fuzz_targets/streaming_decode.rs`. Drives the round-187
  streaming + random-access decode surface on `Decoder`
  (`frame_iter`, `decode_frame_at`, `seek_to_sample`,
  `frame_iter_from`) with attacker-chosen byte streams paired with
  attacker-chosen `target_frame_index` / `target_sample_index` /
  `start_index` seeds packed into the first ten bytes of the fuzz
  input. Asserts cross-API agreement against the eager `decode_all`
  baseline on every constructed input: the lazy `frame_iter` must
  concatenate to the eager output bit-exactly, `decode_frame_at`
  must match the corresponding eager slice, `seek_to_sample` must
  return an in-range `(frame_index, sample_offset_in_frame)` pair,
  and `frame_iter_from(start_index)` must equal the eager suffix
  from the matching sample boundary. Contract is the standard
  panic-free / no-integer-overflow / no-OOB / no-unbounded-alloc
  shape the existing three fuzz targets share. Seed corpus under
  `fuzz/corpus/streaming_decode/` includes the five real-stream
  fixtures plus four crate-encoded multi-frame seeds (mono16 /
  stereo16 / stereo24 spanning 2.5-3 s + a 3 s format=2 stereo
  stream), each pre-prefixed with the ten-byte seed header so the
  random-access branches are driven from the first fuzz iteration.
  The fuzz workflow's auto-discovery picks up the new `[[bin]]`
  block from `fuzz/Cargo.toml`; the daily 30-minute budget is now
  split four-way across `decode`, `scan_trailers`,
  `encode_roundtrip`, and `streaming_decode`. The cross-API
  agreement assertion is gated on `frame_count <= 4096` so the
  fuzzer's per-iteration budget stays on the streaming-state-
  machine surface rather than the `total_samples * channels`
  eager allocation.

- Round-187: streaming + random-access decode API on `Decoder`.
  The new surface exposes:
  - `Decoder::frame_iter(&self) -> FrameIter` — lazy iterator that
    yields one `Result<Vec<i32>>` per frame. Memory usage is
    bounded by `O(samples_per_frame * channels)` regardless of
    stream length. The eager `Decoder::decode_all` path is
    unchanged; new code that wants to start producing PCM before
    the whole file is decoded can use this instead.
  - `Decoder::decode_frame_at(&self, index)` — random-access
    decode of a single frame addressed by its seek-table index.
    Bit-identical to the slice of `decode_all` covering that
    frame (verified by the new
    `decode_frame_at_matches_decode_all_mono_24bit` test): the
    spec/01 §5.1 + spec/02..05 §3.1 per-frame state-reset
    discipline is what makes this safe.
  - `Decoder::seek_to_sample(&self, sample_index)` — locate the
    frame containing a given per-channel sample, returning a
    `SeekPoint { frame_index, sample_offset_in_frame }` driven by
    the spec §4.1 `regular_frame_samples = floor(sample_rate *
    256 / 245)` arithmetic.
  - `Decoder::frame_iter_from(&self, start_index)` — start a lazy
    iterator at the given frame instead of zero, so the
    seek-and-resume path does not decode the skipped prefix.
  - `Decoder::total_samples(&self)` — accessor for the
    per-channel total sample count (mirrors the header field but
    avoids the consumer having to reach into `header`).
  - `FrameIter` (re-exported at crate root) and `SeekPoint` —
    `ExactSizeIterator` shape with a correct `size_hint`.
  Adds `Error::FrameIndexOutOfRange` and
  `Error::SampleIndexOutOfRange` for the two new failure modes.
  Six new lib tests in `roundtrip_tests` lock the bit-exact
  agreement with `decode_all`, the seek-and-resume integration
  property, and the out-of-range rejection behaviour. Lib-test
  count: 78 → 85 (+7 incl. the new past-end test); integration
  tests unchanged at 9 (the existing `malformed_props.rs`
  exhaustive-`match Error` block was widened with the two new
  variants as panic arms — they are decoder-API misuse, not
  outcomes a header bit-flip can produce).
- Round-175: two additional cargo-fuzz targets under
  `fuzz/fuzz_targets/`:
  - `scan_trailers` — drives the public `scan_trailers` entry point
    with arbitrary bytes. Exercises the ID3v2 prefix skip, TTA1
    header parse, seek-table sum arithmetic, and the
    `detect_trailers` ID3v1 / APEv2 footer scanner (`spec/01` §7).
    Contract: every call returns a `Result`, never panicking,
    integer-overflowing, indexing out of bounds, or allocating
    proportional to an attacker-controlled header field. 500K
    iterations clean (cov 132, ft 133).
  - `encode_roundtrip` — drives the public `encode` /
    `encode_with_password` over a typed `(channels × bps ×
    sample_rate × format × samples)` parameter cube and asserts (i)
    the encoder either rejects with a typed
    `Error::Unsupported…` / `InvalidSampleBuffer`, or returns
    `Ok(bytes)`; (ii) every `Ok(bytes)` decodes back via the matching
    `decode` / `decode_with_password` call; (iii) the recovered
    `i32` samples equal the input bit-exactly. 500K iterations clean
    (cov 688, ft 3221, ~18.5K exec/s).
  Both targets seeded under `fuzz/corpus/{scan_trailers,encode_roundtrip}/`.
  The reusable `OxideAV/.github` fuzz workflow auto-discovers
  cargo-fuzz `[[bin]]` blocks from `fuzz/Cargo.toml`, so the 30-min
  daily budget is now split three-way across `decode`,
  `scan_trailers`, and `encode_roundtrip`.

- Round-156: malformed-input property tests under `tests/malformed_props.rs`.
  Nine integration tests, all driven by a deterministic xorshift64*
  PRNG so failures reproduce from the literal seed in the source
  (matching `oxideav-scene/tests/transform_props.rs`'s convention).
  Coverage classes — exhaustive 22×8 bit-flip walk of the stream
  header, exhaustive prefix-truncation walk (format=1 and format=2),
  seek-table re-CRC bait (recompute the seek-table CRC after
  corrupting an entry, forcing the rejection signal onto the
  per-frame CRC), oversize `total_samples` with recomputed header
  CRC, wrong-password format=2 (must not panic; if `Ok` the PCM
  shape must match the header), randomised ID3v2-prefix sweep
  (every prefix must yield identical PCM to the un-prefixed
  decode), randomised trailer-region junk (`scan_trailers` must
  never claim a trailer that overlaps the in-stream frame region),
  and pseudo-noise round-trip at randomised channel-count /
  bit-depth shapes. Targets the *semantic* fault classes that
  random-bytes fuzzing typically misses (a corrupt seek-table
  with a valid CRC is structurally indistinguishable from a real
  one to a panic-only oracle). All nine pass against the r124
  Rice-cap fix, the r127 baseline encoder, and the r5 audit/07
  closures. Tests use only the public crate API (`decode`,
  `decode_with_password`, `encode`, `encode_with_password`,
  `scan_trailers`); a local IEEE-802.3 CRC32 helper duplicates
  `spec/01` §6 instead of reaching into the crate's private
  `crc32` module.

- Round-127: Criterion benchmark harnesses under `benches/`. Three
  self-contained binaries — `decode`, `encode`, and `roundtrip` —
  drive the production decoder + encoder over a deterministic
  xorshift-synthesised PCM workload (mono / stereo / 6-channel,
  16-bit and 24-bit, plus a format=2 password-derived qm-priming
  variant). No checked-in fixtures: each scenario builds its input
  in-bench so future optimisation rounds (SIMD Rice emit, faster
  qm-priming, etc.) have a stable A/B baseline. Run with
  `cargo bench -p oxideav-tta --bench <name>`. Pairs with the
  r124 fuzz harness as the "saturated → fuzz/bench/profile"
  follow-through.

- Round-124: cargo-fuzz harness. `fuzz/fuzz_targets/decode.rs` is a
  decode-only libfuzzer target driving both `decode` (format=1) and
  `decode_with_password` (format=2) over arbitrary bytes; the contract
  is that the call always returns a `Result` rather than panicking,
  overflowing, indexing out of bounds, or OOMing. Seed corpus under
  `fuzz/corpus/decode/` is five real streams from the crate's own
  encoder. `.github/workflows/fuzz.yml` gives it a daily 30-minute
  budget via the org reusable `crate-fuzz.yml`. The harness body is
  clean-room (no external-reference oracle). Two regression unit tests in `rice`
  pin the cap behaviour found by the fuzzer.

### Fixed

- Round-124: Rice decoder could drive the adaptive parameter `k` past
  31 on a corrupt high-mode bitstream that chained enough escapes,
  after which the next binary-tail read requested more than 32 bits and
  tripped `BitReader::read_bits`'s `k <= 32` invariant (a debug-build
  panic; a garbage shift in release). `k0`/`k1` are now capped at 31 on
  increment — matching the `[0, 31]` range the reference encoder stays
  within per `spec/05` §5.3 — so the cap never alters the decode of any
  valid stream. Found by the new `fuzz/fuzz_targets/decode.rs` harness.

- Round-5: multi-frame format=2 (password-derived qm priming) round-trip
  + trace-tape coverage closing `docs/audio/tta-cleanroom/audit/07`
  §6.2-5. New tests in `roundtrip_tests` exercise format=2 across 3+
  frames in both mono (2.5 s @ 44.1 kHz) and stereo configurations and
  verify (a) sample-exact decoder/encoder round-trip across every
  frame boundary, and (b) under the `trace` feature, that the
  `LMS_PRE` event's `qm_pre[]` carries the ECMA-182 CRC-64 digest
  bytes at step_idx ∈ {0..nch-1} of **every** frame — proving the
  spec/07 §3.5 / §3.6 "qm priming reapplied at every frame init,
  shared across all `nch` channels" rule on the wire (single-frame
  trace coverage couldn't distinguish "prime once at frame 0" from
  "prime at every frame").

### Changed

- `decoder::Decoder` now stores the freshly-computed IEEE-802.3 CRC32
  of the 18 stream-header body bytes (`header_crc`) alongside the
  parsed header. The `trace` feature's `HEADER_CRC` event surfaces
  the real value instead of the prior placeholder zero, closing
  `audit/07` §6.2-3. The header parser exposes the value through a
  new `parse_stream_header_with_crc` entry; the existing
  `parse_stream_header_any_format` is a thin wrapper that drops it.
- `decode_with_password` no longer re-parses the header and seek
  table when the on-disk `format` field is `1` (audit/07 §6.2-2).
  A new crate-internal `Decoder::clear_priming` method drops the
  computed digest in place on the already-constructed decoder so
  format=1's spec/02 §3.1 zero-init invariant is preserved without
  the redundant second parse.

## [0.0.1] — round 1–4

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
  2026-05-06 (external reference source cited as the writeup's basis, not merely
  as the trace-instrumentation host); the prior history is preserved
  on the `old` branch.
- The new code is being written against the strict-isolation
  clean-room workspace at `docs/audio/tta-cleanroom/` (Specifier /
  Extractor / Implementer / Auditor roles, with explicit allow-list
  and forbidden-input list per role). The Implementer reads only
  `spec/` + `tables/` + `reference/docs/`; external reference
  implementations and their source are forbidden inputs.
