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

use std::io::Write;


/// Unified proc path formatter. Writes `/proc/<pid>/<suffix>\0` into a stack buffer.
/// `suffix` must NOT include a leading slash (e.g. `"statm"`, `"cmdline"`).
#[inline(always)]
pub fn format_proc_path(buf: &mut [u8; 32], pid: u32, suffix: &str) {
    *buf = [0u8; 32];
    let _ = write!(&mut buf[..31], "/proc/{}/{}", pid, suffix);
}

pub fn read_statm_rss_kb(pid: u32, page_size_kb: u64) -> u64 {
    let mut path_buf = [0u8; 32];
    format_proc_path(&mut path_buf, pid, "statm");

    let fd = unsafe {
        libc::open(path_buf.as_ptr() as *const libc::c_char, libc::O_RDONLY | libc::O_CLOEXEC)
    };
    if fd < 0 {
        return 0;
    }

    let mut buf = [0u8; 128];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    unsafe { libc::close(fd); }

    if n <= 0 {
        return 0;
    }

    std::str::from_utf8(&buf[..n as usize])
        .ok()
        .and_then(|s| s.split_ascii_whitespace().nth(1))
        .and_then(|s| s.parse::<u64>().ok())
        .map(|pages| pages * page_size_kb)
        .unwrap_or(0)
}

/// Reads /proc/<pid>/oom_score_adj.
pub fn read_oom_score_adj(pid: u32) -> Option<i32> {
    let mut path_buf = [0u8; 32];
    format_proc_path(&mut path_buf, pid, "oom_score_adj");

    let fd = unsafe {
        libc::open(path_buf.as_ptr() as *const libc::c_char, libc::O_RDONLY | libc::O_CLOEXEC)
    };
    if fd < 0 {
        return None;
    }

    let mut buf = [0u8; 16];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    unsafe { libc::close(fd); }

    if n <= 0 {
        return None;
    }

    std::str::from_utf8(&buf[..n as usize])
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Reads MemTotal and MemAvailable from /proc/meminfo in kilobytes.
pub fn read_meminfo_kb() -> (u64, u64) {
    let fd = unsafe {
        libc::open(c"/proc/meminfo".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC)
    };
    if fd < 0 {
        return (0, 0);
    }

    let mut buf = [0u8; 1024];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    unsafe { libc::close(fd); }

    if n <= 0 {
        return (0, 0);
    }

    let mut total_kb = 0u64;
    let mut avail_kb = 0u64;

    if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                total_kb = rest.split_ascii_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                avail_kb = rest.split_ascii_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            if total_kb > 0 && avail_kb > 0 {
                break;
            }
        }
    }
    (total_kb, avail_kb)
}

/// Parses `some avg10` out of `/proc/pressure/memory` contents.
/// Format: `some avg10=0.00 avg60=0.00 avg300=0.00 total=0` (then a `full` line).
pub fn parse_psi_some_avg10(contents: &str) -> Option<f32> {
    contents
        .lines()
        .find(|l| l.starts_with("some "))
        .and_then(|l| l.split_ascii_whitespace().find(|w| w.starts_with("avg10=")))
        .and_then(|w| w.strip_prefix("avg10="))
        .and_then(|v| v.parse().ok())
}

/// Reads `some avg10` from /proc/pressure/memory (kernel 4.20+): the share of time some
/// task stalled waiting for memory over the last 10 seconds. `None` = PSI absent
/// (old kernel, `CONFIG_PSI=n`, or a denied open).
pub fn read_memory_psi_some_avg10() -> Option<f32> {
    let fd = unsafe {
        libc::open(c"/proc/pressure/memory".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC)
    };
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 512];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    unsafe { libc::close(fd); }
    if n <= 0 {
        return None;
    }
    parse_psi_some_avg10(std::str::from_utf8(&buf[..n as usize]).ok()?)
}

/// Memory pressure probe, deciding the emergency `T_idle = 10s` grace window.
///
/// PSI (`/proc/pressure/memory`) when the kernel provides it: `some avg10` is the earliest
/// signal of thrashing — MemAvailable can read healthy while direct reclaim is stalling
/// tasks. Falls back to the MemAvailable watermark where PSI is absent. Either way
/// `mem_critical_percent` is the threshold: percent of stall time (PSI) or percent of
/// MemTotal (fallback). `0` disables the check on both probes.
pub fn check_mem_critical(mem_critical_percent: u64) -> bool {
    // Without this, 0 means "always emergency" under PSI (`avg10 >= 0.0` is always
    // true) but "disabled" under the fallback — keep the two probes agreeing.
    if mem_critical_percent == 0 {
        return false;
    }
    if let Some(psi) = read_memory_psi_some_avg10() {
        return psi >= mem_critical_percent as f32;
    }
    let (mem_total_kb, mem_avail_kb) = read_meminfo_kb();
    if mem_total_kb > 0 {
        mem_avail_kb * 100 < mem_total_kb * mem_critical_percent
    } else {
        false
    }
}

/// Reads MemTotal from /proc/meminfo and returns physical RAM in megabytes.
/// Falls back to 4096 MB if unreadable.
pub fn read_total_ram_mb() -> u64 {
    let (total_kb, _) = read_meminfo_kb();
    if total_kb > 0 {
        total_kb / 1024
    } else {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_meminfo_kb() {
        let (total_kb, _) = read_meminfo_kb();
        assert!(total_kb > 0);
    }

    #[test]
    fn test_read_total_ram_mb() {
        let ram_mb = read_total_ram_mb();
        assert!(ram_mb > 0);
    }

    #[test]
    fn test_format_proc_path() {
        let mut buf = [0u8; 32];
        format_proc_path(&mut buf, 12345, "statm");
        let path = std::ffi::CStr::from_bytes_until_nul(&buf).unwrap();
        assert_eq!(path.to_str().unwrap(), "/proc/12345/statm");

        let mut buf2 = [0u8; 32];
        format_proc_path(&mut buf2, 0, "cmdline");
        let path2 = std::ffi::CStr::from_bytes_until_nul(&buf2).unwrap();
        assert_eq!(path2.to_str().unwrap(), "/proc/0/cmdline");

        // Verify dirty buffer and overflow resilience
        let mut dirty = [0xFFu8; 32];
        format_proc_path(&mut dirty, 99999, "suffix_that_is_far_too_long_for_this_small_buffer");
        assert_eq!(dirty[31], 0);
        let path3 = std::ffi::CStr::from_bytes_until_nul(&dirty).unwrap();
        assert!(path3.to_str().unwrap().starts_with("/proc/99999/"));
    }

    #[test]
    fn test_read_oom_score_adj() {
        let pid = std::process::id();
        let adj = read_oom_score_adj(pid);
        assert!(adj.is_some());
    }

    #[test]
    fn test_parse_psi_some_avg10() {
        let sample = "some avg10=12.34 avg60=0.55 avg300=0.10 total=123456789\n\
                      full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(parse_psi_some_avg10(sample), Some(12.34));
        // `full`-only content is not the signal we read.
        assert_eq!(parse_psi_some_avg10("full avg10=9.99 avg60=0.00 total=1\n"), None);
        assert_eq!(parse_psi_some_avg10(""), None);
        assert_eq!(parse_psi_some_avg10("some avg10=junk\n"), None);
        // 0 disables the escalation on both probes, before any file is read.
        assert!(!check_mem_critical(0));
    }
}
