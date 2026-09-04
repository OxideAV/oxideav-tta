//! Decode a `.tta` file to packed little-endian PCM.
//!
//! Round 456. Companion to `emit_corpus`: where that driver hands the
//! *encoder's* output to an external decoder, this one takes an
//! *externally produced* `.tta` and writes what the crate's decoder
//! recovers, so "their bytes → our decoder → their input PCM" can be
//! asserted from the outside with nothing but a diff. Packing follows
//! `spec/01` §3.2 via [`oxideav_tta::pack_pcm`] (2 bytes per sample
//! for 16-bit, 3 bytes for 17..=24-bit).
//!
//! ```sh
//! cargo run --release --example decode_file -- <in.tta> <out.pcm> [password]
//! ```
//!
//! Prints the stream geometry on success; a decode error is reported
//! on stderr with a non-zero exit status.

use std::fs;
use std::process::ExitCode;

use oxideav_tta::{decode, decode_with_password, pack_pcm};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: decode_file <in.tta> <out.pcm> [password]");
        return ExitCode::from(2);
    }
    let bytes = match fs::read(&args[1]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {}: {e}", args[1]);
            return ExitCode::from(2);
        }
    };
    let result = match args.get(3) {
        Some(pw) => decode_with_password(&bytes, pw.as_bytes()),
        None => decode(&bytes),
    };
    let (info, samples) = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("decode {}: {e}", args[1]);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = fs::write(&args[2], pack_pcm(&samples, info.bits_per_sample)) {
        eprintln!("write {}: {e}", args[2]);
        return ExitCode::from(2);
    }
    println!(
        "format={} channels={} bits_per_sample={} sample_rate={} total_samples={} decoded_entries={}",
        info.format,
        info.channels,
        info.bits_per_sample,
        info.sample_rate,
        info.total_samples,
        samples.len()
    );
    ExitCode::SUCCESS
}
