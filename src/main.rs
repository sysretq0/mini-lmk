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

mod config;
mod daemon;
mod events;
mod hasher;
mod kill;
mod parser;
mod probe;
mod procfs;
mod spawn;
mod telemetry;

use std::io::{stderr, stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Cleared by the signal handler; the run loop polls it between epoll waits.
pub(crate) static RUNNING: AtomicBool = AtomicBool::new(true);

pub(crate) extern "C" fn sig_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

pub(crate) const TOKEN_LOGCAT_PIPE: u64 = 1;
pub(crate) const TOKEN_INOTIFY: u64 = 2;

/// Minimum `oom_score_adj` for the root SIGKILL fast path (AOSP `CACHED_APP_MIN_ADJ`).
/// Below this the AMS path is used even as root: `cmd activity kill` updates AMS state
/// properly, so service-state processes are not killed behind its back and not
/// immediately respawned.
pub(crate) const SIGKILL_MIN_ADJ: i32 = 900;

/// One eviction candidate, as the scan produced it and the dispatcher consumes it.
pub(crate) struct Candidate {
    pkg: String,
    live_pids: Vec<u32>,
    total_rss_kb: u64,
    idle_sec: u64,
    lru_pos: usize,
}

/// Report a fatal error and leave the process. Free function because the few startup failures
/// that happen *before* a telemetry sink exists have nothing to flush; everything after the sink
/// exists goes through `DaemonState::fatal`.
fn fatal_exit(msg: std::fmt::Arguments) -> ! {
    let _ = writeln!(stderr(), "[FATAL] {msg}");
    std::process::exit(1)
}

fn print_help() {
    let _ = writeln!(stdout(),
        r#"mini-lmk - Rootless event-driven memory manager for Android 7.0+ (API 24+)
Usage: mini-lmk <MODE> [OPTIONS]

Modes:
  --observe        Run in observation mode (simulate kills, emit telemetry)
  --act            Execute real kills via `cmd activity kill --user all`
                   (as root: SIGKILL for oom_score_adj >= 900, AMS below)

Options:
  --json           Emit raw NDJSON to stdout instead of tabular columnar format
  --no-log         Never write operations.log (overrides log_enabled in daemon.conf)
  -h, --help       Print this help message"#
    );
}

fn main() {
    let mut mode = None;
    let mut json_stdout = false;
    let mut no_log = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--act" => mode = Some(true),
            "--observe" => mode = Some(false),
            "--json" => json_stdout = true,
            "--no-log" => no_log = true,
            "-h" | "--help" => return print_help(),
            other => { let _ = writeln!(stderr(), "[WARN] Unknown argument: {}", other); }
        }
    }

    let Some(act_mode) = mode else {
        let _ = writeln!(stderr(), "[ERROR] Operating mode must be specified: use --observe or --act\n");
        return print_help();
    };

    // A `--json` stdout is a machine contract: the banner would be the one non-NDJSON line an
    // axcore pipeline has to skip. A redirected stdout deserves the same treatment, because every
    // consumer of it (`scripts/benchmark.sh`, an operator piping the supervisor into a file) wants
    // the progress banners and not the event table, so the human banner follows the same `isatty(1)`
    // rule as the table it introduces.
    if !json_stdout && telemetry::stdout_is_tty() {
        let _ = writeln!(stdout(), "=== mini-lmk daemon ===");
    }
    daemon::DaemonState::new(act_mode, json_stdout, no_log).run();
}

