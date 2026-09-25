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

use std::io::{BufWriter, Write};

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

pub struct TelemetrySink {
    writer: Option<BufWriter<File>>,
    bytes_written: usize,
    log_path: String,
    pub json_stdout: bool,
}

impl TelemetrySink {
    pub fn new(log_path: &str, json_stdout: bool) -> Self {
        let mut current_bytes = 0;
        if let Ok(meta) = fs::metadata(log_path) {
            current_bytes = meta.len() as usize;
        }

        let mut sink = Self {
            writer: None,
            bytes_written: current_bytes,
            log_path: log_path.to_string(),
            json_stdout,
        };

        if sink.bytes_written >= ROTATION_THRESHOLD_BYTES {
            sink.rotate();
        } else {
            sink.open_file();
        }

        sink
    }

    fn open_file(&mut self) {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
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

    pub fn rotate(&mut self) {
        self.flush();
        self.writer = None;
        let old_path = format!("{}.old", &self.log_path);
        let _ = fs::rename(&self.log_path, &old_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&old_path, fs::Permissions::from_mode(0o666));
        }
        self.open_file();
        self.bytes_written = 0;
    }

    pub fn write_to_log(&mut self, line: &str) {
        let line_len = line.len() + 1;
        if self.bytes_written + line_len >= ROTATION_THRESHOLD_BYTES {
            self.rotate();
        }
        if let Some(ref mut w) = self.writer {
            if writeln!(w, "{}", line).is_ok() {
                self.bytes_written += line_len;
                let _ = w.flush();
            }
        }
    }

    #[inline]
    pub fn emit_with<F>(&mut self, make_tabular: F, json_line: &str)
    where
        F: FnOnce() -> String,
    {
        if self.json_stdout {
            println!("{}", json_line);
        } else {
            let tabular = make_tabular();
            println!("{}", tabular);
        }
        self.write_to_log(json_line);
    }

    pub fn flush(&mut self) {
        let _ = std::io::stdout().flush();
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
            let mut sink = TelemetrySink::new(path_str, false);
            sink.emit_with(|| "12:00:00.000   FG_SWITCH    com.test                   prev=--".to_string(), r#"{"event":"fg_switch"}"#);
            sink.emit_with(|| "12:00:01.000   KILL         com.test.bg                rss=100MB".to_string(), r#"{"event":"kill"}"#);
            sink.flush();
            assert!(sink.bytes_written > 0);

            // Force rotation
            sink.rotate();
            assert_eq!(sink.bytes_written, 0);

            sink.emit_with(|| "12:00:02.000   SCREEN_OFF   --                         active=10s".to_string(), r#"{"event":"screen_state"}"#);
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
    fn test_emit_with_lazy_evaluation() {
        let mut tmp_path = std::env::temp_dir();
        tmp_path.push("mini_lmk_lazy_test.log");
        let path_str = tmp_path.to_str().unwrap();
        let _ = fs::remove_file(&tmp_path);

        // When json_stdout is true, tabular closure is NEVER evaluated (0 allocations)
        {
            let mut sink = TelemetrySink::new(path_str, true);
            let mut called = false;
            sink.emit_with(
                || {
                    called = true;
                    "never_evaluated".to_string()
                },
                r#"{"event":"test"}"#,
            );
            assert!(!called, "Tabular closure should NOT be evaluated in JSON mode");
        }

        // When json_stdout is false, tabular closure IS evaluated
        {
            let mut sink = TelemetrySink::new(path_str, false);
            let mut called = false;
            sink.emit_with(
                || {
                    called = true;
                    "evaluated".to_string()
                },
                r#"{"event":"test"}"#,
            );
            assert!(called, "Tabular closure should be evaluated in tabular mode");
        }

        let _ = fs::remove_file(&tmp_path);
    }
}
