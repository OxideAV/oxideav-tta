//! `oxideav-core` framework integration: codec + container
//! registration, plus the [`oxideav_core::Decoder`] implementation
//! wrapping the crate's existing per-frame decoder.
//!
//! Compiled only when the default-on `registry` Cargo feature is
//! enabled. Standalone consumers (`default-features = false`) do not
//! pull in `oxideav-core` and skip this module entirely.

#![cfg(feature = "registry")]

use oxideav_core::{
    AudioFrame, CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry,
    CodecResolver, ContainerRegistry, Decoder, Demuxer, Error as CoreError, Frame, MediaType,
    Packet, ProbeData, ReadSeek, Result as CoreResult, RuntimeContext, SampleFormat, StreamInfo,
    TimeBase,
};
use std::io::Read;

use crate::header::{
    parse_seek_table, parse_stream_header_any_format, skip_id3v2_prefix, FrameDescriptor,
    StreamHeader,
};

/// Canonical codec id string for True Audio. `oxideav-meta`'s
/// `register_all` calls `crate::__oxideav_entry`, which delegates
/// here; `oxideav_pipeline::make_decoder_with` looks the codec up by
/// this id.
pub const CODEC_ID_STR: &str = "tta";

/// Register the TTA codec with `reg`.
pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::audio("tta_sw")
        .with_lossless(true)
        .with_intra_only(true)
        .with_max_channels(6)
        .with_max_sample_rate(0x007F_FFFF);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder),
    );
}

/// Register the TTA1 raw-file demuxer with `reg`.
pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("tta", open_demuxer);
    reg.register_extension("tta", "tta");
    reg.register_probe("tta", probe);
}

/// Unified entry point invoked by the macro-generated wrapper.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

// ───────────────────────── Decoder impl ─────────────────────────

fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    let output_format = params
        .sample_format
        .ok_or_else(|| CoreError::invalid("oxideav-tta: sample_format missing on stream"))?;
    // The TTA1 stream itself self-declares bits_per_sample (16 or 24);
    // the decoder pulls that out of the stream header on the hot
    // path. Mapping the framework's SampleFormat (S16/S24) onto an
    // expected bps is purely a sanity rail — the decoder will surface
    // a clear error if the stream disagrees with what the demuxer
    // configured.
    let expected_bps = match output_format {
        SampleFormat::S16 => 16u16,
        SampleFormat::S24 => 24u16,
        other => {
            return Err(CoreError::unsupported(format!(
                "oxideav-tta: unsupported output sample format {other:?}"
            )))
        }
    };
    let channels = params
        .channels
        .ok_or_else(|| CoreError::invalid("oxideav-tta: channels missing on stream"))?;
    Ok(Box::new(TtaDecoder {
        codec_id: params.codec_id.clone(),
        output_format,
        channels,
        bits_per_sample: expected_bps,
        pending: None,
        eof: false,
    }))
}

struct TtaDecoder {
    codec_id: CodecId,
    output_format: SampleFormat,
    channels: u16,
    bits_per_sample: u16,
    pending: Option<Packet>,
    eof: bool,
}

impl Decoder for TtaDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if self.pending.is_some() {
            return Err(CoreError::other(
                "oxideav-tta: receive_frame must be called before sending another packet",
            ));
        }
        self.pending = Some(packet.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        let Some(pkt) = self.pending.take() else {
            return if self.eof {
                Err(CoreError::Eof)
            } else {
                Err(CoreError::NeedMore)
            };
        };
        // Each demuxed packet carries one full TTA file (raw
        // container), so the simplest decode path is a one-shot
        // `decode` call. The packet stream is one-packet-per-stream
        // for this raw container, mirroring a single-frame audio
        // file.
        let (info, samples) = crate::decode(&pkt.data)
            .map_err(|e| CoreError::invalid(format!("oxideav-tta: {e}")))?;
        if info.channels != self.channels {
            return Err(CoreError::invalid(format!(
                "oxideav-tta: stream has {} channels but decoder configured for {}",
                info.channels, self.channels
            )));
        }
        if info.bits_per_sample != self.bits_per_sample {
            return Err(CoreError::invalid(format!(
                "oxideav-tta: stream has {} bps but decoder configured for {}",
                info.bits_per_sample, self.bits_per_sample
            )));
        }
        let bytes = pcm_pack_for_format(&samples, self.output_format)?;
        Ok(Frame::Audio(AudioFrame {
            samples: info.total_samples,
            pts: pkt.pts,
            data: vec![bytes],
        }))
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.eof = true;
        Ok(())
    }
}

/// Repack interleaved `i32` PCM into the byte layout the AudioFrame
/// expects for `output_format`.
fn pcm_pack_for_format(samples: &[i32], output_format: SampleFormat) -> CoreResult<Vec<u8>> {
    let bytes_per_sample = output_format.bytes_per_sample();
    let mut out = Vec::with_capacity(samples.len() * bytes_per_sample);
    match output_format {
        SampleFormat::S16 => {
            for &s in samples {
                out.extend_from_slice(&(s as i16).to_le_bytes());
            }
        }
        SampleFormat::S24 => {
            for &s in samples {
                let v = s & 0x00FF_FFFF;
                out.push((v & 0xFF) as u8);
                out.push(((v >> 8) & 0xFF) as u8);
                out.push(((v >> 16) & 0xFF) as u8);
            }
        }
        other => {
            return Err(CoreError::unsupported(format!(
                "oxideav-tta: unsupported output sample format {other:?}"
            )))
        }
    }
    Ok(out)
}

// ───────────────────────── Demuxer impl ─────────────────────────

fn probe(p: &ProbeData) -> u8 {
    if p.buf.len() >= 4 && &p.buf[0..4] == b"TTA1" {
        return 100;
    }
    if p.buf.len() >= 14 && &p.buf[0..3] == b"ID3" {
        let size = ((p.buf[6] as usize) << 21)
            | ((p.buf[7] as usize) << 14)
            | ((p.buf[8] as usize) << 7)
            | (p.buf[9] as usize);
        let off = 10 + size;
        if off + 4 <= p.buf.len() && &p.buf[off..off + 4] == b"TTA1" {
            return 100;
        }
    }
    0
}

/// Crate-internal alias for `open_demuxer` used by the in-tree seek
/// tests. The codec registry only exposes the factory by function
/// pointer; tests want to call it directly so they can keep a typed
/// handle on the result rather than going through the
/// `ContainerRegistry` indirection.
#[cfg(test)]
pub(crate) fn open_demuxer_for_test(
    input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> CoreResult<Box<dyn Demuxer>> {
    open_demuxer(input, codecs)
}

/// Raw `.tta` demuxer: parses the TTA1 header + seek table at open
/// time, then emits one packet per audio frame. Each packet is a
/// self-contained mini-TTA1 file (re-prefixed header + 1-entry seek
/// table + the frame body) so the existing single-file `TtaDecoder`
/// can decode it without alteration.
///
/// Because TTA1 carries a complete byte-offset seek table in the file
/// header (per `spec/01-bitstream-framing.md` §4), `seek_to` is O(1):
/// the target frame is `pts / regular_frame_samples`, and its byte
/// offset is the pre-computed cumulative sum of seek-table entries up
/// to that index.
fn open_demuxer(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> CoreResult<Box<dyn Demuxer>> {
    let mut all = Vec::new();
    input.read_to_end(&mut all)?;

    // Skip optional ID3v2 prefix and parse the header for stream info.
    let id3_skip = skip_id3v2_prefix(&all)
        .map_err(|e| CoreError::invalid(format!("oxideav-tta demuxer: {e}")))?;
    let after_id3_off = id3_skip;
    let after_id3 = &all[after_id3_off..];
    let (header, hdr_len) = parse_stream_header_any_format(after_id3)
        .map_err(|e| CoreError::invalid(format!("oxideav-tta demuxer: {e}")))?;
    // Parse the seek table — we need the per-frame byte sizes to emit
    // one packet per frame and to fast-path seek_to.
    let (frame_count, _last_samples) = header.frame_geometry();
    let seek_table_len = (frame_count as usize) * 4 + 4;
    let frame_data_start = (after_id3_off + hdr_len + seek_table_len) as u64;
    let (seek_table, _seek_consumed) =
        parse_seek_table(&after_id3[hdr_len..], &header, frame_data_start)
            .map_err(|e| CoreError::invalid(format!("oxideav-tta demuxer: seek-table: {e}")))?;

    let sample_format = match header.bits_per_sample {
        16 => SampleFormat::S16,
        17..=24 => SampleFormat::S24,
        other => {
            return Err(CoreError::unsupported(format!(
                "oxideav-tta demuxer: unsupported bps {other}"
            )));
        }
    };

    let mut params = CodecParameters::audio(CodecId::new(CODEC_ID_STR));
    params.media_type = MediaType::Audio;
    params.channels = Some(header.channels);
    params.sample_rate = Some(header.sample_rate);
    params.sample_format = Some(sample_format);

    let time_base = TimeBase::new(1, header.sample_rate as i64);
    let stream = StreamInfo {
        index: 0,
        time_base,
        duration: Some(header.total_samples as i64),
        start_time: Some(0),
        params,
    };

    let duration_micros: i64 = if header.sample_rate > 0 && header.total_samples > 0 {
        (header.total_samples as i128 * 1_000_000 / header.sample_rate as i128) as i64
    } else {
        0
    };

    let regular_samples = header.regular_frame_samples() as i64;

    Ok(Box::new(TtaDemuxer {
        streams: vec![stream],
        all,
        header,
        frames: seek_table.frames,
        regular_samples,
        current_frame: 0,
        next_pts: 0,
        duration_micros,
    }))
}

struct TtaDemuxer {
    streams: Vec<StreamInfo>,
    /// The full file bytes (including any ID3v2 prefix). Frame
    /// descriptors carry absolute byte offsets into this buffer.
    all: Vec<u8>,
    /// Parsed TTA1 stream header.
    header: StreamHeader,
    /// One descriptor per audio frame, ordered, with absolute
    /// `file_offset` into `all` and the on-disk `disk_size` (body +
    /// 4-byte trailing CRC).
    frames: Vec<FrameDescriptor>,
    /// `regular_frame_samples()` cached as i64 for pts arithmetic.
    /// All non-last frames carry exactly this many per-channel samples
    /// per spec §4.1; the last frame may be shorter.
    regular_samples: i64,
    /// Index of the next frame `next_packet` will emit. Reset by
    /// `seek_to`.
    current_frame: usize,
    /// pts (in samples = time_base 1/sample_rate) that the next
    /// emitted packet will carry. For frame N this is
    /// `N * regular_samples`. Reset by `seek_to`.
    next_pts: i64,
    duration_micros: i64,
}

impl TtaDemuxer {
    /// Build a self-contained TTA1 byte sequence carrying exactly one
    /// frame (the frame at `frame_idx`). This lets the existing
    /// single-file `TtaDecoder` consume each demuxer packet without
    /// modification: the mini-file re-uses the parsed header fields
    /// (channels / bps / sample_rate / format) and rewrites
    /// `total_samples` to that frame's sample count, which the header
    /// parser + seek-table parser will agree on (`frame_geometry`
    /// then returns `(1, sample_count)`).
    fn build_single_frame_file(&self, frame_idx: usize) -> Vec<u8> {
        let frame = &self.frames[frame_idx];
        let body_off = frame.file_offset as usize;
        let body_end = body_off + frame.disk_size as usize;
        let frame_bytes = &self.all[body_off..body_end];

        let mut out = Vec::with_capacity(22 + 8 + frame_bytes.len());
        // 22-byte stream header. Spec/01 §3: magic + 18 bytes of meta
        // (format, channels, bps, sample_rate, total_samples) + CRC32.
        out.extend_from_slice(b"TTA1");
        out.extend_from_slice(&self.header.format.to_le_bytes());
        out.extend_from_slice(&self.header.channels.to_le_bytes());
        out.extend_from_slice(&self.header.bits_per_sample.to_le_bytes());
        out.extend_from_slice(&self.header.sample_rate.to_le_bytes());
        let mini_total: u32 = frame.sample_count;
        out.extend_from_slice(&mini_total.to_le_bytes());
        let header_crc = crate::crc32::crc32(&out[..18]);
        out.extend_from_slice(&header_crc.to_le_bytes());

        // 1-entry seek table.
        let seek_start = out.len();
        out.extend_from_slice(&frame.disk_size.to_le_bytes());
        let seek_crc = crate::crc32::crc32(&out[seek_start..seek_start + 4]);
        out.extend_from_slice(&seek_crc.to_le_bytes());

        // Frame data (body + trailing CRC, byte-for-byte).
        out.extend_from_slice(frame_bytes);
        out
    }
}

impl Demuxer for TtaDemuxer {
    fn format_name(&self) -> &str {
        "tta"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> CoreResult<Packet> {
        if self.current_frame >= self.frames.len() {
            return Err(CoreError::Eof);
        }
        let frame_idx = self.current_frame;
        let frame_samples = self.frames[frame_idx].sample_count as i64;
        let data = self.build_single_frame_file(frame_idx);

        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(self.next_pts);
        pkt.dts = Some(self.next_pts);
        pkt.duration = Some(frame_samples);
        pkt.flags.keyframe = true;

        self.current_frame += 1;
        self.next_pts += self.regular_samples;
        Ok(pkt)
    }

    /// Seek to the audio frame whose sample range contains `pts`.
    ///
    /// TTA1's built-in seek table makes this an O(1) lookup: every
    /// non-last frame contains exactly `regular_frame_samples`
    /// per-channel samples (`floor(sample_rate * 256 / 245)`,
    /// `spec/01-bitstream-framing.md` §4.1), so the containing frame
    /// is simply `pts / regular_samples`. We then reset `current_frame`
    /// and re-anchor `next_pts` to the frame's first sample so the
    /// subsequent `next_packet` call reproduces the post-seek pts
    /// stream from a known frame boundary.
    ///
    /// The codec's per-channel LMS / Stage-B / Rice state is reset at
    /// every frame boundary by construction (`spec/02-stage-a-lms.md`
    /// §3.1, `spec/03-stage-b-recursive.md` §3, `spec/05-adaptive-rice.md`
    /// §3) — so the demuxer doesn't have to coordinate decoder reset:
    /// the next mini-file packet the decoder receives starts a fresh
    /// decoder run.
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> CoreResult<i64> {
        if stream_index != 0 {
            return Err(CoreError::invalid(format!(
                "oxideav-tta: stream index {stream_index} out of range (only stream 0 exists)"
            )));
        }
        if self.frames.is_empty() || self.regular_samples == 0 {
            return Err(CoreError::unsupported(
                "oxideav-tta: cannot seek in a zero-frame stream",
            ));
        }
        let n_frames = self.frames.len() as u64;
        let raw_target = pts.max(0) as u64;
        let mut target_frame = raw_target / self.regular_samples as u64;
        if target_frame >= n_frames {
            target_frame = n_frames - 1;
        }
        self.current_frame = target_frame as usize;
        let landed_pts = (target_frame as i64) * self.regular_samples;
        self.next_pts = landed_pts;
        Ok(landed_pts)
    }

    fn duration_micros(&self) -> Option<i64> {
        Some(self.duration_micros)
    }
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Arc;

    use crate::encoder::encode_to_tta1;

    struct NoopResolver;
    impl CodecResolver for NoopResolver {
        fn resolve_tag(&self, _ctx: &oxideav_core::ProbeContext) -> Option<CodecId> {
            None
        }
    }

    fn synth_tta_file() -> Vec<u8> {
        // 0.05 s of mono 16-bit silence at 44.1 kHz → one frame.
        let n = 2_048;
        let samples = vec![0i32; n];
        encode_to_tta1(&samples, 1, 16, 44_100)
    }

    #[test]
    fn register_via_runtime_context_installs_codec_and_container() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let codec_id = CodecId::new(CODEC_ID_STR);
        assert!(
            ctx.codecs.has_decoder(&codec_id),
            "codec registration should install a decoder factory"
        );
        let mut found = false;
        for name in ctx.containers.demuxer_names() {
            if name == "tta" {
                found = true;
                break;
            }
        }
        assert!(found, "container registration should install a demuxer");
    }

    #[test]
    fn end_to_end_demux_then_decode() {
        let bytes = synth_tta_file();
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);

        // Probe → open demuxer.
        let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
        let mut demuxer = open_demuxer(cursor, &NoopResolver).expect("open_demuxer");
        let stream = demuxer.streams()[0].clone();
        let _ = Arc::new(stream.clone());
        // Pull one packet out.
        let pkt = demuxer.next_packet().expect("next_packet");
        // Build the decoder via the registry.
        let mut dec = ctx
            .codecs
            .first_decoder(&stream.params)
            .expect("first_decoder");
        dec.send_packet(&pkt).expect("send_packet");
        let frame = dec.receive_frame().expect("receive_frame");
        match frame {
            Frame::Audio(a) => {
                assert_eq!(a.samples, 2_048);
                // 2 bytes per S16 sample × 1 channel = 4096.
                assert_eq!(a.data.len(), 1);
                assert_eq!(a.data[0].len(), 4_096);
                // Silence → all zeros.
                assert!(a.data[0].iter().all(|&b| b == 0));
            }
            other => panic!("expected audio frame, got {other:?}"),
        }
    }
}
