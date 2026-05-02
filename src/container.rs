//! Tiny TTA file walker exposed for callers that want to drive the
//! decoder without writing their own demuxer. Not registered as a
//! container — the decoder ships first; a full demuxer (sample-rate
//! seeking + APE/ID3 trailer skip) is a follow-up.

pub use crate::header::{parse_file, FrameRef, ParsedFile, TtaHeader, HEADER_LEN};
