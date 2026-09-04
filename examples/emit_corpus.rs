//! Emit the crate's deterministic synthetic corpus as `.tta` files with
//! raw-PCM sidecars, for black-box cross-checking against an external
//! TTA-capable decoder binary.
//!
//! Round 456. The crate's own verification is self-roundtrip plus the
//! `spec/` worked-step hand-verifications; this driver makes the
//! encoder's *output* available to any third-party decoder invoked as
//! a black box (only its behaviour is observed — never its source), so
//! "our bytes → their decoder → our input PCM" can be asserted
//! externally. Every stream here is built by the production
//! [`oxideav_tta::encode`] / [`oxideav_tta::encode_with_password`] from
//! the same xorshift tone-plus-noise generator the Criterion benches
//! and `tests/bitexact_pins.rs` use, so the emitted files are exactly
//! the pinned corpus.
//!
//! ```sh
//! cargo run --release --example emit_corpus -- <out-dir>
//! ```
//!
//! For each cell `<name>` the directory receives:
//!
//! * `<name>.tta` — the encoder's bytes (format=1, or format=2 for the
//!   `_pw_*` cells, whose password is the suffix after `_pw_`).
//! * `<name>.pcm` — the encoder's *input* samples, interleaved, packed
//!   little-endian at the cell's byte depth (2 bytes for 16-bit, 3 for
//!   17..=24-bit, MSB-aligned within the 3 bytes per `spec/01` §3.2 —
//!   the same packing [`oxideav_tta::pack_pcm`] produces).
//! * `<name>.meta` — one line: `channels bits_per_sample sample_rate
//!   n_samples`.

use std::fs;
use std::path::PathBuf;

use oxideav_tta::{encode, encode_with_password, pack_pcm};

fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Same generator as `tests/bitexact_pins.rs::build_pcm`.
fn build_pcm(n_samples: usize, channels: u16, bits_per_sample: u16) -> Vec<i32> {
    let nch = channels as usize;
    let mut out = Vec::with_capacity(n_samples * nch);
    let mut state: u32 = 0xCAFE_F00D;
    let amp: i32 = if bits_per_sample <= 16 {
        1 << 13
    } else {
        1 << (bits_per_sample - 3)
    };
    for s in 0..n_samples {
        let phase = (s % 256) as i32 - 128;
        let env = (phase / 128) * amp + (phase % 128) * (amp / 128);
        for ch in 0..nch {
            let noise = (xorshift32(&mut state) as i32) >> 24;
            let chan_bias = (ch as i32) * (amp / 16);
            out.push(env + chan_bias + noise);
        }
    }
    out
}

struct Cell {
    name: &'static str,
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    n_samples: usize,
    password: Option<&'static [u8]>,
}

fn main() {
    let out_dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tta-corpus"));
    fs::create_dir_all(&out_dir).expect("create output directory");

    let cells = [
        Cell {
            name: "mono16_44k1",
            channels: 1,
            bits_per_sample: 16,
            sample_rate: 44_100,
            n_samples: 50_000,
            password: None,
        },
        Cell {
            name: "stereo16_44k1",
            channels: 2,
            bits_per_sample: 16,
            sample_rate: 44_100,
            n_samples: 50_000,
            password: None,
        },
        Cell {
            name: "stereo17_44k1",
            channels: 2,
            bits_per_sample: 17,
            sample_rate: 44_100,
            n_samples: 50_000,
            password: None,
        },
        Cell {
            name: "stereo20_48k",
            channels: 2,
            bits_per_sample: 20,
            sample_rate: 48_000,
            n_samples: 52_000,
            password: None,
        },
        Cell {
            name: "stereo24_48k",
            channels: 2,
            bits_per_sample: 24,
            sample_rate: 48_000,
            n_samples: 52_000,
            password: None,
        },
        Cell {
            name: "3ch20_48k",
            channels: 3,
            bits_per_sample: 20,
            sample_rate: 48_000,
            n_samples: 52_000,
            password: None,
        },
        Cell {
            name: "6ch16_48k",
            channels: 6,
            bits_per_sample: 16,
            sample_rate: 48_000,
            n_samples: 52_000,
            password: None,
        },
        Cell {
            name: "6ch24_48k",
            channels: 6,
            bits_per_sample: 24,
            sample_rate: 48_000,
            n_samples: 52_000,
            password: None,
        },
        Cell {
            name: "stereo16_44k1_pw_pin-r386",
            channels: 2,
            bits_per_sample: 16,
            sample_rate: 44_100,
            n_samples: 50_000,
            password: Some(b"pin-r386"),
        },
        Cell {
            name: "stereo24_48k_pw_hunter2",
            channels: 2,
            bits_per_sample: 24,
            sample_rate: 48_000,
            n_samples: 52_000,
            password: Some(b"hunter2"),
        },
    ];

    for cell in &cells {
        let pcm = build_pcm(cell.n_samples, cell.channels, cell.bits_per_sample);
        let tta = match cell.password {
            Some(pw) => encode_with_password(
                &pcm,
                cell.channels,
                cell.bits_per_sample,
                cell.sample_rate,
                pw,
            )
            .expect("encode format=2"),
            None => {
                encode(&pcm, cell.channels, cell.bits_per_sample, cell.sample_rate).expect("encode")
            }
        };
        fs::write(out_dir.join(format!("{}.tta", cell.name)), &tta).expect("write .tta");
        fs::write(
            out_dir.join(format!("{}.pcm", cell.name)),
            pack_pcm(&pcm, cell.bits_per_sample),
        )
        .expect("write .pcm");
        fs::write(
            out_dir.join(format!("{}.meta", cell.name)),
            format!(
                "{} {} {} {}\n",
                cell.channels, cell.bits_per_sample, cell.sample_rate, cell.n_samples
            ),
        )
        .expect("write .meta");
        println!(
            "{:<28} {} bytes tta, {} samples x {} ch @ {} bps",
            cell.name,
            tta.len(),
            cell.n_samples,
            cell.channels,
            cell.bits_per_sample
        );
    }
}
