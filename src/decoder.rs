//! Per-frame decoder orchestration.
//!
//! Glues the bit reader (`spec/05` §2), the adaptive Rice decoder
//! (`spec/05` §3..5), the Stage-A LMS (`spec/02`), the Stage-B
//! recursive predictor (`spec/03`), and the inverse channel
//! decorrelation cascade (`spec/04`) into a single per-step pipeline,
//! then verifies the trailing per-frame CRC32 (`spec/01` §5.4).

use crate::bitreader::BitReader;
use crate::decorr;
use crate::error::{Error, Result};
use crate::header::{FrameDescriptor, StreamHeader};
use crate::lms::LmsState;
use crate::rice::{self, RiceState};
use crate::stage_b::StageBState;

/// One per-channel pipeline state bundle.
struct ChannelState {
    rice: RiceState,
    lms: LmsState,
    stage_b: StageBState,
}

/// Decode a single frame from its on-disk byte block (body + trailing
/// CRC). Returns the `samples_per_frame * channels` interleaved signed
/// samples in channel order `(c0_s0, c1_s0, ..., c0_s1, c1_s1, ...)`.
///
/// The output range is `i32` to give every supported bps (16, 24)
/// headroom; the caller packs into the appropriate PCM byte layout
/// per `spec/01` §3.2.
pub fn decode_frame(
    header: &StreamHeader,
    descriptor: &FrameDescriptor,
    frame_bytes: &[u8],
) -> Result<Vec<i32>> {
    let disk = descriptor.disk_size as usize;
    if frame_bytes.len() < disk {
        return Err(Error::Truncated);
    }
    if disk < 4 {
        return Err(Error::Truncated);
    }
    let body_len = disk - 4;
    let body = &frame_bytes[..body_len];
    let crc_bytes = &frame_bytes[body_len..body_len + 4];
    let stored_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());

    let nch = header.channels as usize;
    let bytes_per_sample = header.bytes_per_sample();
    let samples_per_frame = descriptor.sample_count as usize;
    let mut out = vec![0i32; samples_per_frame * nch];

    let mut reader = BitReader::new(body);
    let mut channels: Vec<ChannelState> = (0..nch)
        .map(|_| ChannelState {
            rice: RiceState::frame_init(),
            lms: LmsState::frame_init(bytes_per_sample),
            stage_b: StageBState::frame_init(),
        })
        .collect();

    // Per-step inner loop: for each PCM sample slot, decode every
    // channel's Rice -> Stage-A -> Stage-B in turn into a scratch
    // buffer, then run the inverse decorrelation cascade in place,
    // then write into `out` interleaved.
    let mut scratch: Vec<i32> = vec![0; nch];
    for sample_idx in 0..samples_per_frame {
        for ch in 0..nch {
            let cs = &mut channels[ch];
            let e = rice::decode_one(&mut reader, &mut cs.rice)?;
            let s_a = cs.lms.step(e);
            let s_b = cs.stage_b.step(s_a);
            scratch[ch] = s_b;
        }
        decorr::inverse(&mut scratch);
        let base = sample_idx * nch;
        out[base..base + nch].copy_from_slice(&scratch);
    }

    // CRC verification — the bit reader has folded every byte the
    // entropy decoder consumed into its CRC register. Per spec §5.3,
    // the encoder pads the bit cache up to the next byte boundary
    // before writing the trailing CRC; therefore the decoder may have
    // up to 7 unread bits in its cache once `samples_per_frame * nch`
    // residuals have been produced. The CRC is over BYTES (not bits),
    // so any body bytes the cache has already drawn from but which the
    // residual budget did not technically "consume" must still fold
    // into the CRC register. Walk any remaining body bytes to do so.
    let consumed = reader.bytes_consumed();
    let mut crc = reader.crc_state();
    if consumed < body_len {
        for &b in &body[consumed..body_len] {
            crc.update_byte(b);
        }
    }
    if crc.finalize() != stored_crc {
        return Err(Error::Crc32Mismatch { region: "frame" });
    }

    Ok(out)
}

/// Convenience structure: parse the header + seek table out of a
/// `&[u8]` slice and return a closure-able decoder. Used by both the
/// public `decode` entry point and by integration tests.
#[derive(Debug, Clone)]
pub struct Decoder<'a> {
    pub header: StreamHeader,
    pub frames: Vec<FrameDescriptor>,
    /// `true` if the seek-table CRC matched. (Kept alive but unused
    /// by the linear-decode path; consumers can warn if they care.)
    pub seek_table_crc_ok: bool,
    pub bytes: &'a [u8],
}

impl<'a> Decoder<'a> {
    /// Parse `bytes` as a TTA1 file (with optional ID3v2 prefix) and
    /// return a [`Decoder`] ready to walk the frames.
    pub fn new(bytes: &'a [u8]) -> Result<Self> {
        let id3_skip = crate::header::skip_id3v2_prefix(bytes)?;
        let after_id3 = &bytes[id3_skip..];
        let (header, hdr_len) = crate::header::parse_stream_header(after_id3)?;
        let seek_table_input = &after_id3[hdr_len..];
        let seek_base = (id3_skip + hdr_len) as u64;
        // Compute frame data start: after header + entire seek table.
        let (frame_count, _) = header.frame_geometry();
        let seek_table_len = (frame_count as usize) * 4 + 4;
        let frame_data_start = seek_base + seek_table_len as u64;
        let (seek_table, _seek_consumed) =
            crate::header::parse_seek_table(seek_table_input, &header, frame_data_start)?;
        Ok(Self {
            header,
            frames: seek_table.frames,
            seek_table_crc_ok: seek_table.crc_ok,
            bytes,
        })
    }

    /// Decode every frame and return interleaved `i32` PCM samples
    /// for the entire stream (`total_samples * channels` entries).
    pub fn decode_all(&self) -> Result<Vec<i32>> {
        let mut out = Vec::with_capacity(
            (self.header.total_samples as usize) * (self.header.channels as usize),
        );
        for frame in &self.frames {
            let off = frame.file_offset as usize;
            let end = off + frame.disk_size as usize;
            if end > self.bytes.len() {
                return Err(Error::Truncated);
            }
            let frame_bytes = &self.bytes[off..end];
            let pcm = decode_frame(&self.header, frame, frame_bytes)?;
            out.extend_from_slice(&pcm);
        }
        Ok(out)
    }
}
