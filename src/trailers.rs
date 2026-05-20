//! Optional ID3v1 / APEv2 trailer detection per `spec/01` §7.
//!
//! TTA1's framing layer ends at the last frame's trailing CRC. Any
//! bytes that appear past `frame_start[N-1] + seek_table_entry[N-1]`
//! are out-of-stream metadata containers — libtta itself parses
//! neither, but a TTA-aware host application typically wants to know
//! whether the file carries an ID3v1 trailer, an APEv2 trailer, both,
//! or neither, so it can preserve them on re-encode.
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
}
