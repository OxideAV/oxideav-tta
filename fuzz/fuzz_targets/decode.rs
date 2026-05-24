#![no_main]

//! Decode arbitrary fuzz-supplied bytes through the TTA framing path.
//!
//! The contract under test is purely that the calls *return*: a
//! malformed stream yields `Err(oxideav_tta::Error::…)`, a well-formed
//! one yields `Ok((StreamInfo, Vec<i32>))`, and neither path may
//! panic, integer-overflow (in a debug build), index out of bounds, or
//! allocate an attacker-controlled sample buffer the size of the
//! claimed `total_samples * channels`. The return values are
//! intentionally discarded.
//!
//! Two entry points are exercised on every input:
//!
//! 1. [`oxideav_tta::decode`] — format=1 (integer PCM) single-shot
//!    decode. Format=2 streams short-circuit to
//!    `Err(Error::PasswordRequired)` here.
//! 2. [`oxideav_tta::decode_with_password`] — the format=2 path, fed
//!    the first byte of the input as a one-byte password so the
//!    qm-priming derivation (`spec/07` §3) is also driven by fuzz
//!    bytes. This covers the encrypted-stream framing without needing
//!    a separate corpus.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Format=1 framing + decode.
    let _ = oxideav_tta::decode(data);

    // Format=2 framing + decode. Derive the password from the input's
    // own bytes so the qm-priming path is fuzzed too; an empty input
    // uses an empty password (a valid, all-zero digest).
    let password = data.get(..1).unwrap_or(&[]);
    let _ = oxideav_tta::decode_with_password(data, password);
});
