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

use crate::header::{parse_seek_table, parse_stream_header_any_format, skip_id3v2_prefix};

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

/// Raw `.tta` demuxer: slurp the entire file into one packet that
/// the decoder treats as a single self-contained TTA1 stream. Frame-
/// boundary aware streaming demuxing (one packet per TTA frame keyed
/// off the seek table) is feasible but not needed for the round-2
/// integration milestone — the codec self-decodes the whole file in
/// one call already.
fn open_demuxer(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> CoreResult<Box<dyn Demuxer>> {
    let mut all = Vec::new();
    input.read_to_end(&mut all)?;

    // Skip optional ID3v2 prefix and parse the header for stream info.
    let id3_skip = skip_id3v2_prefix(&all)
        .map_err(|e| CoreError::invalid(format!("oxideav-tta demuxer: {e}")))?;
    let after_id3 = &all[id3_skip..];
    let (header, hdr_len) = parse_stream_header_any_format(after_id3)
        .map_err(|e| CoreError::invalid(format!("oxideav-tta demuxer: {e}")))?;
    // Validate the seek table; the demuxer doesn't use the entries
    // since it ships the whole file as one packet, but a
    // CRC-mismatch is a clean way to surface a corrupted stream
    // before the codec tries to decode it.
    let (frame_count, _) = header.frame_geometry();
    let seek_len = (frame_count as usize) * 4 + 4;
    let _ = parse_seek_table(
        &after_id3[hdr_len..],
        &header,
        (id3_skip + hdr_len + seek_len) as u64,
    )
    .map_err(|e| CoreError::invalid(format!("oxideav-tta demuxer: seek-table parse: {e}")))?;

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

    Ok(Box::new(TtaDemuxer {
        streams: vec![stream],
        all,
        emitted: false,
        duration_micros,
    }))
}

struct TtaDemuxer {
    streams: Vec<StreamInfo>,
    all: Vec<u8>,
    emitted: bool,
    duration_micros: i64,
}

impl Demuxer for TtaDemuxer {
    fn format_name(&self) -> &str {
        "tta"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> CoreResult<Packet> {
        if self.emitted {
            return Err(CoreError::Eof);
        }
        self.emitted = true;
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, std::mem::take(&mut self.all));
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.duration = stream.duration;
        pkt.flags.keyframe = true;
        Ok(pkt)
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
