#![no_main]

//! Drive the framework [`oxideav_core::Decoder`] trait — the surface
//! `oxideav-meta::register_all` actually hands the host application —
//! with arbitrary fuzz-supplied bytes (round 336).
//!
//! The existing `decode` target hammers the free-function
//! [`oxideav_tta::decode`] entry point, and the `demuxer` target (round
//! 299) hammers the registered [`oxideav_core::Demuxer`]. Neither runs
//! the registered **`Decoder` trait adapter** (`TtaDecoder` in
//! `src/registry.rs`): the `send_packet` → `receive_frame` → `flush`
//! state machine, the channel / bits-per-sample sanity rails it asserts
//! against the demuxer-configured `CodecParameters`, and the
//! `pcm_pack_for_format` repack that turns the decoder's interleaved
//! `i32` samples into the byte layout an `AudioFrame` carries (S16 →
//! 2 bytes LE, S24 → 3 bytes LE). That repack + rail layer is the glue
//! the framework relies on and it is exercised by no other target.
//!
//! This target stitches the two halves of the framework pipeline
//! together exactly as a host does:
//!
//! 1. [`ContainerRegistry::open_demuxer`] parses the TTA1 header + seek
//!    table from attacker bytes and exposes a single [`StreamInfo`].
//! 2. [`CodecRegistry::first_decoder`] is asked for a `Box<dyn Decoder>`
//!    from that stream's [`CodecParameters`] — driving `make_decoder`'s
//!    `SampleFormat` → expected-bps mapping and its missing-field
//!    rejections.
//! 3. Each demuxer packet is fed through `send_packet` / `receive_frame`
//!    (the double-send guard, the per-packet one-shot decode, the
//!    channel + bps rails, and `pcm_pack_for_format`), then `flush`
//!    flips the decoder to the `Eof` terminal state.
//!
//! The contract under test is purely that the calls *return*: a
//! malformed stream is rejected with a typed `oxideav-core` error at
//! some stage, and a well-formed one runs the whole pipeline without a
//! panic, integer overflow (in a debug build), index-out-of-bounds, or
//! OOM. Return values are intentionally discarded.
//!
//! Where the decoder *does* produce an `AudioFrame`, the one
//! attacker-independent invariant the framework guarantees is asserted:
//! the packed byte length equals `decoded_sample_count *
//! bytes_per_sample` for the configured output format. A packer that
//! emitted a short or over-long buffer would silently corrupt every
//! downstream mux, so this is the framework analogue of the bit-exact
//! roundtrip the free-function targets pin.
//!
//! The harness body is clean-room: no reference-implementation oracle,
//! only the panic-free / typed-error / packed-length contract.

use libfuzzer_sys::fuzz_target;

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecResolver, Frame, ProbeContext, ReadSeek, RuntimeContext, SampleFormat,
};
use oxideav_tta::register;

/// A resolver that never maps a tag — the TTA demuxer self-describes
/// its single stream and ignores the resolver, mirroring the `demuxer`
/// target.
struct NoopResolver;
impl CodecResolver for NoopResolver {
    fn resolve_tag(&self, _ctx: &ProbeContext) -> Option<CodecId> {
        None
    }
}

/// Cap the number of packets fed through the decoder so a pathological
/// seek table cannot turn the drive loop unbounded. A real `.tta` seek
/// table is `4 * frame_count + 4` bytes, so the frame count is bounded
/// by the input length anyway, but the cap keeps the per-iteration cost
/// predictable for the fuzzer.
const MAX_PACKETS: usize = 4096;

/// Bytes-per-sample for the AudioFrame packed layout, matching
/// `registry::pcm_pack_for_format`.
fn packed_bytes_per_sample(fmt: SampleFormat) -> Option<usize> {
    match fmt {
        SampleFormat::S16 => Some(2),
        SampleFormat::S24 => Some(3),
        _ => None,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);

    let resolver = NoopResolver;

    // ── 1. Open through the public registry demuxer path ───────────
    // Most malformed inputs are rejected here (bad magic / CRC /
    // out-of-range header fields / truncated seek table) with a typed
    // error — that is the contractually correct outcome.
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    let mut demuxer = match ctx.containers.open_demuxer("tta", input, &resolver) {
        Ok(d) => d,
        Err(_) => return,
    };

    // The TTA raw container always exposes exactly one stream; if it
    // somehow has none, there is nothing to decode.
    let streams = demuxer.streams();
    if streams.is_empty() {
        return;
    }
    let params = streams[0].params.clone();
    let configured_format = params.sample_format;

    // ── 2. Build the framework Decoder via the codec registry ──────
    // This drives `make_decoder`'s SampleFormat → expected-bps mapping
    // and its missing-channels / unsupported-format rejections. A
    // rejection here is a valid typed error, not a fuzz finding.
    let mut decoder = match ctx.codecs.first_decoder(&params) {
        Ok(d) => d,
        Err(_) => return,
    };

    // ── 3. Drive send_packet → receive_frame → flush ───────────────
    // Pull each demuxer packet and run it through the framework decoder
    // adapter. The packed-length invariant is asserted on every frame
    // the decoder successfully produces.
    for _ in 0..MAX_PACKETS {
        let pkt = match demuxer.next_packet() {
            Ok(p) => p,
            Err(_) => break,
        };

        // `send_packet` must always return (Ok, or the double-send
        // guard error if a prior frame is still pending — we never
        // double-send, so a clean adapter returns Ok).
        if decoder.send_packet(&pkt).is_err() {
            continue;
        }

        match decoder.receive_frame() {
            Ok(Frame::Audio(audio)) => {
                // Packed-length invariant: the byte buffer the adapter
                // emits must be exactly `decoded_samples *
                // bytes_per_sample` for the configured output format.
                if let Some(fmt) = configured_format {
                    if let Some(bps) = packed_bytes_per_sample(fmt) {
                        // The decoder emits one data plane.
                        if audio.data.len() == 1 {
                            let plane_len = audio.data[0].len();
                            // The plane length must be a whole number of
                            // samples …
                            assert_eq!(
                                plane_len % bps,
                                0,
                                "packed audio plane {plane_len} not a multiple of \
                                 bytes-per-sample {bps} (fmt={fmt:?})"
                            );
                            // … and the sample count it implies must be
                            // consistent with the frame's declared
                            // per-frame sample budget (samples *
                            // channels), so the packer can neither drop
                            // nor duplicate a sample.
                            if let Some(channels) = params.channels {
                                let expected = (audio.samples as usize) * (channels as usize) * bps;
                                assert_eq!(
                                    plane_len, expected,
                                    "packed audio plane {plane_len} != \
                                     samples({}) * channels({channels}) * bps({bps}) = {expected}",
                                    audio.samples
                                );
                            }
                        }
                    }
                }
            }
            Ok(_) => {
                // The TTA decoder only ever yields audio frames; any
                // other variant would be a framework contract break, but
                // we tolerate it without asserting to keep the target
                // forward-compatible with future Frame variants.
            }
            Err(_) => {
                // A per-packet decode rejection (corrupt frame CRC,
                // channel / bps skew against the configured params) is a
                // valid typed error. The decoder must stay usable, so
                // keep draining subsequent packets.
            }
        }
    }

    // `flush` flips the decoder to its EOF terminal state; it must not
    // panic and a post-flush `receive_frame` with no pending packet must
    // surface `Eof` rather than blocking or panicking.
    let _ = decoder.flush();
    let _ = decoder.receive_frame();
});
