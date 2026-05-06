# oxideav-tta

Pure-Rust True Audio (TTA) lossless audio codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 0 — clean-room rebuild.** This `master` branch is a fresh
orphan; the previous implementation was retired alongside the docs
audit dated 2026-05-06 (see [AUDIT-2026-05-06.md](https://github.com/OxideAV/docs/blob/master/AUDIT-2026-05-06.md)).
The previous history is preserved on the `old` branch for reference.

The new implementation is being built against the strict-isolation
clean-room workspace at
[`docs/audio/tta-cleanroom/`](https://github.com/OxideAV/docs/tree/master/audio/tta-cleanroom)
under the four-role discipline (Specifier / Extractor / Implementer /
Auditor). The Implementer in this repo reads only `spec/` + `tables/`
+ `reference/docs/` from the clean-room workspace; it does NOT read
libtta source, FFmpeg `libavcodec/tta*`, or the retired
`audio/tta/tta-trace-reverse-engineering.md` writeup.

## Why clean-room

libtta is the canonical TTA reference (Aleksander Djuric / Pavel
Zhilin, en.true-audio.com, LGPL-2.1). oxideav cannot ship LGPL code,
so every line of this crate must be written without reading libtta
or any FFmpeg-derived TTA source. The clean-room workspace at
`docs/audio/tta-cleanroom/` is the wall.
