// Copyright (C) 2026 sysretq0
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-only

use std::fs::{self, File, OpenOptions};

use std::io::{stdout, BufWriter, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormattedTime([u8; 12]);

impl std::ops::Deref for FormattedTime {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }
}

impl std::fmt::Display for FormattedTime {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self)
    }
}

#[inline(always)]
pub fn format_time_hms_ms(epoch_ms: u64) -> FormattedTime {
    let secs = (epoch_ms / 1000) as libc::time_t;
    let millis = epoch_ms % 1000;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    let mut buf = [0u8; 12];
    buf[0] = b'0' + (tm.tm_hour / 10) as u8;
    buf[1] = b'0' + (tm.tm_hour % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (tm.tm_min / 10) as u8;
    buf[4] = b'0' + (tm.tm_min % 10) as u8;
    buf[5] = b':';
    buf[6] = b'0' + (tm.tm_sec / 10) as u8;
    buf[7] = b'0' + (tm.tm_sec % 10) as u8;
    buf[8] = b'.';
    buf[9] = b'0' + ((millis / 100) % 10) as u8;
    buf[10] = b'0' + ((millis / 10) % 10) as u8;
    buf[11] = b'0' + (millis % 10) as u8;
    FormattedTime(buf)
}

/// Zero-allocation JSON string escaper. Returns Cow::Borrowed if no escaping is needed.
#[inline]
pub fn escape_json(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.bytes().any(|b| b == b'"' || b == b'\\' || b < 0x20) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    pub bg_spawns: u32,
    pub bg_deaths: u32,
    pub spawn_rss_kb: i64,
}

const ROTATION_THRESHOLD_BYTES: usize = 512 * 1024; // 512 KB

/// Whether fd 1 is a terminal. `TelemetrySink` caches the answer at construction
/// because consulting it per event would be a syscall per event; callers that run
/// before a sink exists (the startup banner) call it directly, costing one syscall
/// per process. `--json` overrides the answer either way, because that mode's
/// stdout is a machine contract rather than a human surface.
#[inline]
pub fn stdout_is_tty() -> bool {
    (unsafe { libc::isatty(libc::STDOUT_FILENO) }) == 1
}

pub struct TelemetrySink {
    /// The single source of truth for "file logging is on": `Some` means records are being
    /// appended, `None` means the log is switched off (`log_enabled = false` or `--no-log`) or
    /// the file could not be opened. Deliberately not a separate `bool` + `Option` pair, which
    /// could disagree with each other.
    writer: Option<BufWriter<File>>,
    bytes_written: usize,
    log_path: String,
    pub json_stdout: bool,
    /// Whether stdout is a terminal, i.e. whether the columnar table has a reader. Cached at
    /// construction because consulting `isatty` per event would be a syscall per event; `--json`
    /// overrides it either way, because that mode's stdout is a machine contract rather than a
    /// human surface.
    stdout_tty: bool,
}

impl TelemetrySink {
    pub fn new(log_path: &str, json_stdout: bool, log_enabled: bool) -> Self {
        Self::with_stdout(log_path, json_stdout, log_enabled, stdout_is_tty())
    }

    /// `stdout_tty` is a parameter rather than an `isatty()` call inside [`Self::new`] so tests
    /// stay deterministic: under `cargo test` fd 1 is a pipe, never a terminal.
    pub fn with_stdout(
        log_path: &str,
        json_stdout: bool,
        log_enabled: bool,
        stdout_tty: bool,
    ) -> Self {
        let mut current_bytes = 0;
        if let Ok(meta) = fs::metadata(log_path) {
            current_bytes = meta.len() as usize;
        }

        let mut sink = Self {
            writer: None,
            bytes_written: current_bytes,
            log_path: log_path.to_string(),
            json_stdout,
            stdout_tty,
        };

        if log_enabled {
            sink.open_log();
        }

        sink
    }

    /// Apply the `log_enabled` state (`daemon.conf`, `--no-log`). Disabling closes the file after
    /// flushing, so a disabled daemon leaves no `operations.log` behind and writes no bytes; a
    /// disabled-then-re-enabled log resumes in append mode at the same rotation threshold.
    /// stdout behaviour is independent: `--json` and the terminal table keep working.
    ///
    /// An open writer *is* the enabled state, so re-enabling after a failed open retries it here
    /// rather than treating the daemon as already logged.
    pub fn set_log_enabled(&mut self, on: bool) {
        if on == self.writer.is_some() {
            return;
        }
        if on {
            self.open_log();
        } else {
            self.flush();
            self.writer = None;
        }
    }

    /// Open the log for appending, rotating first if the existing file is already at the limit.
    ///
    /// The limit is re-read from the inode rather than trusted from `bytes_written`: the cache only
    /// moves while the daemon holds the file, so an `operations.log` that grew (or was replaced)
    /// while the log was switched off would otherwise be appended past the cap, and a truncated one
    /// would be rotated early.
    fn open_log(&mut self) {
        self.bytes_written = fs::metadata(&self.log_path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        if self.bytes_written >= ROTATION_THRESHOLD_BYTES {
            self.rotate();
        } else {
            self.open_file();
        }
    }

    /// Whether nothing at all will be rendered for this event: no open log file (either
    /// `log_enabled = false` or the file could not be opened), no `--json` contract, and no
    /// terminal to read the tabular column. The log file is the deciding factor for a background
    /// daemon -- `service.sh` redirects stdout to `/dev/null`, so `operations.log` must keep
    /// receiving records even though no closure output is printed.
    #[inline]
    pub fn silent(&self) -> bool {
        self.writer.is_none() && !self.json_stdout && !self.stdout_tty
    }

    /// Whether the columnar table actually reaches stdout. The startup header is printed under
    /// exactly this condition, so a daemon whose stdout is redirected prints neither header nor
    /// rows; `operations.log` is the only per-event artifact in that mode.
    #[inline]
    pub fn tabular_stdout(&self) -> bool {
        !self.json_stdout && self.stdout_tty
    }

    fn open_file(&mut self) {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .ok();
        #[cfg(unix)]
        if file.is_some() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.log_path, fs::Permissions::from_mode(0o666));
        }
        self.writer = file.map(|f| BufWriter::with_capacity(8192, f));
    }

    /// Rename the current log to `<path>.old` and start a fresh file. Only ever called when the
    /// log is meant to be open, so it always reopens; the disabled case cannot reach it.
    fn rotate(&mut self) {
        self.flush();
        self.writer = None;
        let old_path = format!("{}.old", self.log_path);
        let _ = fs::rename(&self.log_path, &old_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&old_path, fs::Permissions::from_mode(0o666));
        }
        self.open_file();
        self.bytes_written = 0;
    }

    /// Buffer the record; do **not** flush. See [`Self::flush`]. Public so
    /// `benches/microbench.rs` can time the real write path.
    pub fn write_to_log(&mut self, line: &str) {
        if self.writer.is_none() {
            return; // logging switched off, or the file could not be opened
        }
        let line_len = line.len() + 1;
        if self.bytes_written + line_len >= ROTATION_THRESHOLD_BYTES {
            self.rotate();
        }
        if let Some(ref mut w) = self.writer {
            if writeln!(w, "{line}").is_ok() {
                self.bytes_written += line_len;
            }
        }
    }

    #[inline]
    pub fn emit_with<FJ, FT>(&mut self, make_tabular: FT, make_json: FJ)
    where
        FJ: FnOnce() -> String,
        FT: FnOnce() -> String,
    {
        // Both halves are lazy: nothing is allocated or formatted for a sink that will not
        // consume it.
        if self.silent() {
            return;
        }
        // JSON is the log's payload, and stdout's payload under `--json`. With neither
        // interested there is nothing to format; `write_to_log` drops the empty line on the
        // floor because its writer is `None`.
        let json_line = if self.json_stdout || self.writer.is_some() {
            make_json()
        } else {
            String::new()
        };
        if self.json_stdout {
            // `--json` stdout is a machine contract (axcore, a pipe), so it holds regardless
            // of whether fd 1 is a terminal.
            let _ = writeln!(stdout(), "{json_line}");
        } else if self.tabular_stdout() {
            // Tabular rendering costs a `localtime_r` and a String; it exists for a human
            // reading a terminal, so that is the only place it runs.
            let _ = writeln!(stdout(), "{}", make_tabular());
        }
        self.write_to_log(&json_line);
    }

    /// Push buffered records to the log file. The reactor calls this once per
    /// `epoll_wait` batch and before every `exit`, so a burst of events costs one
    /// `write(2)` instead of one per line. A batch that emitted nothing costs
    /// nothing either: an empty `BufWriter::flush` and `File::flush` perform no
    /// syscall, so no dirty flag is needed to keep quiet windows write-free.
    pub fn flush(&mut self) {
        let _ = stdout().flush();
        if let Some(ref mut w) = self.writer {
            let _ = w.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_time_hms_ms() {
        let formatted = format_time_hms_ms(1790266000120);
        assert_eq!(formatted.len(), 12);
        assert_eq!(&formatted[2..3], ":");
        assert_eq!(&formatted[5..6], ":");
        assert_eq!(&formatted[8..9], ".");
    }

    #[test]
    fn test_session_stats() {
        let mut stats = SessionStats::default();
        stats.bg_spawns += 2;
        stats.spawn_rss_kb += 32768;
        stats.bg_deaths += 1;
        stats.spawn_rss_kb -= 16384;
        assert_eq!(stats.bg_spawns, 2);
        assert_eq!(stats.bg_deaths, 1);
        assert_eq!(stats.spawn_rss_kb, 16384);
    }

    #[test]
    fn test_telemetry_sink_and_rotation() {
        let mut tmp_path = std::env::temp_dir();
        tmp_path.push("mini_lmk_rotation_test.log");
        let path_str = tmp_path.to_str().unwrap();
        let old_path_str = format!("{}.old", path_str);
        let _ = fs::remove_file(&tmp_path);
        let _ = fs::remove_file(&old_path_str);

        {
            let mut sink = TelemetrySink::with_stdout(path_str, false, true, true);
            sink.emit_with(
                || "12:00:00.000   FG_SWITCH    com.test                   prev=--".to_string(),
                || r#"{"event":"fg_switch"}"#.to_string(),
            );
            sink.emit_with(
                || "12:00:01.000   KILL         com.test.bg                rss=100MB".to_string(),
                || r#"{"event":"kill"}"#.to_string(),
            );
            sink.flush();
            assert!(sink.bytes_written > 0);

            // Force rotation
            sink.rotate();
            assert_eq!(sink.bytes_written, 0);

            sink.emit_with(
                || "12:00:02.000   SCREEN_OFF   --                         active=10s".to_string(),
                || r#"{"event":"screen_state"}"#.to_string(),
            );
            sink.flush();
        }

        assert!(fs::metadata(&old_path_str).is_ok());
        let old_content = fs::read_to_string(&old_path_str).unwrap();
        assert!(old_content.contains(r#"{"event":"fg_switch"}"#));
        assert!(old_content.contains(r#"{"event":"kill"}"#));

        let new_content = fs::read_to_string(&tmp_path).unwrap();
        assert!(new_content.contains(r#"{"event":"screen_state"}"#));
        assert!(!new_content.contains(r#"{"event":"fg_switch"}"#));

        let _ = fs::remove_file(&tmp_path);
        let _ = fs::remove_file(&old_path_str);
    }

    #[test]
    fn test_escape_json() {
        assert_eq!(escape_json("com.android.settings"), "com.android.settings");
        assert!(matches!(escape_json("com.android.settings"), std::borrow::Cow::Borrowed(_)));

        let bad = "pkg\"with\\escapes\nand\tcontrol";
        let escaped = escape_json(bad);
        assert_eq!(escaped, "pkg\\\"with\\\\escapes\\nand\\tcontrol");
        assert!(matches!(escaped, std::borrow::Cow::Owned(_)));
    }

    #[test]
    fn test_emit_with_output_gates() {
        let mut tmp_path = std::env::temp_dir();
        tmp_path.push("mini_lmk_gates_test.log");
        let path_str = tmp_path.to_str().unwrap();
        let _ = fs::remove_file(&tmp_path);
        // Parent directory does not exist -> open_file() leaves `writer` as None.
        let dead_path = "/no-such-dir-mlmk/operations.log";

        // All eight combinations: `emit_with` reads exactly these three inputs.
        // (json_stdout, stdout_tty, log_openable, columnar expected, json expected)
        let cases = [
            (true, true, true, false, true),     // --json on a terminal
            (true, true, false, false, true),    // --json on a terminal, log unavailable
            (true, false, true, false, true),    // --json into a pipe: the shipped service shape
            (true, false, false, false, true),   // --json into a pipe, log unavailable
            (false, true, true, true, true),     // interactive: columnar row plus file record
            (false, true, false, true, false),   // human turned the log off: table only
            (false, false, true, false, true),   // service.sh: the file record is all that runs
            (false, false, false, false, false), // nowhere to write: both closures skipped
        ];
        for (json, tty, log_ok, want_tabular, want_json) in cases {
            let path = if log_ok { path_str } else { dead_path };
            // `log_openable` stands in for "the log is closed": an unopenable parent directory
            // leaves `writer` as None, the same sink state `log_enabled = false` arrives at.
            let mut sink = TelemetrySink::with_stdout(path, json, true, tty);
            let (mut tabular, mut made_json) = (false, false);
            sink.emit_with(
                || {
                    tabular = true;
                    "rendered-row".to_string()
                },
                || {
                    made_json = true;
                    r#"{"event":"test"}"#.to_string()
                },
            );
            assert_eq!(tabular, want_tabular, "columnar closure (json={json}, tty={tty})");
            assert_eq!(
                made_json, want_json,
                "json closure (json={json}, tty={tty}, log_openable={log_ok})"
            );
            sink.flush();
        }

        let content = fs::read_to_string(&tmp_path).unwrap();
        assert_eq!(
            content.matches(r#"{"event":"test"}"#).count(),
            4,
            "operations.log keeps receiving records in every mode that can open it"
        );
        assert!(!content.contains("rendered-row"), "the two formatters never mix outputs");
        let _ = fs::remove_file(&tmp_path);
    }

    #[test]
    fn test_redirected_stdout_still_logs() {
        let mut tmp_path = std::env::temp_dir();
        tmp_path.push("mini_lmk_silent_test.log");
        let path_str = tmp_path.to_str().unwrap();
        let _ = fs::remove_file(&tmp_path);

        // No --json, no terminal, but the log file IS openable: the record must still be logged.
        let mut sink = TelemetrySink::with_stdout(path_str, false, true, false);
        assert!(!sink.silent(), "an open log file is an output");
        let mut tabular_called = false;
        sink.emit_with(
            || {
                tabular_called = true;
                "never".to_string()
            },
            || r#"{"event":"test"}"#.to_string(),
        );
        assert!(!tabular_called, "columnar rendering needs a terminal");
        sink.flush();
        assert!(fs::read_to_string(&tmp_path)
            .unwrap()
            .contains(r#"{"event":"test"}"#));

        let _ = fs::remove_file(&tmp_path);
    }

    /// `log_enabled = false` in daemon.conf and `--no-log` on the CLI share one mechanism: the
    /// sink closes the file. stdout rendering stays independent, so a run attached to a terminal
    /// keeps its table and a `--json` stream keeps its records.
    #[test]
    fn test_log_disable_closes_file_and_reenable_resumes() {
        let mut tmp_path = std::env::temp_dir();
        tmp_path.push("mini_lmk_disabled_test.log");
        let path_str = tmp_path.to_str().unwrap();
        let _ = fs::remove_file(&tmp_path);

        let mut sink = TelemetrySink::with_stdout(path_str, false, false, true);
        assert!(sink.writer.is_none(), "a disabled log is never opened");
        assert!(!sink.silent(), "a terminal is still an output");
        let mut rendered = false;
        sink.emit_with(
            || {
                rendered = true;
                "row".to_string()
            },
            || r#"{"event":"kill"}"#.to_string(),
        );
        assert!(
            rendered,
            "the terminal table is independent of the log switch"
        );
        assert!(!tmp_path.exists(), "a disabled daemon creates no log file");

        sink.set_log_enabled(true);
        sink.emit_with(
            || "row".to_string(),
            || r#"{"event":"fg_switch"}"#.to_string(),
        );
        sink.set_log_enabled(false); // flushes the buffer on its way out
        assert_eq!(
            fs::read_to_string(&tmp_path).unwrap(),
            "{\"event\":\"fg_switch\"}\n",
            "the record buffered before disabling must survive the close"
        );

        // Dropped while disabled: prove they survive no later reopen, by reopening and flushing.
        sink.emit_with(
            || "row".to_string(),
            || r#"{"event":"respawn"}"#.to_string(),
        );
        sink.set_log_enabled(true);
        sink.emit_with(|| "row".to_string(), || r#"{"event":"late"}"#.to_string());
        sink.set_log_enabled(false); // flushes the buffer on its way out
        assert_eq!(
            fs::read_to_string(&tmp_path).unwrap(),
            "{\"event\":\"fg_switch\"}\n{\"event\":\"late\"}\n",
            "the event emitted while the log was closed must not reappear when it reopens"
        );

        let _ = fs::remove_file(&tmp_path);
    }

    #[test]
    fn test_reopen_rotates_a_log_that_grew_while_disabled() {
        // `bytes_written` cannot move while the writer is closed, so a size cached at open time
        // would let an `operations.log` that grew in the meantime be appended past the cap.
        // Re-enabling must re-read the inode and rotate first.
        let pid = std::process::id();
        let tmp_path = std::env::temp_dir().join(format!("mlmk_test_{pid}_grow.log"));
        let old_path = format!("{}.old", tmp_path.to_string_lossy());
        let _ = fs::remove_file(&tmp_path);
        let _ = fs::remove_file(&old_path);

        let mut sink = TelemetrySink::with_stdout(&tmp_path.to_string_lossy(), false, true, true);
        sink.emit_with(|| "row".to_string(), || r#"{"event":"before"}"#.to_string());
        sink.set_log_enabled(false);
        {
            let mut grower = OpenOptions::new().append(true).open(&tmp_path).unwrap();
            grower.write_all(&[b'x'; 600 * 1024]).unwrap();
        }

        sink.set_log_enabled(true);
        sink.emit_with(|| "row".to_string(), || r#"{"event":"after"}"#.to_string());
        sink.flush();

        let rotated = fs::metadata(&old_path).expect("re-enabling an oversized log must rotate it");
        assert!(
            rotated.len() >= 600 * 1024,
            "the rotated file must carry what arrived while we were not writing: {}",
            rotated.len()
        );
        assert_eq!(
            fs::read_to_string(&tmp_path).unwrap(),
            "{\"event\":\"after\"}\n",
            "the live file starts empty after the rotation"
        );

        // And the other direction: a shrunk or deleted file must not be rotated early.
        sink.set_log_enabled(false);
        fs::write(&tmp_path, b"").unwrap();
        sink.set_log_enabled(true);
        sink.emit_with(|| "row".to_string(), || r#"{"event":"fresh"}"#.to_string());
        sink.flush();
        assert_eq!(
            fs::read_to_string(&tmp_path).unwrap(),
            "{\"event\":\"fresh\"}\n",
            "a truncated log must be appended to, not rotated away"
        );
        assert_eq!(
            fs::metadata(&old_path).unwrap().len(),
            rotated.len(),
            "no second rotation"
        );

        let _ = fs::remove_file(&tmp_path);
        let _ = fs::remove_file(&old_path);
    }
}
