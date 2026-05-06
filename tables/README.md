# tables/ — Stage-A LMS numeric tables

Vendored snapshot of the Extractor-published tables for the TTA1
Stage-A predictor, used by `src/tables.rs` via `include_str!`.

The source-of-truth lives in the clean-room workspace at
`docs/audio/tta-cleanroom/tables/`; these copies are byte-identical
and carry the same provenance. Any update to the cleanroom CSVs must
be mirrored here in the same commit.

## Files

| File | Shape | Spec reference |
| ---- | ----- | -------------- |
| `lms-shift.csv` | 3 rows: `(index, shift)` | `spec/02-stage-a-lms.md` §3.2 / §10 |
| `lms-dx-magnitudes.csv` | 4 rows: `(dx_index, magnitude)` | `spec/02-stage-a-lms.md` §4.5 / §10 |

The cleanroom workspace's `tables/lms-shift.meta` and
`tables/lms-dx-magnitudes.meta` carry the full Extractor session
provenance (SHA-256 of the source file, line numbers, extraction
method). This crate vendors only the data; the metadata is consulted
in the cleanroom workspace.
