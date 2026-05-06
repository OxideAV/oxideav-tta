//! Pure-Rust True Audio (TTA) lossless audio codec.
//!
//! **Round 0 — clean-room rebuild scaffold.** This is a fresh orphan
//! `master`; the previous implementation was retired alongside the
//! OxideAV docs audit dated 2026-05-06 (see
//! `https://github.com/OxideAV/docs/blob/master/AUDIT-2026-05-06.md`).
//! The new implementation is being built against the strict-isolation
//! clean-room workspace at `docs/audio/tta-cleanroom/`. Until the
//! Implementer round lands, this crate exposes nothing beyond the
//! crate-local `Error` type below.
//!
//! See `README.md` for the rebuild scope and the four-role
//! (Specifier / Extractor / Implementer / Auditor) methodology.

#![forbid(unsafe_code)]

/// Crate-local error type. Concrete variants are added as the
/// Implementer round populates each pipeline stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Reserved placeholder. Will be replaced by real variants
    /// (InvalidHeader / Truncated / Crc32Mismatch / Unsupported / …)
    /// in the Implementer round.
    NotImplemented,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotImplemented => f.write_str(
                "oxideav-tta: clean-room rebuild in progress — see crates/oxideav-tta/README.md",
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Crate-local Result alias.
pub type Result<T> = core::result::Result<T, Error>;
