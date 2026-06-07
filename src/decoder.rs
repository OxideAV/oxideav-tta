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

    /// Total per-channel playback duration of the stream.
    ///
    /// Computed from the header's `total_samples` and `sample_rate`
    /// fields per `spec/01` §3.3 / §3.4. The whole-seconds and
    /// sub-second components are derived in integer arithmetic from
    /// the wire-side `(total_samples, sample_rate)` pair so the result
    /// is exact at nanosecond granularity (no floating-point
    /// intermediates) for any in-scope stream (`sample_rate` capped at
    /// `0x7FFFFF` per `spec/01` §3.3, `total_samples` at most
    /// `u32::MAX`).
    ///
    /// Special case: `sample_rate == 0` is not validly accepted by
    /// [`Decoder::new`], but if a hand-constructed [`Decoder`] ever
    /// reached this method with `sample_rate == 0`, `Duration::ZERO`
    /// is returned (no division). The same convention applies to
    /// `total_samples == 0`.
    pub fn total_duration(&self) -> core::time::Duration {
        samples_to_duration(self.header.total_samples as u64, self.header.sample_rate)
    }

    /// Locate the per-channel sample at clock time `time` from the
    /// start of the stream, returning the same [`SeekPoint`] that
    /// [`Decoder::seek_to_sample`] would return for the corresponding
    /// sample index.
    ///
    /// The sample index is computed by integer arithmetic from
    /// `(time_ns, sample_rate)`: `sample_index = floor(time_ns *
    /// sample_rate / 1_000_000_000)`. The intermediate is widened to
    /// `u128` so the multiplication does not overflow even for the
    /// upper-end `sample_rate = 0x7FFFFF` × `Duration::MAX` envelope.
    ///
    /// The conversion is monotonically non-decreasing in `time`; two
    /// timestamps within the same sample period (`1 / sample_rate`
    /// seconds) round to the same sample index, which then resolves
    /// to the same `(frame_index, sample_offset_in_frame)` pair under
    /// `spec/01` §4.1.
    ///
    /// Returns [`Error::SampleIndexOutOfRange`] when `time` lies at or
    /// past [`Decoder::total_duration`]; `time = Duration::ZERO` always
    /// resolves to the first sample of the stream (assuming
    /// `total_samples > 0`).
    pub fn seek_to_time(&self, time: core::time::Duration) -> Result<SeekPoint> {
        let sample_index = duration_to_sample_index(time, self.header.sample_rate);
        self.seek_to_sample(sample_index)
    }

    /// Player-API sugar: convert `time` to a per-channel sample
    /// boundary via [`Decoder::seek_to_time`], then forward to
    /// [`Decoder::frame_iter_from_sample`].
    ///
    /// The returned iterator yields interleaved `i32` PCM samples
    /// starting at the boundary; the concatenation of every yielded
    /// `Vec<i32>` equals the suffix of [`Decoder::decode_all`] from
    /// the corresponding sample cursor.
    ///
    /// Returns [`Error::SampleIndexOutOfRange`] when `time` lies at or
    /// past [`Decoder::total_duration`].
    pub fn frame_iter_from_time(
        &self,
        time: core::time::Duration,
    ) -> Result<SampleSkipIter<'_, 'a>> {
        let sample_index = duration_to_sample_index(time, self.header.sample_rate);
        self.frame_iter_from_sample(sample_index)
    }

    /// Player-API sugar: eager analogue of
    /// [`Decoder::frame_iter_from_time`]. Returns the interleaved
    /// `i32` PCM tail of the stream starting at clock time `time`,
    /// equivalent to
    /// `decode_all()[sample_index_for(time) * channels..]` but without
    /// paying for the discarded prefix.
    ///
    /// Returns [`Error::SampleIndexOutOfRange`] when `time` lies at or
    /// past [`Decoder::total_duration`].
    pub fn decode_from_time(&self, time: core::time::Duration) -> Result<Vec<i32>> {
        let sample_index = duration_to_sample_index(time, self.header.sample_rate);
        self.decode_from_sample(sample_index)
    }

    /// Player-API sugar: eager half-open `[start, end)` sample range
    /// decode.
    ///
    /// Returns the interleaved `i32` PCM for per-channel samples
    /// `start..end`, where `end` may be one past the last sample
    /// (`end == total_samples`). The yielded buffer has
    /// `(end - start) * channels` entries; in particular, an
    /// `start == end` request returns an empty `Vec` without touching
    /// the bitstream. Equivalent to
    /// `decode_all()[start * channels .. end * channels]` but without
    /// paying for samples outside the requested range — the leading
    /// prefix is skipped via [`Decoder::seek_to_sample`]'s `spec/01`
    /// §4.1 arithmetic, and the trailing suffix is never decoded
    /// (whole frames past `end` are dropped from the inner walk).
    ///
    /// # Errors
    ///
    /// - [`Error::SampleIndexOutOfRange`] when `start > end` (the range
    ///   is invalid).
    /// - [`Error::SampleIndexOutOfRange`] when `end > total_samples`
    ///   (the range overshoots the stream). Note that `end ==
    ///   total_samples` is **valid** — it means "to the very end" — so
    ///   the half-open convention lines up with Rust's `Range` and
    ///   `Vec` slicing.
    ///
    /// `start == total_samples` with `end == total_samples` returns
    /// `Ok(vec![])` (empty range at the boundary). `start ==
    /// total_samples` with `end > total_samples` errors per the
    /// out-of-range rule.
    pub fn decode_sample_range(&self, start: u64, end: u64) -> Result<Vec<i32>> {
        let iter = self.frame_iter_sample_range(start, end)?;
        let channels = self.header.channels as usize;
        let suffix_entries = ((end - start) as usize).saturating_mul(channels);
        let mut out = Vec::with_capacity(suffix_entries);
        for frame in iter {
            out.extend_from_slice(&frame?);
        }
        Ok(out)
    }

    /// Player-API sugar: lazy analogue of [`Decoder::decode_sample_range`].
    ///
    /// Returns a [`SampleRangeIter`] yielding interleaved `i32` PCM for
    /// per-channel samples `start..end` (half-open). The concatenation
    /// of every yielded `Vec<i32>` equals
    /// `decode_sample_range(start, end)`'s `Vec`.
    ///
    /// The iterator decodes only the frames that overlap `[start, end)`:
    /// the leading frame is trimmed at its head (per
    /// [`Decoder::seek_to_sample`]'s in-frame offset), every interior
    /// frame is yielded verbatim, and the trailing frame is trimmed at
    /// its tail so the final concatenated count is exactly
    /// `(end - start) * channels`. Frames past `end` are not decoded
    /// at all.
    ///
    /// `start == end` returns an iterator that yields no frames; the
    /// caller observes `Ok(())` from the wrapping call and an empty
    /// iterator. `start == total_samples` with `end == total_samples`
    /// is the same boundary case.
    ///
    /// Like [`Decoder::frame_iter_from_sample`], the iterator is
    /// trace-silent (it does not emit `spec/06` trace events).
    ///
    /// # Errors
    ///
    /// Same as [`Decoder::decode_sample_range`].
    pub fn frame_iter_sample_range(&self, start: u64, end: u64) -> Result<SampleRangeIter<'_, 'a>> {
        if start > end {
            return Err(Error::SampleIndexOutOfRange);
        }
        let total = self.header.total_samples as u64;
        if end > total {
            return Err(Error::SampleIndexOutOfRange);
        }
        let channels = self.header.channels as usize;
        // Empty range short-circuit: no inner walk, no seek_to_sample
        // call (the seek would reject `start == total_samples`).
        if start == end {
            return Ok(SampleRangeIter {
                inner: SampleSkipIter {
                    inner: FrameIter {
                        decoder: self,
                        next_idx: self.frames.len(),
                    },
                    prefix_to_skip: 0,
                },
                remaining_entries: 0,
            });
        }
        // `start < end <= total` and `start < total` — the
        // `seek_to_sample(start)` call is valid (`start < total_samples`).
        let inner = self.frame_iter_from_sample(start)?;
        let remaining_entries = ((end - start) as usize).saturating_mul(channels);
        Ok(SampleRangeIter {
            inner,
            remaining_entries,
        })
    }

    /// Player-API sugar: duration-keyed eager analogue of
    /// [`Decoder::decode_sample_range`].
    ///
    /// Both endpoints are converted to per-channel sample indices via
    /// the same `floor(time_ns * sample_rate / 1e9)` arithmetic used by
    /// [`Decoder::seek_to_time`], then forwarded to
    /// [`Decoder::decode_sample_range`].
    ///
    /// Returns the interleaved `i32` PCM for the half-open clock range
    /// `[start, end)` of the stream's playback timeline; the yielded
    /// buffer has `(sample_for(end) - sample_for(start)) * channels`
    /// entries.
    ///
    /// `end == total_duration()` is valid and equivalent to "to the
    /// end of the stream"; `end > total_duration()` errors per the
    /// out-of-range rule on the underlying sample index. `start > end`
    /// errors. `start == end` returns `Ok(vec![])`.
    ///
    /// # Errors
    ///
    /// Same as [`Decoder::decode_sample_range`].
    pub fn decode_time_range(
        &self,
        start: core::time::Duration,
        end: core::time::Duration,
    ) -> Result<Vec<i32>> {
        let rate = self.header.sample_rate;
        let start_sample = duration_to_sample_index(start, rate);
        let end_sample = duration_to_sample_index(end, rate);
        self.decode_sample_range(start_sample, end_sample)
    }

    /// Player-API sugar: duration-keyed lazy analogue of
    /// [`Decoder::frame_iter_sample_range`].
    ///
    /// Both endpoints are converted to per-channel sample indices via
    /// `floor(time_ns * sample_rate / 1e9)`, then forwarded to
    /// [`Decoder::frame_iter_sample_range`].
    ///
    /// The concatenation of every yielded `Vec<i32>` equals
    /// `decode_time_range(start, end)`'s `Vec`.
    ///
    /// # Errors
    ///
    /// Same as [`Decoder::frame_iter_sample_range`].
    pub fn frame_iter_time_range(
        &self,
        start: core::time::Duration,
        end: core::time::Duration,
    ) -> Result<SampleRangeIter<'_, 'a>> {
        let rate = self.header.sample_rate;
        let start_sample = duration_to_sample_index(start, rate);
        let end_sample = duration_to_sample_index(end, rate);
        self.frame_iter_sample_range(start_sample, end_sample)
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

/// Typed wrapper around a [`SeekPoint`]'s `frame_index` field — a
/// zero-based index into the stream's seek table per `spec/01` §4.1 /
/// §4.2.
///
/// Validated against the stream's `frame_count`: every
/// parser-produced seek point is bounded by
/// [`Decoder::seek_to_sample`]'s `frame_index < self.frames.len()`
/// defensive gate, and the typed accessor surfaces that invariant at
/// lift time so a caller that constructs a [`SeekPoint`] literal
/// (e.g. an ad-hoc fixture) gets the same
/// [`Error::InvalidFrameIndex`] discipline the random-access path
/// produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameIndex(usize);

impl FrameIndex {
    /// Lift a raw `usize` into the typed accessor. Returns
    /// [`Error::InvalidFrameIndex`] when `value >= frame_count` per
    /// `spec/01` §4.1 (the closed-form `ceil(total_samples /
    /// regular_frame_samples)` total). Every value in
    /// `0..frame_count` is structurally legal: the empty-stream case
    /// (`frame_count == 0` per spec §4.4) accepts no value, and the
    /// non-empty case accepts exactly the seek-table window.
    pub fn from_raw(value: usize, frame_count: usize) -> Result<Self> {
        if value >= frame_count {
            Err(Error::InvalidFrameIndex(value))
        } else {
            Ok(FrameIndex(value))
        }
    }

    /// Zero-based index into the stream's seek table (`spec/01` §4.2).
    pub fn index(&self) -> usize {
        self.0
    }

    /// `true` when this index addresses the final frame of a stream
    /// of `frame_count` frames (i.e. `index() + 1 == frame_count`).
    /// The last frame is the only one allowed to carry fewer than
    /// `regular_frame_samples` per-channel samples per `spec/01` §4.1.
    pub fn is_last(&self, frame_count: usize) -> bool {
        self.0 + 1 == frame_count
    }
}

/// Typed wrapper around a [`SeekPoint`]'s `sample_offset_in_frame`
/// field — the zero-based per-channel sample offset within the
/// addressed frame per `spec/01` §4.1.
///
/// Validated against the stream's `regular_frame_samples`: every
/// parser-produced offset is `sample_index % regular_frame_samples`
/// per [`Decoder::seek_to_sample`], which is strictly less than the
/// regular per-frame count by construction. The typed accessor
/// surfaces that invariant at lift time so a caller that constructs a
/// [`SeekPoint`] literal gets the same
/// [`Error::InvalidInFrameSampleOffset`] discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InFrameSampleOffset(u32);

impl InFrameSampleOffset {
    /// Lift a raw `u32` into the typed accessor. Returns
    /// [`Error::InvalidInFrameSampleOffset`] when `value >=
    /// regular_frame_samples` per `spec/01` §4.1. The strict-less
    /// bound is what makes `(frame_index, sample_offset_in_frame)` a
    /// unique addressing pair: when the modulo reaches the regular
    /// count it rolls over to the next frame's zero offset.
    ///
    /// A `regular_frame_samples` argument of `0` (only reachable for
    /// the empty-stream case per `spec/01` §3.4) rejects every value
    /// — there is no in-frame addressing window in an empty stream.
    pub fn from_raw(value: u32, regular_frame_samples: u32) -> Result<Self> {
        if value >= regular_frame_samples {
            Err(Error::InvalidInFrameSampleOffset(value))
        } else {
            Ok(InFrameSampleOffset(value))
        }
    }

    /// Zero-based per-channel sample offset within the addressed
    /// frame (`spec/01` §4.1).
    pub fn offset(&self) -> u32 {
        self.0
    }

    /// `true` when the offset is exactly at the frame boundary
    /// (`offset() == 0`) — the seek point addresses the first sample
    /// of the frame, so the [`Decoder::frame_iter_from_sample`] prefix
    /// skip is a no-op.
    pub fn is_frame_boundary(&self) -> bool {
        self.0 == 0
    }

    /// Number of interleaved `i32` PCM entries to discard from the
    /// decoded `frame_index` frame to land exactly at the seek point,
    /// given the stream's `channels` count (= `offset() * channels`
    /// per `spec/01` §4.1 / §3.2 — the per-channel offset times the
    /// channel-interleave stride). Saturates at `usize::MAX` on the
    /// upper-end `(u32::MAX, u16::MAX)` envelope; the practical
    /// `(46_080, 6)` ceiling for in-scope streams fits comfortably in
    /// `usize` on every supported target.
    pub fn interleaved_skip(&self, channels: u16) -> usize {
        (self.0 as usize).saturating_mul(channels as usize)
    }
}

impl SeekPoint {
    /// Lifts the raw `frame_index` field into the typed
    /// [`FrameIndex`] accessor per `spec/01` §4.1 / §4.2 (validates
    /// `< frame_count`).
    ///
    /// A successfully-produced [`SeekPoint`] from
    /// [`Decoder::seek_to_sample`] / [`Decoder::seek_to_time`] is
    /// guaranteed to satisfy the bound because the defensive
    /// `frame_index >= self.frames.len()` gate rejects out-of-range
    /// indices at construction; the accessor returns a `Result`
    /// rather than an infallible projection so an ad-hoc
    /// [`SeekPoint`] literal constructed by a caller (e.g. a test
    /// fixture) gets the same [`Error::InvalidFrameIndex`]
    /// discipline.
    pub fn frame_index_typed(&self, frame_count: usize) -> Result<FrameIndex> {
        FrameIndex::from_raw(self.frame_index, frame_count)
    }

    /// Lifts the raw `sample_offset_in_frame` field into the typed
    /// [`InFrameSampleOffset`] accessor per `spec/01` §4.1 (validates
    /// `< regular_frame_samples`).
    ///
    /// A successfully-produced [`SeekPoint`] is guaranteed to satisfy
    /// the bound because [`Decoder::seek_to_sample`] computes the
    /// offset as `sample_index % regular_frame_samples`. Same
    /// `Result` discipline as [`Self::frame_index_typed`] for the
    /// ad-hoc-literal path.
    pub fn sample_offset_typed(&self, regular_frame_samples: u32) -> Result<InFrameSampleOffset> {
        InFrameSampleOffset::from_raw(self.sample_offset_in_frame, regular_frame_samples)
    }
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

/// Half-open `[start, end)` sample-range decoder iterator returned by
/// [`Decoder::frame_iter_sample_range`] and
/// [`Decoder::frame_iter_time_range`].
///
/// Yields the same content as a [`SampleSkipIter`] starting at `start`
/// but stops once `(end - start) * channels` interleaved entries have
/// been produced. The trailing frame is trimmed in-place so the final
/// concatenated count is exact.
///
/// The iterator is **trace-silent**: the underlying [`FrameIter`] does
/// not emit `spec/06` trace events, and the head- / tail-trim wrapper
/// adds no events of its own.
pub struct SampleRangeIter<'d, 'a> {
    inner: SampleSkipIter<'d, 'a>,
    /// Remaining interleaved entries to yield across all subsequent
    /// `next()` calls. The inner walk stops yielding once this reaches
    /// zero, regardless of how many frames remain in the seek table.
    remaining_entries: usize,
}

impl<'d, 'a> Iterator for SampleRangeIter<'d, 'a> {
    type Item = Result<Vec<i32>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining_entries == 0 {
            return None;
        }
        match self.inner.next()? {
            Ok(mut pcm) => {
                if pcm.len() > self.remaining_entries {
                    // Trailing frame: trim the suffix so the final
                    // concatenation hits the requested per-sample
                    // boundary exactly. The drop is in-place
                    // (`Vec::truncate`) and is O(1) on the truncated
                    // tail beyond `remaining_entries`.
                    pcm.truncate(self.remaining_entries);
                }
                self.remaining_entries -= pcm.len();
                Some(Ok(pcm))
            }
            Err(e) => {
                // Surface the error and let the inner iterator
                // short-circuit on subsequent calls.
                self.remaining_entries = 0;
                Some(Err(e))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Upper bound is the inner walk's upper bound; the lower bound
        // is 0 because a bitstream error could short-circuit before
        // any further frame yields.
        let (_, upper) = self.inner.size_hint();
        (0, upper)
    }
}

/// Convert a clock `Duration` to the corresponding per-channel sample
/// index at `sample_rate` Hz, using integer arithmetic so the result is
/// exact and overflow-free for the entire in-scope envelope.
///
/// The formula is `floor(time_ns * sample_rate / 1_000_000_000)`,
/// promoted to `u128` for the multiplication so the largest in-scope
/// combination (`sample_rate = 0x7FFFFF`, `Duration::MAX` ≈
/// `18 446 744 073 709 551 615` seconds) cannot overflow.
///
/// The result is clamped to `u64::MAX` for the (otherwise unreachable)
/// case where it exceeds the addressing range of [`Decoder::seek_to_sample`]'s
/// `u64` argument; the subsequent `seek_to_sample` call will then
/// surface [`Error::SampleIndexOutOfRange`] in the usual way.
///
/// A `sample_rate == 0` decoder (rejected by [`Decoder::new`] but
/// reachable in unit tests) returns `0` — no division by zero —
/// which is the correct sentinel given there is no playable stream.
fn duration_to_sample_index(time: core::time::Duration, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    let ns = time.as_nanos();
    let prod = ns.saturating_mul(sample_rate as u128);
    let idx = prod / 1_000_000_000u128;
    if idx > u64::MAX as u128 {
        u64::MAX
    } else {
        idx as u64
    }
}

/// Convert a per-channel sample count to a `Duration` at `sample_rate`
/// Hz with nanosecond granularity. Inverse of
/// [`duration_to_sample_index`] under the floor-division boundary
/// convention (`samples_to_duration(N+1, rate) − samples_to_duration(N,
/// rate)` is exactly one sample period rounded to nanoseconds).
///
/// `sample_rate == 0` returns [`core::time::Duration::ZERO`].
fn samples_to_duration(samples: u64, sample_rate: u32) -> core::time::Duration {
    if sample_rate == 0 {
        return core::time::Duration::ZERO;
    }
    let secs = samples / (sample_rate as u64);
    let remainder = samples % (sample_rate as u64);
    // Sub-second component in nanoseconds: floor(remainder * 1e9 /
    // sample_rate). Widened to u128 so the multiplication cannot
    // overflow (remainder < sample_rate ≤ 0x7FFFFF, so the product
    // fits easily — the widening is a defensive cost-free
    // simplification).
    let nanos = ((remainder as u128) * 1_000_000_000u128) / (sample_rate as u128);
    core::time::Duration::new(secs, nanos as u32)
}

#[cfg(test)]
mod duration_helpers_tests {
    use super::{duration_to_sample_index, samples_to_duration};
    use core::time::Duration;

    #[test]
    fn duration_zero_maps_to_sample_zero() {
        assert_eq!(duration_to_sample_index(Duration::ZERO, 44_100), 0);
    }

    #[test]
    fn one_second_at_44100_hz_is_44100_samples() {
        assert_eq!(
            duration_to_sample_index(Duration::from_secs(1), 44_100),
            44_100
        );
    }

    #[test]
    fn duration_truncates_to_nearest_sample_boundary_below() {
        // 0.5 sample-periods at 44.1 kHz → floor to sample 0.
        let half_period = Duration::from_nanos(1_000_000_000 / 44_100 / 2);
        assert_eq!(duration_to_sample_index(half_period, 44_100), 0);

        // 1.5 sample-periods → floor to sample 1.
        let one_and_half =
            Duration::from_nanos((1_000_000_000u64 / 44_100) + (1_000_000_000u64 / 44_100 / 2));
        assert_eq!(duration_to_sample_index(one_and_half, 44_100), 1);
    }

    #[test]
    fn duration_is_monotone_nondecreasing() {
        let mut prev = 0u64;
        for ms in 0..200u64 {
            let cur = duration_to_sample_index(Duration::from_millis(ms), 48_000);
            assert!(cur >= prev, "monotonicity at {ms} ms: {cur} < {prev}");
            prev = cur;
        }
    }

    #[test]
    fn sample_rate_zero_yields_zero() {
        assert_eq!(duration_to_sample_index(Duration::from_secs(10), 0), 0);
    }

    #[test]
    fn samples_to_duration_round_trip_within_one_sample_period() {
        // `samples_to_duration` and `duration_to_sample_index` are
        // both floor-rounded against the sample-period grid: at
        // rates where `1_000_000_000 / rate` is not an integer, the
        // forward conversion drops a sub-nanosecond residue, and the
        // reverse floor can then land one sample short. The round
        // trip therefore converges to `{n - 1, n}` within one sample
        // period — exactly the property a player API needs (a
        // `Duration` cursor never overshoots the true sample boundary).
        for rate in [44_100u32, 48_000, 96_000, 0x7FFFFF] {
            for n in [0u64, 1, 2, rate as u64, (rate as u64) * 5 + 17] {
                let d = samples_to_duration(n, rate);
                let back = duration_to_sample_index(d, rate);
                assert!(
                    back == n || back + 1 == n,
                    "round-trip out-of-range at rate={rate} n={n}: dur={d:?} back={back}"
                );
                // Nudging the duration up by one nanosecond when
                // `back == n - 1` must close the gap (or stay at `n`
                // if it was already there). This is the
                // "never-overshoots" half of the player-API contract.
                let plus_one_ns = d + core::time::Duration::from_nanos(1);
                let back_plus = duration_to_sample_index(plus_one_ns, rate);
                assert!(
                    back_plus >= back,
                    "+1ns must be monotone at rate={rate} n={n}"
                );
            }
        }
    }

    #[test]
    fn samples_to_duration_unit_period_matches_nanos() {
        // At 44.1 kHz one sample period is 1_000_000_000 / 44_100 ns
        // (floor). Verify the helper agrees.
        let one_sample = samples_to_duration(1, 44_100);
        let expected_ns = 1_000_000_000u128 / 44_100u128;
        assert_eq!(one_sample.as_nanos(), expected_ns);
    }

    #[test]
    fn samples_to_duration_sample_rate_zero_is_zero() {
        assert_eq!(samples_to_duration(123, 0), Duration::ZERO);
    }
}

#[cfg(test)]
mod seek_point_typed_tests {
    use super::{FrameIndex, InFrameSampleOffset, SeekPoint};
    use crate::error::Error;

    #[test]
    fn frame_index_typed_boundary() {
        // Empty stream (frame_count == 0): every value rejects.
        assert_eq!(FrameIndex::from_raw(0, 0), Err(Error::InvalidFrameIndex(0)));
        assert_eq!(
            FrameIndex::from_raw(usize::MAX, 0),
            Err(Error::InvalidFrameIndex(usize::MAX))
        );

        // Single-frame stream: index 0 accepts; 1 rejects.
        let fi = FrameIndex::from_raw(0, 1).unwrap();
        assert_eq!(fi.index(), 0);
        assert!(fi.is_last(1));
        assert_eq!(FrameIndex::from_raw(1, 1), Err(Error::InvalidFrameIndex(1)));

        // Three-frame stream: indices 0/1/2 accept; 3 rejects.
        for i in 0..3 {
            let fi = FrameIndex::from_raw(i, 3).unwrap();
            assert_eq!(fi.index(), i);
            assert_eq!(fi.is_last(3), i == 2);
        }
        assert_eq!(FrameIndex::from_raw(3, 3), Err(Error::InvalidFrameIndex(3)));

        // Upper-end usize: always rejects against any finite frame_count.
        assert_eq!(
            FrameIndex::from_raw(usize::MAX, 1_000_000),
            Err(Error::InvalidFrameIndex(usize::MAX))
        );
    }

    #[test]
    fn in_frame_sample_offset_typed_boundary() {
        // regular = 0 (only the empty-stream case): every value rejects.
        assert_eq!(
            InFrameSampleOffset::from_raw(0, 0),
            Err(Error::InvalidInFrameSampleOffset(0))
        );

        // regular = 46_080 (derived for 44.1 kHz): 0..46_080 accept,
        // 46_080 + rejects.
        let zero = InFrameSampleOffset::from_raw(0, 46_080).unwrap();
        assert_eq!(zero.offset(), 0);
        assert!(zero.is_frame_boundary());
        let mid = InFrameSampleOffset::from_raw(23_039, 46_080).unwrap();
        assert_eq!(mid.offset(), 23_039);
        assert!(!mid.is_frame_boundary());
        let last_in = InFrameSampleOffset::from_raw(46_079, 46_080).unwrap();
        assert_eq!(last_in.offset(), 46_079);
        assert!(!last_in.is_frame_boundary());
        assert_eq!(
            InFrameSampleOffset::from_raw(46_080, 46_080),
            Err(Error::InvalidInFrameSampleOffset(46_080))
        );
        assert_eq!(
            InFrameSampleOffset::from_raw(u32::MAX, 46_080),
            Err(Error::InvalidInFrameSampleOffset(u32::MAX))
        );
    }

    #[test]
    fn in_frame_sample_offset_interleaved_skip() {
        let off = InFrameSampleOffset::from_raw(100, 46_080).unwrap();
        // mono
        assert_eq!(off.interleaved_skip(1), 100);
        // stereo
        assert_eq!(off.interleaved_skip(2), 200);
        // 6 channels (max in scope)
        assert_eq!(off.interleaved_skip(6), 600);
        // zero channels (defensive — would mean a malformed header,
        // but the helper must not panic): saturating_mul handles it.
        assert_eq!(off.interleaved_skip(0), 0);

        let zero = InFrameSampleOffset::from_raw(0, 46_080).unwrap();
        assert_eq!(zero.interleaved_skip(6), 0);

        // Upper-end u32 offset times u16::MAX must not overflow on
        // 64-bit usize; on 32-bit usize the saturating multiply
        // clamps to usize::MAX rather than panicking.
        let near_max = InFrameSampleOffset::from_raw(46_079, 46_080).unwrap();
        let skip = near_max.interleaved_skip(6);
        assert_eq!(skip, 46_079 * 6);
    }

    #[test]
    fn seek_point_typed_accessors_match_raw() {
        // Hand-crafted seek point against a 3-frame, 44.1 kHz stream
        // (regular_frame_samples == 46_080 per spec §4.1).
        let sp = SeekPoint {
            frame_index: 1,
            sample_offset_in_frame: 12_345,
        };
        let fi = sp.frame_index_typed(3).unwrap();
        assert_eq!(fi.index(), 1);
        assert!(!fi.is_last(3));
        let off = sp.sample_offset_typed(46_080).unwrap();
        assert_eq!(off.offset(), 12_345);
        assert!(!off.is_frame_boundary());

        // Out-of-window frame_index: rejects.
        let bad_fi = SeekPoint {
            frame_index: 5,
            sample_offset_in_frame: 0,
        };
        assert_eq!(
            bad_fi.frame_index_typed(3),
            Err(Error::InvalidFrameIndex(5))
        );

        // Out-of-window sample_offset_in_frame: rejects at the boundary.
        let bad_off = SeekPoint {
            frame_index: 0,
            sample_offset_in_frame: 46_080,
        };
        assert_eq!(
            bad_off.sample_offset_typed(46_080),
            Err(Error::InvalidInFrameSampleOffset(46_080))
        );
    }

    #[test]
    fn seek_point_frame_boundary_zero_offset() {
        // A seek to a frame boundary lands at offset 0; the typed
        // accessor records that condition explicitly for player UIs
        // that want to flag "no prefix skip needed" without computing
        // the skip themselves.
        let sp = SeekPoint {
            frame_index: 2,
            sample_offset_in_frame: 0,
        };
        let off = sp.sample_offset_typed(46_080).unwrap();
        assert!(off.is_frame_boundary());
        assert_eq!(off.interleaved_skip(2), 0);
    }
}
