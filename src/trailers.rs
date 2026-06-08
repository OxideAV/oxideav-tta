//! Optional ID3v1 / APEv2 trailer detection per `spec/01` §7.
//!
//! TTA1's framing layer ends at the last frame's trailing CRC. Any
//! bytes that appear past `frame_start[N-1] + seek_table_entry[N-1]`
//! are out-of-stream metadata containers — the TTA codec does not
//! decode them, but a TTA-aware host application typically wants to
//! know whether the file carries an ID3v1 trailer, an APEv2 trailer,
//! both, or neither, so it can preserve them on re-encode.
//!
//! Per spec §7 the detection rules are:
//!
//! - **ID3v1**: file's last 128 bytes start with `'T','A','G'`. The
//!   trailer is fixed-length (128 bytes) and lives at the very end
//!   of the file when present.
//! - **APEv2**: scan the trailer region for the eight-byte magic
//!   `'APETAGEX'`. The APE-tag region is bracketed by a 32-byte
//!   header at its start AND a 32-byte footer at its end, each
//!   carrying that magic. When ID3v1 is also present, the APE tag
//!   sits immediately before the 128-byte ID3v1 trailer.
//!
//! The DOC §"TTA Frame Structure" prose explicitly notes "TTA format
//! supports both of ID3v1/v2 and APEv2 information tags" but defers
//! to the published ID3 / APE specifications for byte layout. This
//! module implements only the **detection** part — it does not parse
//! tag contents (that is the host application's job, and ID3v1 /
//! APEv2 parsers live in dedicated crates such as `oxideav-id3` and
//! `oxideav-ape` should they be added later).
//!
//! The detection works on the **byte region past the last TTA1
//! frame** — i.e. on the raw file bytes from `end_of_stream_offset`
//! (the byte just after the last frame's trailing CRC) to the end
//! of the buffer. Callers without an end-of-stream offset (e.g. the
//! demuxer at open time, after walking the seek table) compute it
//! from `frame_start[N-1] + seek_table_entry[N-1]` per spec §4.2.
//!
//! ## Typed accessors
//!
//! [`TrailerInfo`] also exposes [`TrailerInfo::id3v1_typed`] and
//! [`TrailerInfo::apev2_typed`] which lift the raw `(start, len)`
//! tuples into validated [`Id3v1Range`] / [`ApeV2Range`] newtypes
//! per `spec/01` §7. The newtypes carry the spec's structural
//! invariants — ID3v1 is exactly 128 bytes anchored at file end;
//! APEv2 is at least the 32-byte footer minimum and lies entirely
//! within the file — so a caller that hand-constructs a literal
//! gets the same `Error::InvalidId3v1Range` /
//! `Error::InvalidApeV2Range` discipline `detect_trailers` enforces
//! at parse time.

use crate::error::{Error, Result};

/// Detected trailer ranges within a TTA1 byte buffer.
///
/// Each range is a `(start, len)` byte tuple — `start` is absolute
/// within the original file buffer, `len` is the trailer's byte
/// length (128 for ID3v1; variable for APEv2 per its 32-byte footer's
/// declared tag size). The host application can slice the bytes
/// directly from `buf[start..start + len]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrailerInfo {
    /// Byte range of the ID3v1 trailer (always 128 bytes when
    /// present; `None` if no ID3v1 sits at file end).
    pub id3v1: Option<(usize, usize)>,
    /// Byte range of the APEv2 trailer (footer-derived size; `None`
    /// if no APE tag is detected past the last TTA frame).
    pub apev2: Option<(usize, usize)>,
}

impl TrailerInfo {
    /// `true` when neither trailer was detected.
    pub fn is_empty(&self) -> bool {
        self.id3v1.is_none() && self.apev2.is_none()
    }

    /// Lift the raw `id3v1` field into the typed [`Id3v1Range`]
    /// accessor per `spec/01` §7.
    ///
    /// Returns `Ok(None)` when no ID3v1 trailer was detected.
    /// Returns `Ok(Some(range))` when the detected trailer satisfies
    /// the spec §7 invariants for the supplied `file_len` (length is
    /// exactly 128 and the range is anchored at the file end).
    /// Returns `Err(Error::InvalidId3v1Range(start, len))` only if
    /// the [`TrailerInfo`] was hand-constructed from a literal that
    /// violates the invariant — the parser-produced [`TrailerInfo`]
    /// values from [`crate::detect_trailers`] / [`crate::scan_trailers`]
    /// always satisfy it at the `file_len` they were scanned against.
    pub fn id3v1_typed(&self, file_len: usize) -> Result<Option<Id3v1Range>> {
        match self.id3v1 {
            None => Ok(None),
            Some((start, len)) => Id3v1Range::from_raw(start, len, file_len).map(Some),
        }
    }

    /// Lift the raw `apev2` field into the typed [`ApeV2Range`]
    /// accessor per `spec/01` §7.
    ///
    /// Returns `Ok(None)` when no APEv2 trailer was detected.
    /// Returns `Ok(Some(range))` when the detected trailer satisfies
    /// the spec §7 invariants for the supplied `file_len` (length is
    /// at least the 32-byte footer minimum, and the range is bounded
    /// by the file). Returns `Err(Error::InvalidApeV2Range(start, len))`
    /// only on a hand-constructed literal that violates the invariant.
    pub fn apev2_typed(&self, file_len: usize) -> Result<Option<ApeV2Range>> {
        match self.apev2 {
            None => Ok(None),
            Some((start, len)) => ApeV2Range::from_raw(start, len, file_len).map(Some),
        }
    }

    /// Combined byte range covering both detected trailers as
    /// `Some((combined_start, combined_len))` — the smallest contiguous
    /// `[start, start + len)` window that contains every detected
    /// trailer per `spec/01` §7.
    ///
    /// Returns `None` when [`Self::is_empty`] is true. When only one
    /// trailer is present, the combined window equals that trailer's
    /// own range. When both are present, `combined_start = min(starts)`
    /// and the combined window's end equals `max(ends)`; the spec §7
    /// "APE immediately before ID3v1" ordering makes the two regions
    /// adjacent in the well-formed case (`detect_trailers` returns
    /// `apev2_start + apev2_len == id3v1_start` when both are present),
    /// so `combined_len == id3v1_len + apev2_len`. The combined window
    /// is what a host application slices to preserve every byte of
    /// out-of-stream metadata on round-trip re-encode.
    pub fn combined_byte_range(&self) -> Option<(usize, usize)> {
        match (self.id3v1, self.apev2) {
            (None, None) => None,
            (Some((s, l)), None) | (None, Some((s, l))) => Some((s, l)),
            (Some((s_a, l_a)), Some((s_b, l_b))) => {
                let start = s_a.min(s_b);
                let end = (s_a.saturating_add(l_a)).max(s_b.saturating_add(l_b));
                Some((start, end.saturating_sub(start)))
            }
        }
    }
}

/// Typed wrapper around a [`TrailerInfo::id3v1`] range — the
/// `(start, len)` byte tuple of a detected ID3v1 trailer per
/// `spec/01` §7.
///
/// Validated against the spec §7 invariants: the length is exactly
/// 128 bytes (ID3v1 is a fixed-size trailer per the published ID3v1
/// design notes referenced from `spec/01` §7), and the range is
/// anchored at the very end of the file (`start + len == file_len`
/// per spec §7's "file's last 128 bytes start with `'TAG'`" detection
/// rule). The parser-produced [`TrailerInfo`] from
/// [`crate::detect_trailers`] / [`crate::scan_trailers`] always
/// satisfies the invariant by construction; the typed accessor
/// surfaces the invariant at lift time so a caller that constructs a
/// [`TrailerInfo`] literal (e.g. an ad-hoc fixture) gets the same
/// [`Error::InvalidId3v1Range`] discipline `detect_trailers` enforces
/// at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id3v1Range {
    start: usize,
    len: usize,
}

impl Id3v1Range {
    /// Lift a raw `(start, len)` ID3v1 range into the typed accessor.
    /// Returns [`Error::InvalidId3v1Range`] when any of the spec §7
    /// invariants fails:
    ///
    /// - `len != 128` (ID3v1 is a fixed-size 128-byte trailer per the
    ///   published ID3v1 design notes referenced from `spec/01` §7),
    /// - `start + len` overflows `usize`,
    /// - `start + len > file_len` (the trailer must fit within the file),
    /// - `start + len != file_len` (ID3v1 lives at the very end of the
    ///   file per `spec/01` §7's "file's last 128 bytes start with
    ///   `'TAG'`" detection rule — any byte past the trailer would
    ///   either be a second ID3v1 trailer or out-of-spec garbage).
    ///
    /// The strict-equality "anchored at file end" check is what
    /// distinguishes a structurally-legal ID3v1 from a forensic
    /// `'TAG'` pattern coincidentally appearing somewhere earlier in
    /// the file.
    pub fn from_raw(start: usize, len: usize, file_len: usize) -> Result<Self> {
        if len != 128 {
            return Err(Error::InvalidId3v1Range(start, len));
        }
        let end = match start.checked_add(len) {
            Some(e) => e,
            None => return Err(Error::InvalidId3v1Range(start, len)),
        };
        if end != file_len {
            return Err(Error::InvalidId3v1Range(start, len));
        }
        Ok(Id3v1Range { start, len })
    }

    /// Absolute byte offset of the ID3v1 trailer's first byte
    /// (the `'T'` of the `'TAG'` magic per `spec/01` §7).
    pub fn start(&self) -> usize {
        self.start
    }

    /// Byte length of the ID3v1 trailer (always exactly `128` per
    /// `spec/01` §7).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always `false` — an [`Id3v1Range`] is fixed-length 128 bytes
    /// per `spec/01` §7, so the structural invariant guarantees a
    /// non-empty range for every value lifted through
    /// [`Self::from_raw`]. The predicate exists for clippy's
    /// `len_without_is_empty` lint and as a uniform interface
    /// surface; callers can rely on the constant `false` return for
    /// every [`Id3v1Range`] instance.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Absolute byte offset one past the ID3v1 trailer's last byte
    /// (always equals `file_len` per `spec/01` §7's "anchored at file
    /// end" invariant; saturating-add for defensive safety against
    /// hand-crafted literals that bypass [`Self::from_raw`]).
    pub fn end(&self) -> usize {
        self.start.saturating_add(self.len)
    }

    /// Half-open byte range `[start(), end())` ready for direct
    /// slicing of the file buffer: `&buf[range.byte_range()]` yields
    /// the 128 trailer bytes per `spec/01` §7.
    pub fn byte_range(&self) -> core::ops::Range<usize> {
        self.start..self.end()
    }

    /// `true` when the trailer is anchored at the very end of a file
    /// of `file_len` bytes per `spec/01` §7's "file's last 128 bytes"
    /// detection rule. Always `true` for a [`Self::from_raw`]-built
    /// instance built against the same `file_len`; the predicate is
    /// available for callers that want to re-confirm the invariant
    /// against a different `file_len` (e.g. after concatenation).
    pub fn is_at_file_end(&self, file_len: usize) -> bool {
        self.end() == file_len
    }
}

/// Typed wrapper around a [`TrailerInfo::apev2`] range — the
/// `(start, len)` byte tuple of a detected APEv2 trailer per
/// `spec/01` §7.
///
/// Validated against the spec §7 invariants: the length is at least
/// `32` bytes (the minimum legal APEv2 region is just the 32-byte
/// footer per the APE tags header spec referenced from `spec/01` §7),
/// the `start + len` arithmetic does not overflow, and the range lies
/// entirely within the file (`start + len <= file_len`). Unlike
/// [`Id3v1Range`] the APEv2 region is not required to be anchored at
/// the file end — when an ID3v1 trailer is also present the APEv2
/// footer sits immediately before the 128-byte ID3v1 region per
/// `spec/01` §7's "When ID3v1 is also present, the APE tag sits
/// immediately before the 128-byte ID3v1 trailer" rule, so the typed
/// accessor accepts both "APE alone at file end" and "APE +128 bytes
/// before file end".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApeV2Range {
    start: usize,
    len: usize,
}

impl ApeV2Range {
    /// Footer size in bytes — the fixed 32-byte trailer block at the
    /// end of every APEv2 region per the APE tags header spec
    /// referenced from `spec/01` §7.
    pub const FOOTER_SIZE: usize = 32;

    /// Optional header size in bytes — the 32-byte block that may
    /// precede the APEv2 body when the footer's `has_header` flag bit
    /// is set (per the APE tags header spec referenced from `spec/01`
    /// §7).
    pub const HEADER_SIZE: usize = 32;

    /// Lift a raw `(start, len)` APEv2 range into the typed accessor.
    /// Returns [`Error::InvalidApeV2Range`] when:
    ///
    /// - `len < 32` (the minimum legal APEv2 region is just the
    ///   32-byte footer per `spec/01` §7),
    /// - `start + len` overflows `usize`,
    /// - `start + len > file_len` (the trailer must lie within the
    ///   file's byte range).
    ///
    /// The "anchored at file end" check that applies to ID3v1 does
    /// not apply here — when both trailers are present APE sits
    /// before ID3v1 per `spec/01` §7 — so the accessor accepts every
    /// `start + len <= file_len` placement.
    pub fn from_raw(start: usize, len: usize, file_len: usize) -> Result<Self> {
        if len < Self::FOOTER_SIZE {
            return Err(Error::InvalidApeV2Range(start, len));
        }
        let end = match start.checked_add(len) {
            Some(e) => e,
            None => return Err(Error::InvalidApeV2Range(start, len)),
        };
        if end > file_len {
            return Err(Error::InvalidApeV2Range(start, len));
        }
        Ok(ApeV2Range { start, len })
    }

    /// Absolute byte offset of the APEv2 trailer's first byte. With
    /// `has_header` this is the `'A'` of the leading `'APETAGEX'`
    /// header magic; without a header it is the first body byte
    /// (footer-only APE tags begin with arbitrary body bytes per
    /// `spec/01` §7).
    pub fn start(&self) -> usize {
        self.start
    }

    /// Total byte length of the APEv2 region, including the leading
    /// header (when present) + body + trailing footer per `spec/01`
    /// §7.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always `false` — an [`ApeV2Range`] is at least
    /// [`Self::FOOTER_SIZE`] (32) bytes per `spec/01` §7, so the
    /// structural invariant guarantees a non-empty range for every
    /// value lifted through [`Self::from_raw`]. The predicate exists
    /// for clippy's `len_without_is_empty` lint and as a uniform
    /// interface surface; callers can rely on the constant `false`
    /// return for every [`ApeV2Range`] instance.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Absolute byte offset one past the APEv2 trailer's last byte
    /// — the byte just past the trailing 32-byte footer per `spec/01`
    /// §7. Saturating-add for defensive safety on hand-crafted
    /// literals that bypass [`Self::from_raw`].
    pub fn end(&self) -> usize {
        self.start.saturating_add(self.len)
    }

    /// Half-open byte range `[start(), end())` ready for direct
    /// slicing of the file buffer per `spec/01` §7.
    pub fn byte_range(&self) -> core::ops::Range<usize> {
        self.start..self.end()
    }

    /// `true` when the APEv2 region is anchored at the very end of a
    /// file of `file_len` bytes (= no ID3v1 trailer follows it) per
    /// `spec/01` §7's "APE search ends just before the ID3v1 trailer
    /// (if present)" rule. When ID3v1 is also present this is `false`;
    /// the APE region sits 128 bytes before the file end.
    pub fn is_at_file_end(&self, file_len: usize) -> bool {
        self.end() == file_len
    }

    /// Number of body bytes plus optional 32-byte header — the part
    /// of the APEv2 region the parser must read for item content
    /// per `spec/01` §7's reference to the APE tags header spec. With
    /// `has_header = false` this is just the body; with `has_header
    /// = true` it includes the 32-byte header block. Subtracts the
    /// fixed [`Self::FOOTER_SIZE`] from the total length so the
    /// caller does not have to repeat the arithmetic.
    ///
    /// Saturates at `0` on the defensive-bound case where the
    /// stored length is smaller than the footer size — unreachable
    /// for a [`Self::from_raw`]-built instance per the lift-time
    /// `len >= 32` gate.
    pub fn header_and_body_size(&self) -> usize {
        self.len.saturating_sub(Self::FOOTER_SIZE)
    }
}

/// Scan the bytes past `end_of_stream_offset` for optional ID3v1 and
/// APEv2 trailers per `spec/01` §7.
///
/// `end_of_stream_offset` is the byte position immediately following
/// the last TTA1 frame's trailing CRC (i.e. the cumulative sum
/// `frame_start[N-1] + seek_table_entry[N-1]`). Bytes at or after
/// that offset are out-of-stream metadata.
///
/// The function NEVER reads bytes before `end_of_stream_offset` — the
/// in-stream TTA bytes are off-limits to this scanner. Out-of-range
/// `end_of_stream_offset` values (greater than `buf.len()`) yield an
/// empty result.
///
/// Detection follows spec §7 exactly:
///
/// 1. If the last 128 bytes of `buf` start with `'TAG'` AND lie at or
///    past `end_of_stream_offset`, record the ID3v1 range.
/// 2. Compute the APE-search upper bound — `buf.len() - 128` if
///    ID3v1 was detected (the APE footer sits immediately before
///    the ID3v1 trailer), else `buf.len()`.
/// 3. If the 32 bytes immediately before that upper bound start with
///    `'APETAGEX'` (the APEv2 footer's magic per the APE-tags-header
///    spec), parse the footer's declared `tag_size` field (LE u32 at
///    footer offset 12). The APE tag's start offset is
///    `(apev2_upper_bound - tag_size)`; the total APE region runs
///    from there to `apev2_upper_bound`. If the APE tag's header
///    flag bit indicates an additional 32-byte header is present
///    (the "has-header" bit, footer offset 20, bit 31 of the LE u32
///    flag field), the start offset is shifted 32 bytes earlier; the
///    `tag_size` declared in the footer covers the body + footer
///    only per the published APE spec, so a present header is added
///    on top.
pub fn detect_trailers(buf: &[u8], end_of_stream_offset: usize) -> TrailerInfo {
    let mut info = TrailerInfo::default();
    if end_of_stream_offset > buf.len() {
        return info;
    }
    let trailer_region = &buf[end_of_stream_offset..];
    if trailer_region.is_empty() {
        return info;
    }

    // ───── ID3v1: fixed 128 bytes at file end with magic "TAG". ─────
    let id3v1_present = trailer_region.len() >= 128
        && &trailer_region[trailer_region.len() - 128..trailer_region.len() - 128 + 3] == b"TAG";
    if id3v1_present {
        info.id3v1 = Some((buf.len() - 128, 128));
    }

    // APE search ends just before the ID3v1 trailer (if present).
    let ape_search_end = if id3v1_present {
        buf.len() - 128
    } else {
        buf.len()
    };
    if ape_search_end < end_of_stream_offset + 32 {
        return info;
    }

    // ───── APEv2: the 32-byte footer ends at `ape_search_end`. ─────
    let footer_start = ape_search_end - 32;
    let footer = &buf[footer_start..footer_start + 32];
    if &footer[..8] != b"APETAGEX" {
        return info;
    }
    // Footer layout per APE-tags-header (Hydrogenaudio wiki):
    //   bytes  0..8  : magic "APETAGEX"
    //   bytes  8..12 : version (LE u32; v2.0 = 2000)
    //   bytes 12..16 : tag_size (LE u32) — covers body + the 32-byte
    //                  footer; does NOT include the optional 32-byte
    //                  header that may precede the body.
    //   bytes 16..20 : item_count (LE u32)
    //   bytes 20..24 : flags (LE u32) — bit 31 = "has-header", bit 29
    //                  = "is-footer" (always 1 here), bit 30 =
    //                  "is-header" (always 0 here).
    //   bytes 24..32 : reserved (8 bytes, zero per spec).
    let tag_size = u32::from_le_bytes(footer[12..16].try_into().unwrap()) as usize;
    let flags = u32::from_le_bytes(footer[20..24].try_into().unwrap());
    let has_header = (flags & 0x8000_0000) != 0;
    let header_extra = if has_header { 32 } else { 0 };
    // `tag_size` already includes the 32-byte footer; total APE region
    // length is `tag_size + header_extra`. Sanity-bound it against the
    // available bytes past `end_of_stream_offset`.
    let total = tag_size.checked_add(header_extra).unwrap_or(0);
    if total < 32 || total > (ape_search_end - end_of_stream_offset) {
        return info;
    }
    let start = ape_search_end - total;
    info.apev2 = Some((start, total));
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal in-stream byte prefix so the trailer scanner has
    /// something to anchor against. The prefix is `eos` bytes of zeros
    /// representing a (fictitious) TTA1 in-stream region; the scanner
    /// must never look at these bytes.
    fn prefix(eos: usize) -> Vec<u8> {
        vec![0u8; eos]
    }

    #[test]
    fn no_trailers_when_buffer_ends_at_eos() {
        let buf = prefix(100);
        let info = detect_trailers(&buf, 100);
        assert!(info.is_empty());
        assert_eq!(info.id3v1, None);
        assert_eq!(info.apev2, None);
    }

    #[test]
    fn id3v1_detected_at_file_end() {
        let mut buf = prefix(50);
        // Build a 128-byte ID3v1 trailer: "TAG" + 125 bytes of padding.
        buf.extend_from_slice(b"TAG");
        buf.extend(std::iter::repeat(0u8).take(125));
        assert_eq!(buf.len(), 50 + 128);
        let info = detect_trailers(&buf, 50);
        assert_eq!(info.id3v1, Some((50, 128)));
        assert_eq!(info.apev2, None);
    }

    /// ID3v1 with "TAG" prefix but the 128-byte block intersects the
    /// in-stream region — should be rejected (we never count an in-
    /// stream byte as part of a trailer).
    #[test]
    fn id3v1_not_detected_when_eos_too_close_to_end() {
        let mut buf = prefix(200);
        buf.extend_from_slice(b"TAG");
        buf.extend(std::iter::repeat(0u8).take(20)); // only 23 trailer bytes
        let info = detect_trailers(&buf, 200);
        assert_eq!(info.id3v1, None);
    }

    /// Build a valid APEv2 footer (no header).
    fn build_ape_footer_only(item_count: u32, body_size: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + body_size);
        out.extend(std::iter::repeat(0xAAu8).take(body_size)); // body
                                                               // Footer (32 bytes).
        out.extend_from_slice(b"APETAGEX");
        out.extend_from_slice(&2000u32.to_le_bytes()); // version
        out.extend_from_slice(&((body_size + 32) as u32).to_le_bytes()); // tag_size = body + footer
        out.extend_from_slice(&item_count.to_le_bytes());
        // Flags: bit 31 = has_header(0), bit 30 = is_header(0), bit 29 = is_footer(1).
        let flags: u32 = 0x2000_0000;
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]); // reserved
        out
    }

    /// Build a valid APEv2 region with both header and footer.
    fn build_ape_with_header(item_count: u32, body_size: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + body_size);
        // Header (32 bytes).
        out.extend_from_slice(b"APETAGEX");
        out.extend_from_slice(&2000u32.to_le_bytes());
        out.extend_from_slice(&((body_size + 32) as u32).to_le_bytes()); // tag_size = body + footer
        out.extend_from_slice(&item_count.to_le_bytes());
        let header_flags: u32 = 0x8000_0000 | 0x4000_0000; // has_header + is_header
        out.extend_from_slice(&header_flags.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]);
        out.extend(std::iter::repeat(0xAAu8).take(body_size)); // body
                                                               // Footer (32 bytes).
        out.extend_from_slice(b"APETAGEX");
        out.extend_from_slice(&2000u32.to_le_bytes());
        out.extend_from_slice(&((body_size + 32) as u32).to_le_bytes());
        out.extend_from_slice(&item_count.to_le_bytes());
        let footer_flags: u32 = 0x8000_0000 | 0x2000_0000; // has_header + is_footer
        out.extend_from_slice(&footer_flags.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]);
        out
    }

    #[test]
    fn apev2_footer_only_detected() {
        let mut buf = prefix(200);
        let ape = build_ape_footer_only(5, 100);
        let ape_len = ape.len();
        buf.extend(ape);
        let info = detect_trailers(&buf, 200);
        assert_eq!(info.id3v1, None);
        assert_eq!(info.apev2, Some((200, ape_len)));
        assert_eq!(ape_len, 132); // 100 body + 32 footer
    }

    #[test]
    fn apev2_with_header_detected() {
        let mut buf = prefix(200);
        let ape = build_ape_with_header(3, 64);
        let ape_len = ape.len();
        buf.extend(ape);
        let info = detect_trailers(&buf, 200);
        assert_eq!(info.id3v1, None);
        assert_eq!(info.apev2, Some((200, ape_len)));
        assert_eq!(ape_len, 128); // 32 header + 64 body + 32 footer
    }

    #[test]
    fn both_trailers_detected_ape_before_id3v1() {
        let mut buf = prefix(200);
        let ape = build_ape_footer_only(2, 40);
        let ape_len = ape.len();
        buf.extend(ape);
        let ape_end_off = buf.len();
        // Append the ID3v1 trailer immediately after the APE region.
        buf.extend_from_slice(b"TAG");
        buf.extend(std::iter::repeat(0u8).take(125));
        let info = detect_trailers(&buf, 200);
        assert_eq!(info.id3v1, Some((ape_end_off, 128)));
        assert_eq!(info.apev2, Some((200, ape_len)));
    }

    #[test]
    fn apev2_bogus_tag_size_rejected() {
        // Footer claims a tag_size that overruns the EOS boundary —
        // must be rejected, not silently truncated into the TTA stream.
        let mut buf = prefix(20);
        // Body of only 10 bytes but footer declares tag_size = 200
        // (which would require 168 body bytes; way more than is
        // available past eos).
        let body_size = 10;
        let bogus_tag_size = 200u32;
        buf.extend(std::iter::repeat(0xAAu8).take(body_size));
        buf.extend_from_slice(b"APETAGEX");
        buf.extend_from_slice(&2000u32.to_le_bytes());
        buf.extend_from_slice(&bogus_tag_size.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&0x2000_0000u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        let info = detect_trailers(&buf, 20);
        assert_eq!(info.apev2, None);
    }

    #[test]
    fn detect_with_out_of_range_eos_yields_empty() {
        let buf = vec![0u8; 50];
        let info = detect_trailers(&buf, 100); // eos past buf len
        assert!(info.is_empty());
    }

    #[test]
    fn no_apev2_when_footer_magic_absent() {
        let mut buf = prefix(50);
        buf.extend(std::iter::repeat(0u8).take(64)); // no magic anywhere
        let info = detect_trailers(&buf, 50);
        assert!(info.is_empty());
    }

    // ───────── Id3v1Range / ApeV2Range typed-accessor tests ─────────

    #[test]
    fn id3v1_range_from_raw_accepts_anchored_128_byte_trailer() {
        // 128-byte ID3v1 anchored at file end of a 200-byte file:
        // start = 72, len = 128, end = 200 = file_len.
        let r = Id3v1Range::from_raw(72, 128, 200).expect("valid");
        assert_eq!(r.start(), 72);
        assert_eq!(r.len(), 128);
        assert_eq!(r.end(), 200);
        assert_eq!(r.byte_range(), 72..200);
        assert!(r.is_at_file_end(200));
        assert!(!r.is_at_file_end(201));
        // Spec §7 fixed-128 invariant ⇒ never empty.
        assert!(!r.is_empty());
    }

    #[test]
    fn id3v1_range_rejects_wrong_length() {
        // Length 127 / 129 / 0 / 64 / 256 all rejected — ID3v1 is
        // fixed-length 128 bytes per spec §7.
        for bad_len in [0usize, 1, 64, 127, 129, 256] {
            let start = 200_usize.saturating_sub(bad_len);
            let r = Id3v1Range::from_raw(start, bad_len, 200);
            assert_eq!(
                r,
                Err(Error::InvalidId3v1Range(start, bad_len)),
                "len {bad_len} should be rejected"
            );
        }
    }

    #[test]
    fn id3v1_range_rejects_not_anchored_at_file_end() {
        // start + 128 = 199 ≠ 200, so the range is one byte short
        // of the file end — must be rejected per spec §7's
        // "anchored at file end" rule.
        let r = Id3v1Range::from_raw(71, 128, 200);
        assert_eq!(r, Err(Error::InvalidId3v1Range(71, 128)));
    }

    #[test]
    fn id3v1_range_rejects_past_file_end() {
        // start + 128 > file_len: trailer would address bytes past EOF.
        let r = Id3v1Range::from_raw(150, 128, 200);
        assert_eq!(r, Err(Error::InvalidId3v1Range(150, 128)));
    }

    #[test]
    fn id3v1_range_rejects_arithmetic_overflow() {
        // start + len would overflow usize.
        let r = Id3v1Range::from_raw(usize::MAX, 128, usize::MAX);
        assert_eq!(r, Err(Error::InvalidId3v1Range(usize::MAX, 128)));
    }

    #[test]
    fn ape_v2_range_from_raw_accepts_footer_only() {
        // Minimum legal APEv2 region — just the 32-byte footer.
        let r = ApeV2Range::from_raw(168, 32, 200).expect("valid");
        assert_eq!(r.start(), 168);
        assert_eq!(r.len(), 32);
        assert_eq!(r.end(), 200);
        assert_eq!(r.byte_range(), 168..200);
        assert!(r.is_at_file_end(200));
        assert_eq!(r.header_and_body_size(), 0);
        // Spec §7 ≥ 32 invariant ⇒ never empty.
        assert!(!r.is_empty());
    }

    #[test]
    fn ape_v2_range_from_raw_accepts_body_plus_footer() {
        // 100-byte body + 32-byte footer.
        let r = ApeV2Range::from_raw(68, 132, 200).expect("valid");
        assert_eq!(r.len(), 132);
        assert_eq!(r.header_and_body_size(), 100);
    }

    #[test]
    fn ape_v2_range_from_raw_accepts_header_body_footer() {
        // 32-byte header + 64-byte body + 32-byte footer = 128 bytes.
        let r = ApeV2Range::from_raw(72, 128, 200).expect("valid");
        assert_eq!(r.len(), 128);
        // header_and_body_size = 128 - 32 = 96 (includes the optional header).
        assert_eq!(r.header_and_body_size(), 96);
    }

    #[test]
    fn ape_v2_range_accepts_not_anchored_at_file_end() {
        // APE region sitting 128 bytes before the file end (e.g. when
        // a 128-byte ID3v1 trailer follows it per spec §7's "APE
        // immediately before ID3v1" rule). Must accept.
        let r = ApeV2Range::from_raw(40, 32, 200).expect("valid");
        assert!(!r.is_at_file_end(200));
        // But IS at file end of a 72-byte file:
        assert!(r.is_at_file_end(72));
    }

    #[test]
    fn ape_v2_range_rejects_below_footer_minimum() {
        for bad_len in [0usize, 1, 16, 31] {
            let r = ApeV2Range::from_raw(0, bad_len, 200);
            assert_eq!(
                r,
                Err(Error::InvalidApeV2Range(0, bad_len)),
                "len {bad_len} should be rejected"
            );
        }
    }

    #[test]
    fn ape_v2_range_rejects_past_file_end() {
        let r = ApeV2Range::from_raw(180, 64, 200);
        assert_eq!(r, Err(Error::InvalidApeV2Range(180, 64)));
    }

    #[test]
    fn ape_v2_range_rejects_arithmetic_overflow() {
        let r = ApeV2Range::from_raw(usize::MAX, 32, usize::MAX);
        assert_eq!(r, Err(Error::InvalidApeV2Range(usize::MAX, 32)));
    }

    #[test]
    fn trailer_info_typed_accessors_return_none_on_empty() {
        let info = TrailerInfo::default();
        assert_eq!(info.id3v1_typed(0).unwrap(), None);
        assert_eq!(info.apev2_typed(0).unwrap(), None);
        assert_eq!(info.combined_byte_range(), None);
    }

    #[test]
    fn trailer_info_typed_accessors_from_parser_id3v1() {
        // Build a real ID3v1-only buffer and lift it via the parser.
        let mut buf = prefix(50);
        buf.extend_from_slice(b"TAG");
        buf.extend(std::iter::repeat(0u8).take(125));
        let file_len = buf.len(); // = 178
        let info = detect_trailers(&buf, 50);
        let id3 = info.id3v1_typed(file_len).unwrap().expect("present");
        assert_eq!(id3.start(), file_len - 128);
        assert_eq!(id3.len(), 128);
        assert!(id3.is_at_file_end(file_len));
        assert_eq!(info.apev2_typed(file_len).unwrap(), None);
        assert_eq!(info.combined_byte_range(), Some((50, 128)));
    }

    #[test]
    fn trailer_info_typed_accessors_from_parser_apev2_only() {
        let mut buf = prefix(200);
        let ape = build_ape_footer_only(5, 100);
        let ape_len = ape.len();
        buf.extend(ape);
        let file_len = buf.len();
        let info = detect_trailers(&buf, 200);
        let ape_typed = info.apev2_typed(file_len).unwrap().expect("present");
        assert_eq!(ape_typed.start(), 200);
        assert_eq!(ape_typed.len(), ape_len);
        assert!(ape_typed.is_at_file_end(file_len));
        assert_eq!(ape_typed.header_and_body_size(), 100);
        assert_eq!(info.id3v1_typed(file_len).unwrap(), None);
        assert_eq!(info.combined_byte_range(), Some((200, ape_len)));
    }

    #[test]
    fn trailer_info_typed_accessors_from_parser_both() {
        let mut buf = prefix(200);
        let ape = build_ape_footer_only(2, 40);
        let ape_len = ape.len();
        buf.extend(ape);
        let ape_end_off = buf.len();
        buf.extend_from_slice(b"TAG");
        buf.extend(std::iter::repeat(0u8).take(125));
        let file_len = buf.len();
        let info = detect_trailers(&buf, 200);
        let id3 = info.id3v1_typed(file_len).unwrap().expect("present");
        let ape = info.apev2_typed(file_len).unwrap().expect("present");
        assert!(id3.is_at_file_end(file_len));
        assert!(!ape.is_at_file_end(file_len));
        // Combined window should cover both regions contiguously
        // (APE immediately precedes ID3v1 per spec §7).
        let combined = info.combined_byte_range().unwrap();
        assert_eq!(combined.0, 200);
        assert_eq!(combined.1, ape_len + 128);
        assert_eq!(combined.0 + combined.1, file_len);
        // Sanity-check: APE end == ID3v1 start.
        assert_eq!(ape.end(), id3.start());
        assert_eq!(ape.end(), ape_end_off);
    }

    #[test]
    fn trailer_info_typed_accessors_reject_hand_built_literals() {
        // Hand-build a TrailerInfo with a wrong-length ID3v1 entry —
        // typed accessor must reject at lift time.
        let info = TrailerInfo {
            id3v1: Some((10, 64)), // wrong length
            apev2: None,
        };
        assert_eq!(info.id3v1_typed(74), Err(Error::InvalidId3v1Range(10, 64)));
        // APE with sub-footer-minimum length — reject.
        let info2 = TrailerInfo {
            id3v1: None,
            apev2: Some((10, 16)),
        };
        assert_eq!(
            info2.apev2_typed(100),
            Err(Error::InvalidApeV2Range(10, 16))
        );
    }
}
