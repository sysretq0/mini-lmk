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

//! One-shot probes of the Android environment: screen power state, the HOME
//! resolver fallback, and a health check for the freshly spawned logcat child.

use std::process::{Child, Command};

/// Parse screen power state from `dumpsys power` output (API 24-27 fallback).
/// Scans line-by-line to evaluate authoritative current state first and prevent
/// false positives from historical logs or wake lock tables.
pub(crate) fn parse_dumpsys_power_screen(stdout: &str) -> Option<bool> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("mHoldingDisplaySuspendBlocker=") {
            if trimmed.contains("true") {
                return Some(true);
            } else if trimmed.contains("false") {
                return Some(false);
            }
        }
        if trimmed.starts_with("Display Power: state=") {
            if trimmed.contains("ON") {
                return Some(true);
            } else if trimmed.contains("OFF") || trimmed.contains("DOZE") {
                return Some(false);
            }
        }
    }
    None
}

/// Detect initial screen state at daemon startup.
/// Fast path: `cmd deviceidle get screen` (API 28+).
/// Fallback: `dumpsys power` (API 24-27). Defaults cleanly to `true` (screen on).
#[inline(always)]
pub(crate) fn detect_initial_screen_on() -> bool {
    Command::new("/system/bin/cmd")
        .args(["deviceidle", "get", "screen"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            if o.stdout.starts_with(b"true") {
                Some(true)
            } else if o.stdout.starts_with(b"false") {
                Some(false)
            } else {
                None
            }
        })
        .or_else(|| {
            let out = Command::new("/system/bin/dumpsys").arg("power").output().ok()?;
            parse_dumpsys_power_screen(&String::from_utf8_lossy(&out.stdout))
        })
        .unwrap_or(true)
}

/// Parse package name from `cmd package resolve-activity` output (API 24-28 fallback).
pub(crate) fn parse_resolve_activity_pkg(stdout: &str) -> Option<&str> {
    stdout.lines().find_map(|l| {
        l.split_whitespace()
            .find_map(|w| w.split_once('/'))
            .map(|(pkg, _)| pkg.trim())
            .filter(|pkg| !pkg.is_empty() && pkg.contains('.') && !pkg.starts_with(['-', '{']))
    })
}

/// Non-allocating, sub-millisecond health probe for the spawned logcat stream.
/// Validates child process survival and pipe integrity using non-blocking syscalls.
pub(crate) fn probe_logcat_stream(child: &mut Child, pipe_fd: i32) -> Result<(), &'static str> {
    // 1. Immediate check (no sleeping, no retry): Has the child already exited (e.g. exec failure, missing binary, or SELinux denial)?
    match child.try_wait() {
        Ok(Some(_)) => {
            return Err("Child logcat process died immediately after spawn (SELinux denial, missing binary, or invalid arguments)");
        }
        Err(_) => return Err("error checking logcat child status"),
        Ok(None) => {}
    }

    if pipe_fd < 0 {
        return Err("Invalid logcat stdout pipe file descriptor");
    }

    // 2. Zero-timeout poll on pipe_fd to detect immediate POLLHUP, POLLERR, or POLLNVAL
    let mut pfd = libc::pollfd {
        fd: pipe_fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    if ret > 0 {
        if pfd.revents & libc::POLLNVAL != 0 {
            return Err("Invalid logcat stdout pipe file descriptor (POLLNVAL)");
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return Err("Immediate POLLHUP/POLLERR detected on logcat stdout pipe");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::IntoRawFd;
    use std::process::Stdio;

    struct ChildGuard {
        child: Child,
        fd: i32,
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if self.fd >= 0 {
                unsafe { libc::close(self.fd) };
            }
        }
    }

    #[test]
    fn test_probe_logcat_stream_healthy() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let stdout = child.stdout.take().expect("stdout");
        let raw_fd = stdout.into_raw_fd();
        let mut guard = ChildGuard { child, fd: raw_fd };

        let res = probe_logcat_stream(&mut guard.child, guard.fd);
        assert!(res.is_ok());
    }

    #[test]
    fn test_probe_logcat_stream_dead_child() {
        let mut child = Command::new("true")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn true");
        let stdout = child.stdout.take().expect("stdout");
        let raw_fd = stdout.into_raw_fd();
        let mut guard = ChildGuard { child, fd: raw_fd };

        // Wait for child to exit
        let _ = guard.child.wait();

        let res = probe_logcat_stream(&mut guard.child, guard.fd);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("died immediately"));
    }

    #[test]
    fn test_probe_logcat_stream_invalid_fd() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let stdout = child.stdout.take().expect("stdout");
        let raw_fd = stdout.into_raw_fd();
        let mut guard = ChildGuard { child, fd: -1 };

        // Close raw_fd explicitly to simulate invalid fd
        unsafe { libc::close(raw_fd) };

        let res = probe_logcat_stream(&mut guard.child, raw_fd);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("Invalid"));
    }

    #[test]
    fn test_parse_resolve_activity_pkg() {
        // Multi-line output with match line
        let output1 = "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\ncom.google.android.apps.nexuslauncher/.NexusLauncherActivity";
        assert_eq!(parse_resolve_activity_pkg(output1), Some("com.google.android.apps.nexuslauncher"));

        // Single-line component
        let output2 = "com.android.launcher3/.Launcher";
        assert_eq!(parse_resolve_activity_pkg(output2), Some("com.android.launcher3"));

        // Component with leading flags / match
        let output3 = "match=0x200000 com.sec.android.app.launcher/com.sec.android.app.launcher.Launcher";
        assert_eq!(parse_resolve_activity_pkg(output3), Some("com.sec.android.app.launcher"));

        // No match / error
        assert_eq!(parse_resolve_activity_pkg("No activity found"), None);
        assert_eq!(parse_resolve_activity_pkg(""), None);
        assert_eq!(parse_resolve_activity_pkg("Error: could not find activity"), None);
    }

    #[test]
    fn test_parse_dumpsys_power_screen() {
        // API 24-27 suspend blocker checks
        assert_eq!(parse_dumpsys_power_screen("mHoldingDisplaySuspendBlocker=true\nmHoldingWakeLockSuspendBlocker=false"), Some(true));
        assert_eq!(parse_dumpsys_power_screen("mHoldingDisplaySuspendBlocker=false\nmHoldingWakeLockSuspendBlocker=false"), Some(false));

        // API 28+ display power state checks
        assert_eq!(parse_dumpsys_power_screen("Display Power: state=ON"), Some(true));
        assert_eq!(parse_dumpsys_power_screen("Display Power: state=OFF"), Some(false));
        assert_eq!(parse_dumpsys_power_screen("Display Power: state=DOZE"), Some(false));

        // Historical log edge cases: current state at top takes precedence over historical log entries
        let history_off = "  mHoldingDisplaySuspendBlocker=false\nHistorical Suspend Blockers:\n  mHoldingDisplaySuspendBlocker=true";
        assert_eq!(parse_dumpsys_power_screen(history_off), Some(false));

        let history_on = "  mHoldingDisplaySuspendBlocker=true\nHistorical Suspend Blockers:\n  mHoldingDisplaySuspendBlocker=false";
        assert_eq!(parse_dumpsys_power_screen(history_on), Some(true));

        // Unrelated or malformed output
        assert_eq!(parse_dumpsys_power_screen("PowerManagerService is dead"), None);
        assert_eq!(parse_dumpsys_power_screen(""), None);
    }
}
