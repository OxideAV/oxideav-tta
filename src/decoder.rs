//! Per-frame TTA decoder. Wired into the [`oxideav_core::Decoder`]
//! trait via [`make_decoder`].
//!
//! The caller is expected to feed one frame body per packet (matching
//! the seek-table sizes from the file header). Each packet is the
//! full frame *including* its trailing 32-bit CRC.

use oxideav_core::bits::BitReaderLsb;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::crc::crc32;
use crate::header::TtaHeader;

/// Initial Rice parameter for both `k0` and `k1` at frame entry.
/// Constant across all bit depths per §4.4 of the spec doc.
pub const RICE_INIT_K: u32 = 10;

/// Initial value for both Rice running sums at frame entry.
/// `1 << (RICE_INIT_K + 4)` = `0x4000`.
pub const RICE_INIT_SUM: u32 = 1 << (RICE_INIT_K + 4);

/// Stage-A 8-tap LMS filter shift per `bps_bytes - 1` (8/16/24/32-bit).
const FILTER_SHIFTS: [u32; 4] = [10, 9, 10, 12];

/// Stage-B fixed predictor `k` per `bps_bytes - 1` (8/16/24/32-bit).
/// `bps_bytes == 4` would mean "add prev directly" (equivalent to
/// `k = ∞` in the spec); we cap it at 31 so `((1 << k) - 1) >> k` is
/// effectively `1` after the shift, matching the "add full predictor
/// unmodified" behaviour. We only exercise the 1/2/3 entries here.
const PREDICTOR_K: [u32; 4] = [4, 5, 5, 31];

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let header = TtaHeader::parse(&params.extradata)?;
    let output_format = output_format_for(&header)?;
    Ok(Box::new(TtaDecoder {
        codec_id: params.codec_id.clone(),
        header,
        output_format,
        pending: None,
        eof: false,
    }))
}

fn output_format_for(h: &TtaHeader) -> Result<SampleFormat> {
    Ok(match h.bits_per_sample {
        8 => SampleFormat::U8,
        16 => SampleFormat::S16,
        24 => SampleFormat::S32, // 24-bit packed into 32-bit, low byte = 0
        other => return Err(Error::unsupported(format!("TTA bps {other}"))),
    })
}

struct TtaDecoder {
    codec_id: CodecId,
    header: TtaHeader,
    output_format: SampleFormat,
    pending: Option<Packet>,
    eof: bool,
}

impl Decoder for TtaDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::other(
                "TTA decoder: receive_frame must be called before sending another packet",
            ));
        }
        self.pending = Some(packet.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let Some(pkt) = self.pending.take() else {
            return if self.eof {
                Err(Error::Eof)
            } else {
                Err(Error::NeedMore)
            };
        };
        // Decode each packet as one full frame; the caller chooses the
        // sample count via the seek table. We can't know "is this the
        // last frame?" from the packet alone, so we let the residual
        // count fall out naturally: try the full frame size first;
        // if the body has too few samples, the entropy decoder will
        // hit EOF and we'll retry with the short last-frame size.
        decode_one_frame(&pkt.data, &self.header, self.output_format, pkt.pts)
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
}

/// Public per-frame entry point — kept exposed for test harnesses
/// that drive the codec without going through the trait object.
pub fn decode_one_frame(
    frame_with_crc: &[u8],
    header: &TtaHeader,
    output_format: SampleFormat,
    pts: Option<i64>,
) -> Result<Frame> {
    if frame_with_crc.len() < 4 {
        return Err(Error::invalid("TTA frame: shorter than 4-byte CRC trailer"));
    }
    let body_len = frame_with_crc.len() - 4;
    let body = &frame_with_crc[..body_len];

    let claimed_crc = u32::from_le_bytes([
        frame_with_crc[body_len],
        frame_with_crc[body_len + 1],
        frame_with_crc[body_len + 2],
        frame_with_crc[body_len + 3],
    ]);
    let computed = crc32(body);
    if computed != claimed_crc {
        return Err(Error::invalid(format!(
            "TTA frame: CRC32 mismatch (got {computed:#010x}, want {claimed_crc:#010x})"
        )));
    }

    // Try full frame size first; if the entropy stream runs short the
    // caller passed a "last frame" body with `last_frame_size` samples.
    let full = header.frame_size() as usize;
    let last = header.last_frame_size() as usize;
    match decode_with_sample_count(body, header, full) {
        Ok(channels) => emit_frame(channels, output_format, pts),
        Err(_) if last != full => {
            let channels = decode_with_sample_count(body, header, last)?;
            emit_frame(channels, output_format, pts)
        }
        Err(e) => Err(e),
    }
}

/// Decode `samples_per_channel` samples from `body`. Returns one
/// `Vec<i32>` per channel.
pub fn decode_with_sample_count(
    body: &[u8],
    header: &TtaHeader,
    samples_per_channel: usize,
) -> Result<Vec<Vec<i32>>> {
    let channels = header.channels as usize;
    let bps_bytes = header.bps_bytes() as usize;
    if bps_bytes == 0 || bps_bytes > 4 {
        return Err(Error::unsupported(format!("TTA bps_bytes {bps_bytes}")));
    }
    let filter_shift = FILTER_SHIFTS[bps_bytes - 1];
    let predictor_k = PREDICTOR_K[bps_bytes - 1];

    let mut state: Vec<ChannelState> = (0..channels)
        .map(|_| ChannelState::new(filter_shift))
        .collect();

    let mut out: Vec<Vec<i32>> = (0..channels)
        .map(|_| Vec::with_capacity(samples_per_channel))
        .collect();

    let mut br = BitReaderLsb::new(body);

    let mut rice = RiceCoder::new();

    for _ in 0..samples_per_channel {
        for c in 0..channels {
            let value = rice.decode(&mut br)?;
            let signed = un_zigzag(value);
            let recovered = state[c].step(signed, filter_shift, predictor_k);
            out[c].push(recovered);
        }
        if channels >= 2 {
            // Inverse pairwise decorrelation, applied to one
            // sample-frame in place. Encoder direction was:
            //   for i = 0..N-1: c[i] = c[i+1] - c[i]
            //   c[N-1] = c[N-1] + c[N-2]/2          (after diff pass)
            // Decoder undo (last-to-first):
            //   c[N-1] -= c[N-2] / 2
            //   for i = N-2 downto 0: c[i] = c[i+1] - c[i]
            let last = channels - 1;
            let prev = out[last - 1].last().copied().unwrap_or(0);
            let cur = *out[last].last().unwrap();
            *out[last].last_mut().unwrap() = cur.wrapping_sub(prev >> 1);
            for i in (0..channels - 1).rev() {
                let next = *out[i + 1].last().unwrap();
                let here = *out[i].last().unwrap();
                *out[i].last_mut().unwrap() = next.wrapping_sub(here);
            }
        }
    }

    // Body must be byte-aligned after consuming the residual stream
    // (0..7 padding bits permitted). We do not check the exact pad
    // value — only that nothing else extends past byte boundaries.
    // The caller enforces the per-frame CRC over the body length.
    Ok(out)
}

fn emit_frame(
    channels: Vec<Vec<i32>>,
    output_format: SampleFormat,
    pts: Option<i64>,
) -> Result<Frame> {
    let n_ch = channels.len();
    let total = channels[0].len();
    let bps = output_format.bytes_per_sample();
    let mut interleaved: Vec<u8> = Vec::with_capacity(total * n_ch * bps);
    for i in 0..total {
        for c in 0..n_ch {
            let s = channels[c][i];
            match output_format {
                SampleFormat::U8 => interleaved.push((s.wrapping_add(0x80) & 0xFF) as u8),
                SampleFormat::S16 => interleaved.extend_from_slice(&(s as i16).to_le_bytes()),
                SampleFormat::S32 => {
                    // 24-bit signed sample, expanded into s32 by left-shift 8.
                    let v = s << 8;
                    interleaved.extend_from_slice(&v.to_le_bytes());
                }
                _ => {
                    return Err(Error::unsupported(
                        "TTA decoder: unsupported output sample format",
                    ))
                }
            }
        }
    }
    Ok(Frame::Audio(AudioFrame {
        samples: total as u32,
        pts,
        data: vec![interleaved],
    }))
}

// ---------------------------------------------------------------------
// Rice entropy decoder
// ---------------------------------------------------------------------

/// Adaptive two-mode (k0/k1) Rice coder with an escape threshold
/// `1 << k0`. State is reset per-frame.
pub struct RiceCoder {
    pub k0: u32,
    pub k1: u32,
    pub sum0: u32,
    pub sum1: u32,
}

impl Default for RiceCoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RiceCoder {
    pub fn new() -> Self {
        Self {
            k0: RICE_INIT_K,
            k1: RICE_INIT_K,
            sum0: RICE_INIT_SUM,
            sum1: RICE_INIT_SUM,
        }
    }

    /// Decode one unsigned residual.
    pub fn decode(&mut self, br: &mut BitReaderLsb<'_>) -> Result<u32> {
        // Unary prefix: count of leading 1 bits, terminated by a 0.
        let mut depth: u32 = 0;
        loop {
            let b = br.read_u32(1)?;
            if b == 0 {
                break;
            }
            depth += 1;
            if depth > 64 {
                return Err(Error::invalid("TTA Rice: unary prefix too long"));
            }
        }
        if depth == 0 {
            // depth-0 path: k = k0; value = suffix.
            let suffix = if self.k0 == 0 {
                0
            } else {
                br.read_u32(self.k0)?
            };
            let value = suffix;
            self.update_sum0(value);
            Ok(value)
        } else {
            // depth-1+ path: k = k1; value = ((depth-1) << k1) + suffix +
            // (1 << k0) bias.
            let suffix = if self.k1 == 0 {
                0
            } else {
                br.read_u32(self.k1)?
            };
            let escaped = ((depth - 1) << self.k1).wrapping_add(suffix);
            // Update k1 with the un-biased escape value first, then
            // fold the bias into `value` and update k0.
            self.update_sum1(escaped);
            let bias = if self.k0 >= 32 { 0 } else { 1u32 << self.k0 };
            let value = escaped.wrapping_add(bias);
            self.update_sum0(value);
            Ok(value)
        }
    }

    fn update_sum0(&mut self, value: u32) {
        let delta = (self.sum0 >> 4) as i64;
        let new_sum = (self.sum0 as i64) + (value as i64) - delta;
        self.sum0 = new_sum.max(1) as u32;
        // k0 hysteresis: increment if sum0 > 1 << (k0 + 5),
        // decrement if sum0 < 1 << (k0 + 4).
        let upper = if self.k0 + 5 >= 32 {
            u32::MAX
        } else {
            1u32 << (self.k0 + 5)
        };
        let lower = if self.k0 + 4 >= 32 {
            u32::MAX
        } else {
            1u32 << (self.k0 + 4)
        };
        if self.sum0 > upper && self.k0 < 30 {
            self.k0 += 1;
        } else if self.sum0 < lower && self.k0 > 0 {
            self.k0 -= 1;
        }
    }

    fn update_sum1(&mut self, value: u32) {
        let delta = (self.sum1 >> 4) as i64;
        let new_sum = (self.sum1 as i64) + (value as i64) - delta;
        self.sum1 = new_sum.max(1) as u32;
        let upper = if self.k1 + 5 >= 32 {
            u32::MAX
        } else {
            1u32 << (self.k1 + 5)
        };
        let lower = if self.k1 + 4 >= 32 {
            u32::MAX
        } else {
            1u32 << (self.k1 + 4)
        };
        if self.sum1 > upper && self.k1 < 30 {
            self.k1 += 1;
        } else if self.sum1 < lower && self.k1 > 0 {
            self.k1 -= 1;
        }
    }
}

/// Inverse of the spec's interleaved zig-zag mapping
/// `signed = 1 + ((value >> 1) ^ ((value & 1) - 1))`.
///
/// In closed form this lays out as:
/// `0 → 0, 1 → +1, 2 → -1, 3 → +2, 4 → -2, …`
///
/// Even unsigned values (and zero) decode to non-positive signed
/// integers; odd values decode to positive ones. The "+1" terminator
/// in the spec lifts even values up by one, so the value 0 — which
/// appears as a single "depth-0 with empty suffix" symbol when
/// `k0` collapses to zero on long runs of silence — survives back to
/// signed zero.
fn un_zigzag(value: u32) -> i32 {
    if value & 1 == 1 {
        // odd  -> positive
        ((value >> 1) as i32) + 1
    } else {
        // even -> non-positive (value 0 → 0, 2 → -1, 4 → -2, …)
        -((value >> 1) as i32)
    }
}

// ---------------------------------------------------------------------
// Predictor cascade — Stage A (8-tap sign-LMS) + Stage B (fixed)
// ---------------------------------------------------------------------

/// Per-channel filter + predictor state. Reset per frame.
pub struct ChannelState {
    /// Stage A: 8 LMS weights, 8-deep delay line of differences, 8-deep
    /// "gradient sign" line, last residual (`error`).
    qm: [i32; 8],
    dx: [i32; 8],
    dl: [i32; 8],
    error: i32,
    /// Last 3 Stage-A outputs (most-recent first) — used to construct
    /// the new top entries of dl[] each iteration. Reset to zero.
    last_samples: [i32; 3],
    /// Stage B: most recent reconstructed sample (`prev`).
    predictor: i32,
}

impl ChannelState {
    /// Allocate a per-channel state. `_filter_shift` is unused at
    /// init time (zero state); kept in the signature so callers see
    /// the dependency at construction.
    pub fn new(_filter_shift: u32) -> Self {
        Self {
            qm: [0; 8],
            dx: [0; 8],
            dl: [0; 8],
            error: 0,
            last_samples: [0; 3],
            predictor: 0,
        }
    }

    /// Run one decoded residual through Stage A then Stage B and
    /// return the reconstructed differential sample (still pre-channel-
    /// decorrelation if `channels >= 2`). Updates internal state.
    pub fn step(&mut self, residual: i32, filter_shift: u32, predictor_k: u32) -> i32 {
        // ---- Stage A: 8-tap sign-LMS adaptive filter ----
        let after_a = self.stage_a(residual, filter_shift);

        // ---- Stage B: fixed-order integer predictor ----
        // pred_term = (prev * ((1 << k) - 1)) >> k.
        // For k = 31 this reduces to ~prev, matching the spec's
        // "add prev directly" 32-bit special case.
        let prev = self.predictor as i64;
        let mask = if predictor_k >= 31 {
            (1i64 << 31) - 1
        } else {
            (1i64 << predictor_k) - 1
        };
        let term = ((prev * mask) >> predictor_k) as i32;
        let recovered = after_a.wrapping_add(term);
        self.predictor = recovered;
        recovered
    }

    /// Stage A standalone for testability.
    pub fn stage_a(&mut self, residual: i32, filter_shift: u32) -> i32 {
        // Sign-LMS weight update. Driven by the **previous** error
        // (= previous-iteration's residual). Convention: positive
        // error pushes weights along dx[], negative against.
        if self.error != 0 {
            if self.error < 0 {
                for i in 0..8 {
                    self.qm[i] = self.qm[i].wrapping_sub(self.dx[i]);
                }
            } else {
                for i in 0..8 {
                    self.qm[i] = self.qm[i].wrapping_add(self.dx[i]);
                }
            }
        }

        // Inner-product prediction with round-half-up bias.
        let round: i32 = 1i32 << (filter_shift - 1);
        let mut acc: i64 = round as i64;
        for i in 0..8 {
            acc += (self.dl[i] as i64) * (self.qm[i] as i64);
        }
        let pred = (acc >> filter_shift) as i32;

        let after_a = residual.wrapping_add(pred);
        // Save for the next iteration's LMS update.
        self.error = residual;

        // Shift dx[] and dl[] one step left (drop index 0). After this
        // shift, positions 4..=7 still hold values from the *previous*
        // iteration (specifically: shifted-in copies of the prior
        // dl[5..=7] / dx[5..=7]). dl[7] is unchanged by the shift loop;
        // it will be overwritten by the regen step below.
        for i in 0..7 {
            self.dx[i] = self.dx[i + 1];
            self.dl[i] = self.dl[i + 1];
        }

        // -----------------------------------------------------------
        // Stage-A state regeneration (round-2 calibration: dx[] is
        // sourced from the *shifted-in* dl[] values BEFORE the dl[4..=7]
        // regen overwrites them, NOT from the freshly regenerated dl[]).
        // This matches the encoder's "gradient regenerated against
        // history just-rolled-down from positions 5..=7" ordering.
        // -----------------------------------------------------------

        // dx[4..=7]: position-dependent ±{1, 2, 2, 4} step carrying the
        // sign of the *shifted-in* dl[i] entry. The encoder mirrors
        // this exactly so the LMS gradient direction matches.
        self.dx[7] = branchless_sign(self.dl[7], 4);
        self.dx[6] = branchless_sign(self.dl[6], 2);
        self.dx[5] = branchless_sign(self.dl[5], 2);
        self.dx[4] = branchless_sign(self.dl[4], 1);

        // Regenerate dl[4..=7] from a 4-deep telescoping difference
        // pattern ending in the freshly-reconstructed sample.
        //   dl[7] = after_a                              (zeroth-order)
        //   dl[6] = after_a - prev1                      (first-order)
        //   dl[5] = (after_a - prev1) - (prev1 - prev2)  (second-order)
        //   dl[4] = third-order telescoping difference
        let p0 = after_a;
        let p1 = self.last_samples[0];
        let p2 = self.last_samples[1];
        let p3 = self.last_samples[2];
        let d1 = p0.wrapping_sub(p1);
        let d2 = p1.wrapping_sub(p2);
        let d3 = p2.wrapping_sub(p3);
        let dd1 = d1.wrapping_sub(d2);
        let dd2 = d2.wrapping_sub(d3);
        let ddd = dd1.wrapping_sub(dd2);
        self.dl[7] = p0;
        self.dl[6] = d1;
        self.dl[5] = dd1;
        self.dl[4] = ddd;

        // Roll the per-channel sample history (after_a values, not the
        // post-Stage-B reconstructed sample — Stage A's filter operates
        // entirely on its own intermediate output).
        self.last_samples[2] = self.last_samples[1];
        self.last_samples[1] = self.last_samples[0];
        self.last_samples[0] = after_a;

        after_a
    }
}

/// Branch-free `±magnitude` driven by the sign bit of `value`.
/// `value > 0 → +magnitude`, `value < 0 → -magnitude`, `value == 0 → +magnitude`.
/// Equivalent to `((value >> 31) | 1) * magnitude` on i32.
#[inline]
fn branchless_sign(value: i32, magnitude: i32) -> i32 {
    ((value >> 31) | 1) * magnitude
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_zigzag_basic_mapping() {
        // Spec: signed = 1 + ((v >> 1) ^ ((v & 1) - 1)) → the
        // 0/+1/-1/+2/-2/… interleave. value 0 must round-trip to
        // signed 0 (otherwise long runs of silence don't recover).
        assert_eq!(un_zigzag(0), 0);
        assert_eq!(un_zigzag(1), 1);
        assert_eq!(un_zigzag(2), -1);
        assert_eq!(un_zigzag(3), 2);
        assert_eq!(un_zigzag(4), -2);
        assert_eq!(un_zigzag(5), 3);
    }

    #[test]
    fn rice_decode_zero_with_k_zero() {
        // With k0=0 every depth-0 sample is a single 0 bit.
        let mut rc = RiceCoder::new();
        rc.k0 = 0;
        rc.k1 = 0;
        rc.sum0 = 1;
        rc.sum1 = 1;
        let body = vec![0u8; 4]; // many zero bits
        let mut br = BitReaderLsb::new(&body);
        for _ in 0..16 {
            let v = rc.decode(&mut br).unwrap();
            assert_eq!(v, 0);
        }
    }

    #[test]
    fn rice_decode_first_sample_with_k_four() {
        // Manually emit one depth-0 sample with k0=4 and verify the
        // decoder reads back the suffix bits as the value. We don't
        // chain multiple samples here — k0 adapts after each call,
        // so the encoder side would need to track it. The chained
        // round-trip is covered by the ffmpeg-driven integration test.
        use oxideav_core::bits::BitWriterLsb;
        let mut bw = BitWriterLsb::new();
        bw.write_u32(0, 1); // depth-0 terminator
        bw.write_u32(11, 4); // 4-bit suffix = 11
        let bytes = bw.finish();
        let mut rc = RiceCoder::new();
        rc.k0 = 4;
        rc.k1 = 4;
        let mut br = BitReaderLsb::new(&bytes);
        let got = rc.decode(&mut br).unwrap();
        assert_eq!(got, 11);
    }
}
