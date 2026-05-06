//! Spec/06 debug-build trace emitter.
//!
//! When the `trace` Cargo feature is enabled AND the environment
//! variable `OXIDEAV_TTA_TRACE_FILE` is set to a writable path, a
//! [`TraceWriter`] is opened at decoder construction and one TSV line
//! per state-transition event is written to it. With the feature
//! enabled but the env var unset, [`TraceWriter::open_from_env`]
//! returns `None` and every emit becomes a single `Option::is_some`
//! check on the cold path — the decoder behaves byte-for-byte
//! identically to a release build.
//!
//! When the `trace` Cargo feature is OFF, this module compiles to an
//! empty stub: every call site in `decoder.rs` is gated behind
//! `#[cfg(feature = "trace")]`, so the release build pays zero cost.
//!
//! See `docs/audio/tta-cleanroom/spec/06-trace-contract.md` for the
//! 18-event vocabulary, value formats, ordering rules, and counter
//! discipline this emitter implements.

#![cfg(feature = "trace")]
// Each `ev_*` method's argument list mirrors a single event's
// spec/06 field schema; trimming arity here would require packing
// fields into a struct just to unpack them inside the method, which
// loses the point.
#![allow(clippy::too_many_arguments)]

use std::cell::RefCell;
use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

thread_local! {
    /// Thread-local override for the trace path. When `Some`, takes
    /// precedence over `OXIDEAV_TTA_TRACE_FILE`. This exists so the
    /// crate's own tests (which run in parallel and would otherwise
    /// race on a process-global env var) can pin their trace path
    /// to a per-thread tmp file. Production users should set the
    /// env var per spec/06 §2.
    static TRACE_PATH_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Pin a per-thread override for the trace path. When set, takes
/// precedence over `OXIDEAV_TTA_TRACE_FILE`. Pass `None` to clear.
///
/// Test-only convenience — production users should use the env-var
/// contract per spec/06 §2.
#[allow(dead_code)] // used by `#[cfg(test)]` callers; keep visible to the trace user.
pub fn set_thread_trace_path(path: Option<PathBuf>) {
    TRACE_PATH_OVERRIDE.with(|cell| *cell.borrow_mut() = path);
}

/// Buffered TSV-line emitter for the debug trace tape.
///
/// One instance per decoder invocation. Lines are buffered through a
/// `BufWriter`; the file is flushed and closed when the writer is
/// dropped. Mid-decode panics may leave a partial tape, but every
/// completed line is intact (line-buffered semantics).
pub struct TraceWriter {
    inner: BufWriter<File>,
    line: String,
}

impl TraceWriter {
    /// Inspect `OXIDEAV_TTA_TRACE_FILE` (or the thread-local
    /// override set by [`set_thread_trace_path`], if present) and
    /// open the file on the path it names (truncating if it
    /// exists). Returns `None` if neither is set / both are empty.
    pub fn open_from_env() -> Option<Self> {
        let path: Option<PathBuf> = TRACE_PATH_OVERRIDE
            .with(|cell| cell.borrow().clone())
            .or_else(|| env::var_os("OXIDEAV_TTA_TRACE_FILE").map(PathBuf::from));
        let path = path?;
        if path.as_os_str().is_empty() {
            return None;
        }
        let file = File::create(Path::new(&path)).ok()?;
        Some(Self {
            inner: BufWriter::new(file),
            line: String::with_capacity(256),
        })
    }

    /// Begin a new event line (`ev=<NAME>`). Subsequent
    /// [`Self::field_*`] calls append `\t<key>=<value>` records;
    /// [`Self::flush_line`] writes the assembled line + `\n`.
    fn begin(&mut self, name: &str) {
        self.line.clear();
        self.line.push_str("ev=");
        self.line.push_str(name);
    }

    fn field_u(&mut self, key: &str, value: u64) {
        use core::fmt::Write as _;
        self.line.push('\t');
        self.line.push_str(key);
        self.line.push('=');
        let _ = write!(self.line, "{value}");
    }

    fn field_i(&mut self, key: &str, value: i64) {
        use core::fmt::Write as _;
        self.line.push('\t');
        self.line.push_str(key);
        self.line.push('=');
        let _ = write!(self.line, "{value}");
    }

    fn field_b(&mut self, key: &str, value: bool) {
        self.line.push('\t');
        self.line.push_str(key);
        self.line.push('=');
        self.line.push(if value { '1' } else { '0' });
    }

    fn field_hex32(&mut self, key: &str, value: u32) {
        use core::fmt::Write as _;
        self.line.push('\t');
        self.line.push_str(key);
        self.line.push('=');
        let _ = write!(self.line, "0x{value:08x}");
    }

    fn field_arr_i(&mut self, key: &str, values: &[i32]) {
        use core::fmt::Write as _;
        self.line.push('\t');
        self.line.push_str(key);
        self.line.push('=');
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                self.line.push(',');
            }
            let _ = write!(self.line, "{v}");
        }
    }

    fn flush_line(&mut self) {
        self.line.push('\n');
        // Ignore write errors — the trace tape is a debugging
        // convenience, not part of the decoder's correctness contract.
        let _ = self.inner.write_all(self.line.as_bytes());
        let _ = self.inner.flush();
    }

    // -------- Container-level events (§5.1) --------

    pub fn ev_file_header(
        &mut self,
        magic_ok: bool,
        format: u32,
        nch: u32,
        bps: u32,
        sample_rate: u32,
        samples_total: u32,
    ) {
        self.begin("FILE_HEADER");
        self.field_b("magic_ok", magic_ok);
        self.field_u("format", format as u64);
        self.field_u("nch", nch as u64);
        self.field_u("bps", bps as u64);
        self.field_u("sample_rate", sample_rate as u64);
        self.field_u("samples_total", samples_total as u64);
        self.flush_line();
    }

    pub fn ev_header_crc(&mut self, crc_ok: bool, computed_crc: u32) {
        self.begin("HEADER_CRC");
        self.field_b("crc_ok", crc_ok);
        self.field_hex32("computed_crc", computed_crc);
        self.flush_line();
    }

    pub fn ev_seek_table_begin(&mut self, frame_count: u32) {
        self.begin("SEEK_TABLE_BEGIN");
        self.field_u("frame_count", frame_count as u64);
        self.flush_line();
    }

    pub fn ev_seek_entry(&mut self, frame_idx: u32, byte_length: u32, cumulative_offset: u64) {
        self.begin("SEEK_ENTRY");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("byte_length", byte_length as u64);
        self.field_u("cumulative_offset", cumulative_offset);
        self.flush_line();
    }

    pub fn ev_seek_table_end(&mut self, crc_ok: bool) {
        self.begin("SEEK_TABLE_END");
        self.field_b("crc_ok", crc_ok);
        self.flush_line();
    }

    // -------- Per-frame init events (§5.2) --------

    pub fn ev_lms_init(&mut self, frame_idx: i32, channel: u32, shift: i32, round: i32) {
        self.begin("LMS_INIT");
        self.field_i("frame_idx", frame_idx as i64);
        self.field_u("channel", channel as u64);
        self.field_i("shift", shift as i64);
        self.field_i("round", round as i64);
        self.flush_line();
    }

    pub fn ev_rice_k_init(
        &mut self,
        frame_idx: i32,
        channel: u32,
        k0_init: u32,
        k1_init: u32,
        sum0_init: u32,
        sum1_init: u32,
    ) {
        self.begin("RICE_K_INIT");
        self.field_i("frame_idx", frame_idx as i64);
        self.field_u("channel", channel as u64);
        self.field_u("k0_init", k0_init as u64);
        self.field_u("k1_init", k1_init as u64);
        self.field_u("sum0_init", sum0_init as u64);
        self.field_u("sum1_init", sum1_init as u64);
        self.flush_line();
    }

    // -------- Per-frame markers (§5.3) --------

    pub fn ev_frame_begin(&mut self, frame_idx: u32, expected_samples: u32) {
        self.begin("FRAME_BEGIN");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("expected_samples", expected_samples as u64);
        self.flush_line();
    }

    pub fn ev_frame_end(&mut self, frame_idx: u32, computed_crc: u32, expected_crc: u32, ok: bool) {
        self.begin("FRAME_END");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_hex32("computed_crc", computed_crc);
        self.field_hex32("expected_crc", expected_crc);
        self.field_b("ok", ok);
        self.flush_line();
    }

    // -------- Per-step events (§5.4) --------

    pub fn ev_rice_decode(
        &mut self,
        frame_idx: u32,
        step_idx: u32,
        channel: u32,
        raw_unary: u32,
        mode: bool,
        k_used: u32,
        residual_signed: i32,
    ) {
        self.begin("RICE_DECODE");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("step_idx", step_idx as u64);
        self.field_u("channel", channel as u64);
        self.field_u("raw_unary", raw_unary as u64);
        self.field_b("mode", mode);
        self.field_u("k_used", k_used as u64);
        self.field_i("residual_signed", residual_signed as i64);
        self.flush_line();
    }

    pub fn ev_rice_k_update(
        &mut self,
        frame_idx: u32,
        step_idx: u32,
        channel: u32,
        k0_post: u32,
        k1_post: u32,
        sum0_post: u32,
        sum1_post: u32,
    ) {
        self.begin("RICE_K_UPDATE");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("step_idx", step_idx as u64);
        self.field_u("channel", channel as u64);
        self.field_u("k0_post", k0_post as u64);
        self.field_u("k1_post", k1_post as u64);
        self.field_u("sum0_post", sum0_post as u64);
        self.field_u("sum1_post", sum1_post as u64);
        self.flush_line();
    }

    pub fn ev_lms_pre(
        &mut self,
        frame_idx: u32,
        step_idx: u32,
        channel: u32,
        dl_pre: &[i32; 8],
        dx_pre: &[i32; 8],
        qm_pre: &[i32; 8],
    ) {
        self.begin("LMS_PRE");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("step_idx", step_idx as u64);
        self.field_u("channel", channel as u64);
        self.field_arr_i("dl_pre", dl_pre);
        self.field_arr_i("dx_pre", dx_pre);
        self.field_arr_i("qm_pre", qm_pre);
        self.flush_line();
    }

    pub fn ev_stage_a_predict(
        &mut self,
        frame_idx: u32,
        step_idx: u32,
        channel: u32,
        predicted_a: i32,
        error_a: i32,
        sample_after_a: i32,
    ) {
        self.begin("STAGE_A_PREDICT");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("step_idx", step_idx as u64);
        self.field_u("channel", channel as u64);
        self.field_i("predicted_a", predicted_a as i64);
        self.field_i("error_a", error_a as i64);
        self.field_i("sample_after_a", sample_after_a as i64);
        self.flush_line();
    }

    pub fn ev_lms_post(
        &mut self,
        frame_idx: u32,
        step_idx: u32,
        channel: u32,
        dl_post: &[i32; 8],
        dx_post: &[i32; 8],
        qm_post: &[i32; 8],
        error_pre: i32,
    ) {
        self.begin("LMS_POST");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("step_idx", step_idx as u64);
        self.field_u("channel", channel as u64);
        self.field_arr_i("dl_post", dl_post);
        self.field_arr_i("dx_post", dx_post);
        self.field_arr_i("qm_post", qm_post);
        self.field_i("error_pre", error_pre as i64);
        self.flush_line();
    }

    pub fn ev_stage_b_predict(
        &mut self,
        frame_idx: u32,
        step_idx: u32,
        channel: u32,
        prev_in: i32,
        predicted_b: i32,
        residual_b: i32,
        sample_after_b: i32,
    ) {
        self.begin("STAGE_B_PREDICT");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("step_idx", step_idx as u64);
        self.field_u("channel", channel as u64);
        self.field_i("prev_in", prev_in as i64);
        self.field_i("predicted_b", predicted_b as i64);
        self.field_i("residual_b", residual_b as i64);
        self.field_i("sample_after_b", sample_after_b as i64);
        self.flush_line();
    }

    // -------- Per-sample events (§5.5) --------

    pub fn ev_decorr_pre(&mut self, frame_idx: u32, sample_idx: u32, raw_per_channel: &[i32]) {
        self.begin("DECORR_PRE");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("sample_idx", sample_idx as u64);
        self.field_u("nch", raw_per_channel.len() as u64);
        self.field_arr_i("raw_per_channel", raw_per_channel);
        self.flush_line();
    }

    pub fn ev_decorr_post(
        &mut self,
        frame_idx: u32,
        sample_idx: u32,
        decorrelated_per_channel: &[i32],
    ) {
        self.begin("DECORR_POST");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("sample_idx", sample_idx as u64);
        self.field_u("nch", decorrelated_per_channel.len() as u64);
        self.field_arr_i("decorrelated_per_channel", decorrelated_per_channel);
        self.flush_line();
    }

    pub fn ev_pcm_out(&mut self, frame_idx: u32, sample_idx: u32, final_per_channel: &[i32]) {
        self.begin("PCM_OUT");
        self.field_u("frame_idx", frame_idx as u64);
        self.field_u("sample_idx", sample_idx as u64);
        self.field_u("nch", final_per_channel.len() as u64);
        self.field_arr_i("final_per_channel", final_per_channel);
        self.flush_line();
    }
}
