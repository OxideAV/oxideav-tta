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

#[cfg(feature = "trace")]
use crate::trace::TraceWriter;

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
    decode_frame_inner(
        header,
        descriptor,
        frame_bytes,
        /* qm_priming */ None,
        /* frame_idx */ 0,
        #[cfg(feature = "trace")]
        None,
    )
}

/// Internal frame-decode entry point used by both the public
/// [`decode_frame`] and the `Decoder::decode_all` loop. The latter
/// supplies a `frame_idx` for the trace counters and (when the
/// `trace` feature is on) an `&mut TraceWriter` to emit events into.
fn decode_frame_inner(
    header: &StreamHeader,
    descriptor: &FrameDescriptor,
    frame_bytes: &[u8],
    qm_priming: Option<&[i32; 8]>,
    frame_idx: u32,
    #[cfg(feature = "trace")] trace: Option<&mut TraceWriter>,
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
        .map(|_| {
            let mut lms = LmsState::frame_init(bytes_per_sample);
            // Spec/07 §3.5 — for format=2, the per-frame Stage-A
            // reset re-primes qm[0..7] with the password digest
            // (sign-extended int8 → int32) instead of zeros. All
            // other fields keep the format=1 zero-init.
            if let Some(prime) = qm_priming {
                lms.qm = *prime;
            }
            ChannelState {
                rice: RiceState::frame_init(),
                lms,
                stage_b: StageBState::frame_init(),
            }
        })
        .collect();

    #[cfg(feature = "trace")]
    let mut trace = trace;
    #[cfg(feature = "trace")]
    if let Some(t) = trace.as_mut() {
        t.ev_frame_begin(frame_idx, samples_per_frame as u32);
    }

    // Per-step inner loop: for each PCM sample slot, decode every
    // channel's Rice -> Stage-A -> Stage-B in turn into a scratch
    // buffer, then run the inverse decorrelation cascade in place,
    // then write into `out` interleaved.
    let mut scratch: Vec<i32> = vec![0; nch];
    #[cfg(feature = "trace")]
    let mut step_idx: u32 = 0;
    for sample_idx in 0..samples_per_frame {
        for ch in 0..nch {
            let cs = &mut channels[ch];

            #[cfg(feature = "trace")]
            {
                let rice_t = rice::decode_one_traced(&mut reader, &mut cs.rice)?;
                if let Some(t) = &mut trace {
                    t.ev_rice_decode(
                        frame_idx,
                        step_idx,
                        ch as u32,
                        rice_t.raw_unary,
                        rice_t.mode_high,
                        rice_t.k_used,
                        rice_t.residual_signed,
                    );
                    t.ev_rice_k_update(
                        frame_idx,
                        step_idx,
                        ch as u32,
                        rice_t.k0_post,
                        rice_t.k1_post,
                        rice_t.sum0_post,
                        rice_t.sum1_post,
                    );
                }
                let lms_t = cs.lms.step_traced(rice_t.residual_signed);
                if let Some(t) = &mut trace {
                    t.ev_lms_pre(
                        frame_idx,
                        step_idx,
                        ch as u32,
                        &lms_t.dl_pre,
                        &lms_t.dx_pre,
                        &lms_t.qm_pre,
                    );
                    t.ev_stage_a_predict(
                        frame_idx,
                        step_idx,
                        ch as u32,
                        lms_t.predicted_a,
                        rice_t.residual_signed,
                        lms_t.sample_after_a,
                    );
                    t.ev_lms_post(
                        frame_idx,
                        step_idx,
                        ch as u32,
                        &lms_t.dl_post,
                        &lms_t.dx_post,
                        &lms_t.qm_post,
                        lms_t.error_pre,
                    );
                }
                let sb_t = cs.stage_b.step_traced(lms_t.sample_after_a);
                if let Some(t) = &mut trace {
                    t.ev_stage_b_predict(
                        frame_idx,
                        step_idx,
                        ch as u32,
                        sb_t.prev_in,
                        sb_t.predicted_b,
                        sb_t.residual_b,
                        sb_t.sample_after_b,
                    );
                }
                scratch[ch] = sb_t.sample_after_b;
                step_idx += 1;
            }

            #[cfg(not(feature = "trace"))]
            {
                let e = rice::decode_one(&mut reader, &mut cs.rice)?;
                let s_a = cs.lms.step(e);
                let s_b = cs.stage_b.step(s_a);
                scratch[ch] = s_b;
            }
        }

        #[cfg(feature = "trace")]
        if let Some(t) = &mut trace {
            // DECORR_PRE / DECORR_POST are emitted only for nch > 1
            // per spec/06 §5.5; PCM_OUT always.
            if nch > 1 {
                t.ev_decorr_pre(frame_idx, sample_idx as u32, &scratch);
            }
        }

        decorr::inverse(&mut scratch);

        #[cfg(feature = "trace")]
        if let Some(t) = &mut trace {
            if nch > 1 {
                t.ev_decorr_post(frame_idx, sample_idx as u32, &scratch);
            }
            t.ev_pcm_out(frame_idx, sample_idx as u32, &scratch);
        }

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
    let computed_crc = crc.finalize();
    let crc_ok = computed_crc == stored_crc;

    #[cfg(feature = "trace")]
    if let Some(t) = &mut trace {
        t.ev_frame_end(frame_idx, computed_crc, stored_crc, crc_ok);
    }

    if !crc_ok {
        return Err(Error::Crc32Mismatch { region: "frame" });
    }

    let _ = frame_idx; // silence unused warning when `trace` is off
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
    /// Optional Stage-A `qm[0..7]` priming vector. `None` for
    /// format=1 (the default — qm zero-inits per spec/02 §3.1);
    /// `Some(digest)` for format=2 (spec/07 §3.5).
    pub(crate) qm_priming: Option<[i32; 8]>,
    /// IEEE-802.3 CRC32 over the 18 stream-header body bytes,
    /// re-computed at parse time per `spec/01` §3.5. Surfaced to the
    /// `trace` feature's `HEADER_CRC` event per spec/06 §5.1 (and
    /// audit/07 §6.2-3); kept on the struct so the trace emitter
    /// doesn't have to re-parse the header. Without the `trace`
    /// feature the field is unused but still populated, keeping the
    /// parse path uniform.
    #[allow(dead_code)]
    pub(crate) header_crc: u32,
}

impl<'a> Decoder<'a> {
    /// Parse `bytes` as a TTA1 file (with optional ID3v2 prefix) and
    /// return a [`Decoder`] ready to walk the frames.
    ///
    /// Accepts format=1 streams only — format=2 (password-protected,
    /// `spec/07`) streams return [`Error::PasswordRequired`]. Use
    /// [`Decoder::new_with_password`] to construct a streaming decoder
    /// over a format=2 stream.
    pub fn new(bytes: &'a [u8]) -> Result<Self> {
        Self::new_with_priming(bytes, None)
    }

    /// Parse `bytes` as a TTA1 file (with optional ID3v2 prefix) using
    /// `password` to derive the Stage-A `qm[0..7]` priming vector per
    /// `spec/07` §3, and return a [`Decoder`] ready to walk the frames.
    ///
    /// This is the streaming + random-access counterpart to the eager
    /// [`crate::decode_with_password`] entry point. With the returned
    /// [`Decoder`] in hand, callers can drive [`Decoder::frame_iter`],
    /// [`Decoder::decode_frame_at`], [`Decoder::seek_to_sample`], and
    /// [`Decoder::frame_iter_from`] across format=2 streams under the
    /// same bounded-memory / random-access discipline the round-187
    /// surface already provides for format=1.
    ///
    /// Both format=1 and format=2 streams are accepted (this is the
    /// streaming-API analogue of [`crate::decode_with_password`]'s
    /// "accepts format=1 with an unused digest" tolerance):
    ///
    /// - For format=2 streams, the password is hashed with ECMA-182
    ///   CRC-64 per `spec/07` §3.2 and the resulting eight-byte digest
    ///   is applied as the Stage-A `qm[0..7]` priming at every
    ///   per-channel frame init per `spec/07` §3.5–§3.6. An empty
    ///   password produces an all-zero digest per `spec/07` §9 item 2.
    /// - For format=1 streams, the priming is computed but the
    ///   format=1 zero-init invariant of `spec/02` §3.1 is preserved —
    ///   the computed digest is dropped on the constructed [`Decoder`]
    ///   via the same `clear_priming` path that [`crate::decode_with_password`]
    ///   takes (audit/07 §6.2-2).
    ///
    /// Returns the same [`Error`] variants as [`Decoder::new`] (header
    /// CRC, seek-table parse, unsupported format), with the
    /// [`Error::PasswordRequired`] gate lifted because a password is
    /// supplied.
    pub fn new_with_password(bytes: &'a [u8], password: &[u8]) -> Result<Self> {
        let priming = crate::password::derive_qm_priming(password);
        let mut dec = Self::new_with_priming(bytes, Some(priming))?;
        // Format=1 with password supplied: the digest is computed but
        // must not mutate decode state. Drop the priming on the
        // constructed decoder so format=1's spec/02 §3.1 zero-init
        // invariant is preserved without re-parsing the header /
        // seek table (closes audit/07 §6.2-2, same shape as the eager
        // `decode_with_password` path).
        if dec.header.format == 1 {
            dec.clear_priming();
        }
        Ok(dec)
    }

    /// Like [`Self::new`] but accepts a Stage-A LMS `qm` priming
    /// vector for format=2 streams (per spec/07 §3.5). Public callers
    /// should use [`Self::new_with_password`] or
    /// [`crate::decode_with_password`] instead, which compute the
    /// priming from a password automatically.
    pub(crate) fn new_with_priming(bytes: &'a [u8], qm_priming: Option<[i32; 8]>) -> Result<Self> {
        let id3_skip = crate::header::skip_id3v2_prefix(bytes)?;
        let after_id3 = &bytes[id3_skip..];
        let (header, hdr_len, header_crc) = crate::header::parse_stream_header_with_crc(after_id3)?;
        // Format gating fires BEFORE seek-table parse so that a
        // format-validation test on a header-only fixture surfaces
        // the format error (PasswordRequired or UnsupportedFormat)
        // without first tripping a Truncated seek-table read.
        if header.format == 2 && qm_priming.is_none() {
            return Err(Error::PasswordRequired);
        }
        if header.format != 1 && header.format != 2 {
            return Err(Error::UnsupportedFormat(header.format));
        }
        let seek_table_input = &after_id3[hdr_len..];
        let seek_base = (id3_skip + hdr_len) as u64;
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
            qm_priming,
            header_crc,
        })
    }

    /// Drop the Stage-A `qm` priming vector — the underlying decode
    /// state will use the format=1 zero-init at every per-channel
    /// frame entry. Used by [`crate::decode_with_password`] when a
    /// password was supplied but the on-disk `format` field is `1`
    /// (the digest is computed but unused; cf. spec/02 §3.1 and
    /// audit/07 §6.2-2).
    pub(crate) fn clear_priming(&mut self) {
        self.qm_priming = None;
    }

    /// Decode every frame and return interleaved `i32` PCM samples
    /// for the entire stream (`total_samples * channels` entries).
    pub fn decode_all(&self) -> Result<Vec<i32>> {
        #[cfg(feature = "trace")]
        let mut trace = TraceWriter::open_from_env();
        #[cfg(feature = "trace")]
        self.emit_file_level_trace(trace.as_mut());

        let mut out = Vec::with_capacity(
            (self.header.total_samples as usize) * (self.header.channels as usize),
        );
        for (i, frame) in self.frames.iter().enumerate() {
            let off = frame.file_offset as usize;
            let end = off + frame.disk_size as usize;
            if end > self.bytes.len() {
                return Err(Error::Truncated);
            }
            let frame_bytes = &self.bytes[off..end];
            let pcm = decode_frame_inner(
                &self.header,
                frame,
                frame_bytes,
                self.qm_priming.as_ref(),
                i as u32,
                #[cfg(feature = "trace")]
                trace.as_mut(),
            )?;
            out.extend_from_slice(&pcm);
        }
        Ok(out)
    }

    /// Decode a single frame **by its index** in the seek table and
    /// return its interleaved `i32` PCM samples (length
    /// `frame.sample_count * header.channels`).
    ///
    /// Per `spec/01` §5.1 + `spec/02..05` §3.1, every codec stage
    /// (Rice trackers, Stage-A LMS state, Stage-B `prev` register)
    /// **resets at every frame boundary** — frames are therefore
    /// independently decodable from `(header, descriptor, frame_bytes,
    /// qm_priming)`. No carryover state. That property is what makes
    /// random-access decode (this method) and the streaming
    /// [`Decoder::frame_iter`] both legitimate against the spec.
    ///
    /// Returns [`Error::Truncated`] if the seek-table entry points
    /// outside the in-memory slice, [`Error::Crc32Mismatch`] on a
    /// per-frame CRC failure, or any of the bitstream-level errors
    /// raised by the underlying entropy / predictor decoders.
    pub fn decode_frame_at(&self, index: usize) -> Result<Vec<i32>> {
        let frame = self.frames.get(index).ok_or(Error::FrameIndexOutOfRange)?;
        let off = frame.file_offset as usize;
        let end = off
            .checked_add(frame.disk_size as usize)
            .ok_or(Error::Truncated)?;
        if end > self.bytes.len() {
            return Err(Error::Truncated);
        }
        let frame_bytes = &self.bytes[off..end];
        decode_frame_inner(
            &self.header,
            frame,
            frame_bytes,
            self.qm_priming.as_ref(),
            index as u32,
            #[cfg(feature = "trace")]
            None,
        )
    }

    /// Lazy iterator over the stream's frames. Each `next()` decodes
    /// the next frame in order and yields its interleaved PCM
    /// samples — memory usage is bounded by `O(samples_per_frame *
    /// channels)` regardless of total stream length.
    ///
    /// Intended for streaming consumers (e.g. an `oxideav-pipeline`
    /// stage) that want to start producing samples before the whole
    /// file is decoded. The eager `decode_all` path materialises the
    /// full sample buffer; this one does not.
    ///
    /// The iterator is **trace-silent** (it does not emit `spec/06`
    /// trace events). Callers who need a tape should use
    /// [`Decoder::decode_all`] instead — the trace contract was
    /// designed around the eager path and adding per-call trace setup
    /// would defeat the streaming property.
    pub fn frame_iter(&self) -> FrameIter<'_, 'a> {
        FrameIter {
            decoder: self,
            next_idx: 0,
        }
    }

    /// Like [`Decoder::frame_iter`] but starts decoding at frame
    /// `start_index` instead of frame `0`. Used in combination with
    /// [`Decoder::seek_to_sample`] to resume decode at a seek point
    /// without paying for the skipped prefix.
    ///
    /// `start_index >= frames.len()` produces an empty iterator
    /// rather than an error — callers that want to detect that
    /// should bound-check against `dec.frames.len()` first.
    pub fn frame_iter_from(&self, start_index: usize) -> FrameIter<'_, 'a> {
        FrameIter {
            decoder: self,
            next_idx: start_index.min(self.frames.len()),
        }
    }

    /// Per-channel total sample count for the stream.
    pub fn total_samples(&self) -> u32 {
        self.header.total_samples
    }

    /// Locate the frame containing the per-channel `sample_index`
    /// (zero-based) and the sample's offset within that frame.
    ///
    /// All frames except the last contain exactly
    /// `header.regular_frame_samples()` per-channel samples per spec
    /// §4.1; this method walks that arithmetic so callers don't have
    /// to. Returns [`Error::SampleIndexOutOfRange`] if `sample_index
    /// >= header.total_samples`.
    ///
    /// To resume decode from a seek point, decode the returned
    /// `frame_index` (and any subsequent frames) via
    /// [`Decoder::decode_frame_at`] / [`Decoder::frame_iter`] then
    /// skip `sample_offset_in_frame * header.channels` interleaved
    /// PCM entries.
    pub fn seek_to_sample(&self, sample_index: u64) -> Result<SeekPoint> {
        if sample_index >= self.header.total_samples as u64 {
            return Err(Error::SampleIndexOutOfRange);
        }
        let regular = self.header.regular_frame_samples() as u64;
        if regular == 0 {
            return Err(Error::SampleIndexOutOfRange);
        }
        let frame_index = (sample_index / regular) as usize;
        let sample_offset_in_frame = (sample_index % regular) as u32;
        // Defensive: the per-frame math should never index past
        // self.frames given the §4.1 derivation, but a hand-crafted
        // header could disagree with its own total_samples.
        if frame_index >= self.frames.len() {
            return Err(Error::SampleIndexOutOfRange);
        }
        Ok(SeekPoint {
            frame_index,
            sample_offset_in_frame,
        })
    }

    /// Player-API sugar: combine [`Decoder::seek_to_sample`] with
    /// [`Decoder::frame_iter_from`] and the in-frame prefix skip into a
    /// single iterator that yields interleaved `i32` PCM samples
    /// starting **at** `sample_index` (zero-based, per-channel).
    ///
    /// The first frame yielded by the inner iterator is decoded in
    /// full, then its leading `sample_offset_in_frame * channels`
    /// interleaved entries are discarded before the trimmed buffer is
    /// returned. Every subsequent frame is yielded verbatim. The
    /// concatenation of every yielded `Vec<i32>` equals the suffix of
    /// `Decoder::decode_all` starting at `sample_index * channels`.
    ///
    /// The per-frame state-reset discipline of `spec/01` §5.1 +
    /// `spec/02..05` §3.1 is what makes the in-frame skip legitimate:
    /// every frame seeds its Rice / Stage-A / Stage-B trackers from
    /// zero (or the format=2 `qm` priming digest), so the only price
    /// of resuming mid-frame is the per-channel decoded prefix we
    /// throw away — not a tracker-state recovery walk.
    ///
    /// Returns [`Error::SampleIndexOutOfRange`] when `sample_index >=
    /// header.total_samples`. The iterator itself is trace-silent
    /// (same property as [`Decoder::frame_iter`] / `frame_iter_from`).
    pub fn frame_iter_from_sample(&self, sample_index: u64) -> Result<SampleSkipIter<'_, 'a>> {
        let seek = self.seek_to_sample(sample_index)?;
        let inner = self.frame_iter_from(seek.frame_index);
        let skip = (seek.sample_offset_in_frame as usize) * (self.header.channels as usize);
        Ok(SampleSkipIter {
            inner,
            prefix_to_skip: skip,
        })
    }

    /// Player-API sugar: combine [`Decoder::frame_iter_from_sample`]
    /// with eager materialisation. Returns the interleaved `i32` PCM
    /// tail of the stream starting **at** `sample_index` (zero-based,
    /// per-channel); equivalent to
    /// `decode_all()[sample_index * channels..]` but without paying
    /// for the discarded prefix.
    ///
    /// Returns [`Error::SampleIndexOutOfRange`] when `sample_index >=
    /// header.total_samples`. Any bitstream-level error encountered
    /// while decoding the tail short-circuits as
    /// `Err(Error::…)`.
    pub fn decode_from_sample(&self, sample_index: u64) -> Result<Vec<i32>> {
        let iter = self.frame_iter_from_sample(sample_index)?;
        let total = self.header.total_samples as u64;
        let channels = self.header.channels as usize;
        // Suffix length in interleaved entries = (total - sample_index) * channels.
        // The seek_to_sample bound check already guaranteed sample_index < total.
        let suffix_entries = ((total - sample_index) as usize).saturating_mul(channels);
        let mut out = Vec::with_capacity(suffix_entries);
        for frame in iter {
            out.extend_from_slice(&frame?);
        }
        Ok(out)
    }

    /// Emit the §5.1 container-level events plus the §5.2 per-channel
    /// init events to the trace tape, in the order required by §7.1
    /// and §7.2.
    #[cfg(feature = "trace")]
    fn emit_file_level_trace(&self, trace: Option<&mut TraceWriter>) {
        let Some(t) = trace else {
            return;
        };
        t.ev_file_header(
            true,
            self.header.format as u32,
            self.header.channels as u32,
            self.header.bits_per_sample as u32,
            self.header.sample_rate,
            self.header.total_samples,
        );
        // The header CRC was both computed and verified at parse
        // time; surface the real value here per spec/06 §5.1 and
        // audit/07 §6.2-3. The `crc_ok` flag is unconditionally true
        // because a CRC mismatch would have aborted `Decoder::new`
        // before this point.
        t.ev_header_crc(true, self.header_crc);

        let frame_count = self.frames.len() as u32;
        t.ev_seek_table_begin(frame_count);
        for (i, f) in self.frames.iter().enumerate() {
            t.ev_seek_entry(i as u32, f.disk_size, f.file_offset);
        }
        t.ev_seek_table_end(self.seek_table_crc_ok);

        // Per-channel init events fire channel-major, frame_idx = -1.
        let bytes_per_sample = self.header.bytes_per_sample();
        let lms_seed = LmsState::frame_init(bytes_per_sample);
        let rice_seed = RiceState::frame_init();
        for ch in 0..self.header.channels as u32 {
            t.ev_lms_init(-1, ch, lms_seed.shift, lms_seed.round);
            t.ev_rice_k_init(
                -1,
                ch,
                rice_seed.k0,
                rice_seed.k1,
                rice_seed.sum0,
                rice_seed.sum1,
            );
        }
    }
}

/// A located position in the stream produced by
/// [`Decoder::seek_to_sample`].
///
/// Combined with the seek table, this is enough information to start
/// decode at any per-channel sample boundary in the stream without
/// touching prior frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeekPoint {
    /// Zero-based index into [`Decoder::frames`] of the frame
    /// containing the requested sample.
    pub frame_index: usize,
    /// Zero-based per-channel sample offset within that frame at
    /// which the requested sample sits. To consume only samples at
    /// or after the seek point, skip
    /// `sample_offset_in_frame * header.channels` interleaved
    /// entries from the start of the frame's decoded PCM buffer.
    pub sample_offset_in_frame: u32,
}

/// Lazy frame-by-frame decoder iterator returned by
/// [`Decoder::frame_iter`].
///
/// Each call to `next()` decodes exactly one frame and yields its
/// interleaved `i32` PCM samples. Memory used by the iterator itself
/// is `O(1)` (a back-reference to the parent [`Decoder`] plus a
/// `usize` cursor); the per-frame PCM buffer is freshly allocated by
/// the underlying decode step.
///
/// Stops yielding when every frame in the seek table has been
/// consumed. A bitstream-level decode error short-circuits the
/// iterator: the error variant is returned once and any subsequent
/// `next()` call returns `None`.
pub struct FrameIter<'d, 'a> {
    decoder: &'d Decoder<'a>,
    next_idx: usize,
}

impl<'d, 'a> Iterator for FrameIter<'d, 'a> {
    type Item = Result<Vec<i32>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_idx >= self.decoder.frames.len() {
            return None;
        }
        let idx = self.next_idx;
        self.next_idx += 1;
        let res = self.decoder.decode_frame_at(idx);
        if res.is_err() {
            // Short-circuit further iteration; the caller already
            // owns the error and re-polling would just truncate
            // again or produce stale state on a recoverable case.
            self.next_idx = self.decoder.frames.len();
        }
        Some(res)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.decoder.frames.len().saturating_sub(self.next_idx);
        (remaining, Some(remaining))
    }
}

impl<'d, 'a> ExactSizeIterator for FrameIter<'d, 'a> {}

/// Lazy frame-by-frame decoder iterator returned by
/// [`Decoder::frame_iter_from_sample`].
///
/// Wraps an inner [`FrameIter`] (positioned at the frame containing the
/// requested per-channel `sample_index`) and trims the leading
/// `sample_offset_in_frame * channels` interleaved entries off the
/// **first** yielded frame so the iterator's output begins exactly at
/// the requested sample boundary. Every subsequent frame is forwarded
/// verbatim.
///
/// The trim runs once, at first `next()`. If the first decoded frame
/// produces fewer interleaved entries than the requested prefix (an
/// impossible case under [`Decoder::seek_to_sample`]'s `< regular`
/// invariant, but defensive against hand-crafted seek tables), the
/// trim saturates and the yielded buffer is empty for that frame; the
/// next frame onward is forwarded normally.
///
/// Like the underlying [`FrameIter`], this iterator is trace-silent
/// and stops yielding once every frame in the seek table has been
/// consumed.
pub struct SampleSkipIter<'d, 'a> {
    inner: FrameIter<'d, 'a>,
    /// Number of leading interleaved `i32` entries to discard from the
    /// next decoded frame's PCM buffer. Cleared after the first
    /// non-empty trim.
    prefix_to_skip: usize,
}

impl<'d, 'a> Iterator for SampleSkipIter<'d, 'a> {
    type Item = Result<Vec<i32>>;

    fn next(&mut self) -> Option<Self::Item> {
        let nxt = self.inner.next()?;
        if self.prefix_to_skip == 0 {
            return Some(nxt);
        }
        match nxt {
            Ok(mut pcm) => {
                let skip = self.prefix_to_skip.min(pcm.len());
                self.prefix_to_skip = 0;
                if skip == 0 {
                    Some(Ok(pcm))
                } else {
                    // Drain the leading `skip` interleaved entries.
                    // `drain(..skip)` is O(skip + tail copy); for the
                    // expected sub-frame skip sizes (max ~46 073 ×
                    // channels) the tail copy is cheap relative to the
                    // already-paid decode cost.
                    pcm.drain(..skip);
                    Some(Ok(pcm))
                }
            }
            Err(e) => {
                // Surface the error and let inner short-circuit.
                self.prefix_to_skip = 0;
                Some(Err(e))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'d, 'a> ExactSizeIterator for SampleSkipIter<'d, 'a> {}
