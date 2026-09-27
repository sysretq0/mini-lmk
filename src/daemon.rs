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

//! [`DaemonState`]: construction, configuration watching, and the epoll run
//! loop that consumes the logcat stream.

use crate::config::{ConfigPaths, RuntimeConfig};

use crate::hasher::{FastMap, FastSet};
use crate::parser::{parse_logcat_line, LogcatEvent};
use crate::probe::{detect_initial_screen_on, parse_resolve_activity_pkg, probe_logcat_stream};
use crate::procfs::read_total_ram_mb;
use crate::telemetry::{tabular_row, SessionStats, TelemetrySink};
use crate::{fatal_exit, sig_handler, spawn, RUNNING, TOKEN_INOTIFY, TOKEN_LOGCAT_PIPE};
use std::collections::VecDeque;
use std::ffi::CString;
use std::fs;
use std::io::{stderr, stdout, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::IntoRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;


pub(crate) struct DaemonState {
    pub(crate) config: RuntimeConfig,
    pub(crate) json_stdout: bool,
    /// `--no-log`: an explicit CLI request outranks `log_enabled` in daemon.conf, so an inotify
    /// reload cannot silently re-open a log the operator closed.
    pub(crate) no_log_cli: bool,
    /// False until the applied configuration has been recorded. A daemon whose stdout goes to
    /// `/dev/null` (`service.sh`) leaves `operations.log` as its only artifact and the
    /// `[CONFIG] Active` line is printed to that stdout, so the first load is announced even when
    /// it equals the defaults; later loads announce only on an actual change.
    pub(crate) config_announced: bool,
    /// The first-load `config_reload` record, held as `(config, effective log switch, epoch ms)`
    /// until the terminal table exists. Emitted where it was discovered — inside `new()`, long
    /// before `run()` prints the header — it put one data row above the header it belongs to, among
    /// the startup banners. Drained by [`Self::emit_startup_record`].
    pub(crate) startup_record: Option<(RuntimeConfig, bool, u64)>,

    // Canonical base package -> last foreground/activity epoch-ms anchor (UID >= 10000 only).
    pub(crate) alive_apps: FastMap<String, u64>,
    pub(crate) pid_to_pkg: FastMap<u32, String>,
    pub(crate) pkg_to_pids: FastMap<String, FastSet<u32>>,
    pub(crate) pkg_to_uid: FastMap<String, u32>,
    pub(crate) recent_deaths: FastMap<String, u64>,
    pub(crate) user_exclusions: FastSet<String>,
    pub(crate) games: FastSet<String>,
    pub(crate) dynamic_exclusions: FastSet<String>,
    pub(crate) fg_lru: VecDeque<String>,

    pub(crate) telemetry: TelemetrySink,
    pub(crate) logcat_child: Option<Child>,
    pub(crate) current_fg: Option<String>,
    pub(crate) screen_on_start: Option<u64>,
    pub(crate) screen_off_start: Option<u64>,
    pub(crate) game_session_start: Option<u64>,
    pub(crate) session_start: u64,
    pub(crate) last_event_epoch: u64,

    pub(crate) session_stats: SessionStats,
    pub(crate) page_size_kb: u64,

    pub(crate) epoll_fd: i32,
    pub(crate) logcat_fd: i32,
    pub(crate) inotify_fd: i32,
    pub(crate) game_intrusion_count: u32,

    pub(crate) screen_on: bool,
    pub(crate) is_gaming: bool,
    pub(crate) act_mode: bool,
    /// Effective uid 0: direct `libc::kill(pid, SIGKILL)` is available as the dispatch
    /// method for fully cached candidates (see crate::SIGKILL_MIN_ADJ). Shell (uid 2000)
    /// can never signal app processes, so everything goes through AMS there.
    pub(crate) is_root: bool,
    /// The kill dispatcher: which launcher was resolved at startup, its cached `/dev/null`
    /// descriptor and its argument staging. See `spawn`; roadmap decisions D1-D5.
    pub(crate) spawner: spawn::Spawner,
}

impl DaemonState {
    pub(crate) fn new(act_mode: bool, json_stdout: bool, no_log_cli: bool) -> Self {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = sig_handler as *const () as usize;
            sa.sa_flags = 0; // Explicitly NO SA_RESTART: epoll_wait returns EINTR immediately on signal
            libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());

            let mut sa_ign: libc::sigaction = std::mem::zeroed();
            sa_ign.sa_sigaction = libc::SIG_IGN;
            libc::sigaction(libc::SIGPIPE, &sa_ign, std::ptr::null_mut());
        }

        let paths = ConfigPaths::get();
        paths.ensure_default_files(json_stdout);

        let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll_fd < 0 {
            fatal_exit(format_args!("epoll_create1 failed: {}", std::io::Error::last_os_error()));
        }

        let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify_fd < 0 {
            fatal_exit(format_args!("inotify_init1 failed: {}", std::io::Error::last_os_error()));
        }

        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: TOKEN_INOTIFY,
        };
        if unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, inotify_fd, &mut ev) } < 0 {
            fatal_exit(format_args!("epoll_ctl inotify failed: {}", std::io::Error::last_os_error()));
        }

        // Read daemon.conf once before the log is opened, so `log_enabled = false` means the file
        // is never created at all rather than created and closed one record later. The
        // authoritative load is reload_configs(), which also emits the config_reload record.
        let mut initial_cfg = RuntimeConfig::default();
        initial_cfg.load_from_file(&paths.config_file, true);
        let telemetry = TelemetrySink::new(
            &paths.operations_log,
            json_stdout,
            initial_cfg.log_enabled && !no_log_cli,
        );

        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size_kb = if page_size > 0 { (page_size as u64) / 1024 } else { 4 };

        let now = Self::get_epoch_ms();
        let screen_on = detect_initial_screen_on();
        let screen_on_start = if screen_on { Some(now) } else { None };
        let screen_off_start = if !screen_on { Some(now) } else { None };
        if !json_stdout {
            let _ = writeln!(stdout(), "[DAEMON] Initial display state: screen_on={}", screen_on);
        }

        let total_ram_mb = read_total_ram_mb();
        let detected_default_kills = if total_ram_mb <= 4608 {
            4 // <= 4GB RAM devices (accounting for reserved kernel RAM)
        } else if total_ram_mb <= 8704 {
            2 // 6GB - 8GB RAM devices (up to 8.5GB accounting for carveouts)
        } else {
            1 // > 8.5GB RAM devices (12GB+ configurations)
        };
        if !json_stdout {
            let _ = writeln!(stdout(),
                "[DAEMON] Hardware profile: total_ram={}MB (default max_kills={})",
                total_ram_mb, detected_default_kills
            );
        }

        let config = RuntimeConfig {
            max_kills_per_pass: detected_default_kills,
            ..RuntimeConfig::default()
        };

        let mut daemon = Self {
            config,
            epoll_fd,
            logcat_child: None,
            logcat_fd: -1,
            inotify_fd,
            user_exclusions: FastSet::default(),
            games: FastSet::default(),
            dynamic_exclusions: FastSet::default(),
            alive_apps: FastMap::default(),
            pid_to_pkg: FastMap::default(),
            pkg_to_pids: FastMap::default(),
            pkg_to_uid: FastMap::default(),
            recent_deaths: FastMap::default(),
            fg_lru: VecDeque::with_capacity(16),
            current_fg: None,
            screen_on,
            screen_on_start,
            screen_off_start,
            is_gaming: false,
            game_session_start: None,
            game_intrusion_count: 0,
            act_mode,
            is_root: unsafe { libc::geteuid() } == 0,
            telemetry,
            json_stdout,
            no_log_cli,
            config_announced: false,
            startup_record: None,
            // D3: the backend is an override surface, so a device we cannot reflash can still be
            // measured against v1.4.0's own dispatcher. Unset and unrecognised both mean `fork`,
            // never `spawn`.
            spawner: spawn::Spawner::new(spawn::Backend::parse(
                std::env::var("MINI_LMK_SPAWN").ok().as_deref(),
            )),
            session_start: now,
            // Seed the classifier with the bootstrap read instead of a sentinel, so the
            // unsynchronized-RTC -> NTP step is detected on the *first* event. The only
            // value that legitimately suppresses detection is 0 (clock unavailable).
            last_event_epoch: now,
            session_stats: SessionStats::default(),
            page_size_kb,
        };

        daemon.ensure_inotify_watch();
        daemon.reload_configs();
        daemon.detect_system_components();
        daemon.seed_initial_state();
        daemon.spawn_logcat_stream();

        daemon
    }

    /// Log the pending records, report the failure, and leave. `std::process::exit` skips
    /// `Drop`, and with stdout on `/dev/null` (`service.sh`) `operations.log` is the only
    /// post-mortem artifact there is, so flushing cannot be left to each call site to remember:
    /// termination goes through here or it does not happen at all.
    pub(crate) fn fatal(&mut self, msg: std::fmt::Arguments) -> ! {
        // A daemon that dies before `run()` starts still owes the on-disk baseline: this path is how
        // a module's supervisor learns why the service is crash-looping.
        self.emit_startup_record();
        self.telemetry.flush();
        fatal_exit(msg)
    }

    #[inline(always)]
    pub(crate) fn is_excluded(&self, pkg: &str) -> bool {
        self.user_exclusions.contains(pkg)
            || self.dynamic_exclusions.contains(pkg)
            || self.current_fg.as_deref() == Some(pkg)
    }

    pub(crate) fn seed_initial_state(&mut self) {
        if !self.json_stdout {
            let _ = writeln!(stdout(), "[DAEMON] Performing cold-start discovery...");
        }
        let entries = match fs::read_dir("/proc") {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let pid: u32 = match path.file_name().and_then(|n| n.to_str()).and_then(|s| s.parse().ok()) {
                Some(id) => id,
                None => continue,
            };

            let uid = match entry.metadata() {
                Ok(m) => m.uid(),
                Err(_) => continue,
            };

            let bytes = match fs::read(path.join("cmdline")) {
                Ok(b) if !b.is_empty() => b,
                _ => continue,
            };

            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            if let Ok(raw_proc) = std::str::from_utf8(&bytes[..end]) {
                let raw_proc = raw_proc.trim();
                if raw_proc.contains('.') && !raw_proc.starts_with('/') && !raw_proc.starts_with('-') {
                    let pkg = raw_proc.split(':').next().unwrap_or(raw_proc).trim().to_string();
                    if !pkg.starts_with('-') {
                        self.pid_to_pkg.insert(pid, pkg.clone());
                        self.pkg_to_pids.entry(pkg.clone()).or_default().insert(pid);
                        self.pkg_to_uid.insert(pkg, uid);
                        // alive_apps is deliberately NOT populated here.
                        // Cold-start processes must not be artificially aged into eviction candidates.
                    }
                }
            }
        }
        if !self.json_stdout {
            let _ = writeln!(stdout(),
                "[DAEMON] Indexed {} active PIDs across {} packages.",
                self.pid_to_pkg.len(),
                self.pkg_to_pids.len()
            );
        }
    }

    pub(crate) fn ensure_inotify_watch(&mut self) {
        if self.inotify_fd >= 0 {
            let paths = ConfigPaths::get();
            let _ = fs::create_dir_all(&paths.config_dir);
            if let Ok(dir_c) = CString::new(paths.config_dir.as_str()) {
                let mask = libc::IN_CLOSE_WRITE
                    | libc::IN_MOVED_TO
                    | libc::IN_CREATE
                    | libc::IN_DELETE
                    | libc::IN_DELETE_SELF
                    | libc::IN_MOVE_SELF;
                unsafe { libc::inotify_add_watch(self.inotify_fd, dir_c.as_ptr(), mask) };
            }
        }
    }

    fn load_file_lines(path: &str, set: &mut FastSet<String>, json_stdout: bool) {
        set.clear();
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    set.insert(trimmed.to_string());
                }
            }
            if !json_stdout {
                let _ = writeln!(stdout(), "[CONFIG] Loaded {} entries from {}", set.len(), path);
            }
        }
    }

    pub(crate) fn reload_configs(&mut self) {
        self.reload_configs_from(ConfigPaths::get());
    }

    /// The reload against an explicit path set. `ConfigPaths::get()` resolves `$MODPATH` once per
    /// process and otherwise points at `/data/local/tmp/mlmk`, so a test that went through it would
    /// be reading a `daemon.conf` it never wrote — and the only way to show that the file is applied
    /// at all is to point the reload at a file the test did write.
    pub(crate) fn reload_configs_from(&mut self, paths: &ConfigPaths) {
        let prev_cfg = self.config;
        let first_load = !self.config_announced;
        self.config.load_from_file(&paths.config_file, self.json_stdout);
        Self::load_file_lines(&paths.exclude_file, &mut self.user_exclusions, self.json_stdout);
        Self::load_file_lines(&paths.games_file, &mut self.games, self.json_stdout);

        if self.config != prev_cfg || first_load {
            self.config_announced = true;
            // The switch is applied around the record rather than after it, so the config_reload
            // line that announces a change is always written: a re-enable opens the file first so
            // its own record lands, a disable closes it afterwards so its record is the last line.
            let want_log = self.config.log_enabled && !self.no_log_cli;
            // `want_log`, not `self.config.log_enabled`: with `--no-log` the config still says
            // true while nothing is being written, and a record that claims otherwise is worse
            // than no record.
            if want_log {
                self.telemetry.set_log_enabled(true);
            }
            let now_epoch = Self::get_epoch_ms();
            if first_load {
                // Held for `run()` (see `startup_record`), which also covers the terminal with
                // `--no-log`: that record is the only rendering of the *effective* switch, because
                // the `[CONFIG] Active` line reports the file's `log_enabled` value.
                self.startup_record = Some((self.config, want_log, now_epoch));
            } else {
                // Each rendering stays behind its own closure, so the surface nobody is reading is
                // never built: the tabular row costs a `localtime_r` and a String on top of the JSON.
                let cfg = self.config;
                self.telemetry.emit_with(
                    || Self::config_reload_tabular(&cfg, want_log, now_epoch),
                    || Self::config_reload_json(&cfg, want_log, now_epoch),
                );
            }

            if !want_log {
                // Flushed on its way out, so nothing buffered is lost by the close.
                self.telemetry.set_log_enabled(false);
            } else {
                // The startup record is the baseline a post-mortem reads first, so it must not sit
                // in the 8 KB buffer waiting for an event batch a quiet device never produces:
                // an unclean stop (SIGKILL, a module upgrade) would lose it with the daemon.
                self.telemetry.flush();
            }
        }
    }

    /// Render the held startup `config_reload` record. Called from `run()` once the terminal table
    /// header exists, and from [`Self::fatal`] so a daemon that dies during initialisation still
    /// leaves the baseline behind. Flushed at once rather than waiting for the next event batch: a
    /// quiet device may never produce one, and `SIGKILL` — how a module upgrade stops the daemon —
    /// discards whatever is still in the buffer.
    pub(crate) fn emit_startup_record(&mut self) {
        let Some((cfg, want_log, ts)) = self.startup_record.take() else {
            return;
        };
        self.telemetry.emit_with(
            || Self::config_reload_tabular(&cfg, want_log, ts),
            || Self::config_reload_json(&cfg, want_log, ts),
        );
        self.telemetry.flush();
    }

    /// Tabular rendering of a `config_reload` record: the row for a human reading a terminal,
    /// built only when there is one. `want_log` is the *effective* switch (config AND not
    /// `--no-log`), deliberately: a daemon started with `--no-log` still has `log_enabled = true`
    /// in its config, and a record claiming the log is on while nothing is written answers the one
    /// question the record exists for incorrectly.
    pub(crate) fn config_reload_tabular(cfg: &RuntimeConfig, want_log: bool, now_epoch: u64) -> String {
        let detail = format!(
            "t_idle={}s  lru_depth={}  mem_crit={}%  fg_lru_max={}  harvest={}  max_kills={}  min_adj={}  log={}",
            cfg.t_idle_sec,
            cfg.lru_protect_depth,
            cfg.mem_critical_percent,
            cfg.fg_lru_max_depth,
            cfg.screen_off_harvest,
            cfg.max_kills_per_pass,
            cfg.min_oom_score_adj,
            want_log
        );
        tabular_row(now_epoch, "CONFIG_RELOAD", "--", &detail)
    }

    /// NDJSON rendering of a `config_reload` record: the payload for `operations.log`, and for
    /// stdout under `--json`. Carries every field of [`RuntimeConfig`] plus the effective log
    /// switch, same `want_log` rule as [`Self::config_reload_tabular`].
    pub(crate) fn config_reload_json(cfg: &RuntimeConfig, want_log: bool, now_epoch: u64) -> String {
        format!(
            r#"{{"ts":{},"event":"config_reload","t_idle_sec":{},"lru_protect_depth":{},"mem_critical_percent":{},"fg_lru_max_depth":{},"screen_off_harvest":{},"max_kills_per_pass":{},"min_oom_score_adj":{},"log_enabled":{}}}"#,
            now_epoch,
            cfg.t_idle_sec,
            cfg.lru_protect_depth,
            cfg.mem_critical_percent,
            cfg.fg_lru_max_depth,
            cfg.screen_off_harvest,
            cfg.max_kills_per_pass,
            cfg.min_oom_score_adj,
            want_log
        )
    }

    pub(crate) fn detect_system_components(&mut self) {
        self.dynamic_exclusions.clear();

        let roles = [
            ("HOME", "android.app.role.HOME"),
            ("DIALER", "android.app.role.DIALER"),
            ("SMS", "android.app.role.SMS"),
        ];

        let mut home_detected = false;

        for (label, role) in roles {
            if let Ok(output) = Command::new("/system/bin/cmd").args(["role", "get-role-holders", role]).output() {
                if output.status.success() {
                    for line in String::from_utf8_lossy(&output.stdout).lines() {
                        let pkg = line.trim();
                        if !pkg.is_empty()
                            && pkg != "null"
                            && !pkg.contains(' ')
                            && pkg.contains('.')
                            && !pkg.starts_with("No holder")
                            && !pkg.starts_with("Error")
                            && !pkg.starts_with('-')
                        {
                            if !self.json_stdout {
                                let _ = writeln!(stdout(), "[SYSTEM] Detected {}: {}", label, pkg);
                            }
                            if role == "android.app.role.HOME" {
                                home_detected = true;
                            }
                            self.dynamic_exclusions.insert(pkg.to_string());
                        }
                    }
                }
            }
        }

        // Fallback for API 24-28 (Android 7.0-9.0): RoleManager does not exist.
        // Fallback to querying default HOME launcher via cmd package resolve-activity.
        if !home_detected {
            let out = Command::new("/system/bin/cmd")
                .args(["package", "resolve-activity", "--brief", "-c", "android.intent.category.HOME"])
                .output()
                .ok()
                .filter(|o| o.status.success());
            if let Some(out) = out {
                let resolve_out = String::from_utf8_lossy(&out.stdout);
                if let Some(pkg) = parse_resolve_activity_pkg(&resolve_out) {
                    if !self.json_stdout {
                        let _ = writeln!(stdout(), "[SYSTEM] Detected HOME (fallback): {}", pkg);
                    }
                    self.dynamic_exclusions.insert(pkg.to_string());
                }
            }
        }

        let settings = [
            ("IME", "default_input_method"),
            ("Live Wallpaper", "wallpaper_service"),
        ];

        for (label, key) in settings {
            if let Ok(output) = Command::new("/system/bin/cmd").args(["settings", "get", "secure", key]).output() {
                if output.status.success() {
                    let settings_out = String::from_utf8_lossy(&output.stdout);
                    let s = settings_out.trim();
                    if !s.is_empty() && s != "null" && !s.starts_with("null") {
                        // Unpeel optional user prefix (e.g. "0:com.pkg/svc" -> "com.pkg/svc")
                        let unpeeled = if let Some((prefix, rest)) = s.split_once(':') {
                            if rest.contains('.') && prefix.bytes().all(|b| b.is_ascii_digit()) { rest } else { s }
                        } else { s };
                        let pkg = unpeeled.split('/').next().unwrap_or(unpeeled).trim();
                        if !pkg.is_empty() && pkg.contains('.') && !pkg.contains(' ') && !pkg.starts_with('-') {
                            if !self.json_stdout {
                                let _ = writeln!(stdout(), "[SYSTEM] Detected {}: {}", label, pkg);
                            }
                            self.dynamic_exclusions.insert(pkg.to_string());
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn spawn_logcat_stream(&mut self) {
        if !self.json_stdout {
            let _ = writeln!(stdout(), "[DAEMON] Spawning unified logcat stream...");
        }
        let mut child = match Command::new("/system/bin/logcat")
            .args([
                "-b", "events",
                "-v", "epoch",
                "-T", "1",
                "-s",
                "wm_resume_activity:V",
                "am_resume_activity:V",
                "am_proc_start:V",
                "am_proc_died:V",
                "screen_toggled:V",
                "device_idle_light_step:V",
            ])
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                self.fatal(format_args!("Failed to spawn logcat: {}", e));
            }
        };

        let logcat_pipe = child.stdout.take().expect("logcat stdout");
        let raw_fd = logcat_pipe.into_raw_fd();

        if let Err(err) = probe_logcat_stream(&mut child, raw_fd) {
            self.fatal(format_args!("Logcat pipeline startup probe failed: {}", err));
        }

        unsafe {
            let flags = libc::fcntl(raw_fd, libc::F_GETFL, 0);
            libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR) as u32,
            u64: TOKEN_LOGCAT_PIPE,
        };
        if unsafe { libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, raw_fd, &mut ev) } < 0 {
            self.fatal(format_args!("epoll_ctl logcat pipe failed: {}", std::io::Error::last_os_error()));
        }

        self.logcat_fd = raw_fd;
        self.logcat_child = Some(child);
        if !self.json_stdout {
            let _ = writeln!(stdout(), "[DAEMON] Logcat stream active on fd={}", self.logcat_fd);
        }
    }

    /// Split the freshly read bytes into complete lines and hand each one to
    /// `on_line`. Keeps `run()` readable and gives the byte-level hygiene rules
    /// (CRLF stripping, `---` header suppression, oversize-line discard) a test.
    fn drain_logcat_lines(buf: &mut Vec<u8>, incoming: &[u8], mut on_line: impl FnMut(&str)) {
        buf.extend_from_slice(incoming);
        let mut last_newline = 0;
        for (idx, &b) in buf.iter().enumerate() {
            if b == b'\n' {
                let mut line_bytes = &buf[last_newline..idx];
                if let Some(&b'\r') = line_bytes.last() {
                    line_bytes = &line_bytes[..line_bytes.len() - 1];
                }
                if !line_bytes.is_empty() && !line_bytes.starts_with(b"---") {
                    if let Ok(line) = std::str::from_utf8(line_bytes) {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            on_line(trimmed);
                        }
                    }
                }
                last_newline = idx + 1;
            }
        }
        if last_newline > 0 {
            buf.drain(..last_newline);
        } else if buf.len() > 8192 {
            let _ = writeln!(stderr(), "[WARN] Discarding oversized un-terminated logcat line (len={})", buf.len());
            buf.clear();
        }
    }

    pub(crate) fn run(&mut self) {
        if !self.json_stdout {
            let _ = writeln!(stdout(),
                "[DAEMON] mini-lmk active (mode: {}).",
                if self.act_mode { "ACT" } else { "OBSERVE" }
            );
            let _ = writeln!(stdout(), "[DAEMON] Monitoring FDs: [TOKEN_LOGCAT_PIPE, TOKEN_INOTIFY]");
            // D4: name the live dispatcher. An ignored `MINI_LMK_SPAWN` is reported as loudly as a
            // degraded one, because otherwise the same word - "fork" - means "as shipped", "because
            // your request made no sense" and "because /dev/null was unavailable", and only one of
            // those three is what the operator meant.
            let backend = self.spawner.backend().name();
            let ignored =
                spawn::unrecognised_backend(std::env::var("MINI_LMK_SPAWN").ok().as_deref());
            if let Some(text) = &ignored {
                let _ = writeln!(
                    stdout(),
                    "[DAEMON] ignoring MINI_LMK_SPAWN={:?}: not a backend (std, fork)",
                    text
                );
            }
            let detail = match self.spawner.note() {
                Some(note) => format!(" ({note})"),
                None => String::new(),
            };
            let _ = writeln!(
                stdout(),
                "[DAEMON] Kill dispatch backend: {backend}{detail}"
            );
            let _ = writeln!(
                stdout(),
                "[DAEMON] Root dispatch: {}",
                if self.is_root {
                    "SIGKILL for adj>=900 (revalidated per PID), AMS below"
                } else {
                    "shell uid; AMS only"
                }
            );
        }

        if self.telemetry.tabular_stdout() {
            let _ = writeln!(stdout(), "{:<12}   {:<12} {:<26} DETAIL / REASON", "# TIME", "EVENT", "TARGET");
            let _ = writeln!(stdout(), "{}", "-".repeat(80));
        }
        // The startup configuration record, held since `new()`: emitted here so its row lands under
        // the header just printed, and on disk for a headless run.
        self.emit_startup_record();

        let mut events_buf = [libc::epoll_event { events: 0, u64: 0 }; 8];
        let mut logcat_buf = Vec::with_capacity(4096);
        let mut read_buf = [0u8; 4096];

        while RUNNING.load(Ordering::Relaxed) {
            let timeout = if self.spawner.pending_child_reaps() > 0 { 60_000 } else { -1 };
            let nfds = unsafe {
                // F14 as amended by F22: sleep unbounded (-1) while nothing is pending, so the
                // "0 idle CPU wakeups" invariant holds outside a dispatch window. A dispatched
                // `cmd` child is normally reaped by the am_proc_died event its own kill emits;
                // the 60 s bound exists only for the silent case where that event never lands,
                // and stops at the end of the window it was opened by.
                libc::epoll_wait(self.epoll_fd, events_buf.as_mut_ptr(), events_buf.len() as i32, timeout)
            };

            if nfds < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    self.reap_terminated_children();
                    if !RUNNING.load(Ordering::Relaxed) {
                        break;
                    }
                    continue;
                }
                self.fatal(format_args!("epoll_wait error: {}", err));
            }
            self.reap_terminated_children();

            for ev in events_buf.iter().take(nfds as usize) {
                let token = ev.u64;
                let revents = ev.events;

                match token {
                    TOKEN_LOGCAT_PIPE => {
                        if revents & (libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0 {
                            if RUNNING.load(Ordering::Relaxed) {
                                self.fatal(format_args!("Logcat pipe HUP/ERR (0x{:x}). Exiting.", revents));
                            }
                            break;
                        }

                        loop {
                            let n = unsafe {
                                libc::read(self.logcat_fd, read_buf.as_mut_ptr() as *mut libc::c_void, read_buf.len())
                            };
                            if n > 0 {
                                Self::drain_logcat_lines(&mut logcat_buf, &read_buf[..n as usize], |line| {
                                    self.dispatch_logcat_line(line);
                                });
                            } else if n == 0 {
                                if RUNNING.load(Ordering::Relaxed) {
                                    self.fatal(format_args!("Logcat pipe EOF. Exiting."));
                                }
                                break;
                            } else {
                                let err = std::io::Error::last_os_error();
                                if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                                    break;
                                } else if err.raw_os_error() == Some(libc::EINTR) {
                                    if !RUNNING.load(Ordering::Relaxed) { break; }
                                    continue;
                                } else {
                                    self.fatal(format_args!("Logcat pipe error: {}. Exiting.", err));
                                }
                            }
                        }
                    }
                    TOKEN_INOTIFY => {
                        let mut inotify_buf = [0u8; 1024];
                        loop {
                            let n = unsafe {
                                libc::read(self.inotify_fd, inotify_buf.as_mut_ptr() as *mut libc::c_void, inotify_buf.len())
                            };
                            if n > 0 {
                                continue;
                            } else if n < 0 {
                                let err = std::io::Error::last_os_error();
                                if err.raw_os_error() == Some(libc::EINTR) { continue; }
                                break; // EAGAIN/EWOULDBLOCK or other
                            } else {
                                break;
                            }
                        }
                        if !self.json_stdout {
                            let _ = writeln!(stdout(), "[CONFIG] Configuration directory updated. Reloading...");
                        }
                        self.ensure_inotify_watch();
                        self.reload_configs();
                    }
                    _ => {}
                }
            }

            // One `write(2)` per `epoll_wait` batch instead of one per record: a burst of
            // lifecycle events lands in a single batch, and the batch is over in microseconds.
            self.telemetry.flush();
        }

        if !self.json_stdout {
            let _ = writeln!(stdout(), "[DAEMON] Shutdown signal received. Exiting.");
        }
        if let Some(mut child) = self.logcat_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.reap_terminated_children();
        self.telemetry.flush();
    }

    pub(crate) fn dispatch_logcat_line(&mut self, line: &str) {
        if let Some((now_epoch, event)) = parse_logcat_line(line) {
            // Wall-clock step compensation: shift all anchors together, keep event ts as-is.
            self.apply_clock_jump(Self::clock_jump_ms(self.last_event_epoch, now_epoch));
            self.last_event_epoch = now_epoch;

            match event {
                LogcatEvent::ResumeActivity(ev) => self.on_resume_activity(&ev, now_epoch),
                LogcatEvent::ProcStart(ev) => self.on_proc_start(&ev, now_epoch),
                LogcatEvent::ProcDied(ev) => self.on_proc_died(&ev, now_epoch),
                LogcatEvent::ScreenToggled(state) => self.on_screen_toggled(state, now_epoch),
                LogcatEvent::DeviceIdleLightStep => {
                    if !self.screen_on && self.config.screen_off_harvest {
                        self.evaluate_reaping_pipeline(now_epoch);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::telemetry::TelemetrySink;

    /// A config tree the test owns. `ConfigPaths::get()` caches `$MODPATH` for the whole process and
    /// otherwise falls back to `/data/local/tmp/mlmk`, so an ambient `daemon.conf` there (which the
    /// device scratch dir does contain) would decide what these tests see.
    pub(crate) fn isolated_paths(tag: &str) -> (ConfigPaths, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("mlmk_cfg_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&base);
        let config_dir = base.join("config");
        let logs_dir = base.join("logs");
        std::fs::create_dir_all(&config_dir).expect("test config dir");
        std::fs::create_dir_all(&logs_dir).expect("test logs dir");
        let owned = |p: &std::path::Path| p.to_string_lossy().to_string();
        (
            ConfigPaths {
                base_dir: owned(&base),
                config_dir: owned(&config_dir),
                logs_dir: owned(&logs_dir),
                config_file: owned(&config_dir.join("daemon.conf")),
                exclude_file: owned(&config_dir.join("exclude.list")),
                games_file: owned(&config_dir.join("games.list")),
                operations_log: owned(&logs_dir.join("operations.log")),
            },
            base,
        )
    }

    /// Test-only `DaemonState` with inert descriptors and a throwaway telemetry file,
    /// so handler and pipeline paths can be exercised without spawning `logcat`.
    pub(crate) fn daemon_for_test(tag: &str, bootstrap_epoch: u64) -> (DaemonState, std::path::PathBuf) {
        let log_path =
            std::env::temp_dir().join(format!("mlmk_test_{}_{}.log", std::process::id(), tag));
        // The sink opens (and thereby creates) this path, so a leftover from an earlier run must
        // go first: unlinking afterwards would leave the writer appending to a deleted inode.
        let _ = std::fs::remove_file(&log_path);
        let daemon = DaemonState {
            config: RuntimeConfig::default(),
            json_stdout: false,
            no_log_cli: false,
            config_announced: false,
            startup_record: None,
            // The test harness keeps the shipped dispatcher. It is the backend the daemon runs by
            // default, so a test that dispatches at all exercises the same code a device does.
            spawner: spawn::Spawner::new(spawn::Backend::Fork),
            // Host tests are not root, so the SIGKILL fast path stays off and every dispatch
            // decision below mirrors the shell deployment.
            is_root: false,
            alive_apps: FastMap::default(),
            pid_to_pkg: FastMap::default(),
            pkg_to_pids: FastMap::default(),
            pkg_to_uid: FastMap::default(),
            recent_deaths: FastMap::default(),
            user_exclusions: FastSet::default(),
            games: FastSet::default(),
            dynamic_exclusions: FastSet::default(),
            fg_lru: VecDeque::default(),
            telemetry: TelemetrySink::new(&log_path.to_string_lossy(), false, true),
            logcat_child: None,
            current_fg: None,
            screen_on_start: Some(bootstrap_epoch),
            screen_off_start: None,
            game_session_start: None,
            session_start: bootstrap_epoch,
            last_event_epoch: bootstrap_epoch,
            session_stats: SessionStats::default(),
            page_size_kb: 4,
            epoll_fd: -1,
            logcat_fd: -1,
            inotify_fd: -1,
            game_intrusion_count: 0,
            screen_on: true,
            is_gaming: false,
            act_mode: true,
        };
        (daemon, log_path)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{daemon_for_test, isolated_paths};
    use super::*;

    #[test]
    fn test_config_reload_renderings_report_effective_log_switch() {
        // log_enabled=true in the config, but --no-log means nothing is written. The record must
        // say false: it is the only place an operator learns what the daemon is doing with the log.
        let cfg = RuntimeConfig {
            log_enabled: true,
            ..RuntimeConfig::default()
        };
        let tabular = DaemonState::config_reload_tabular(&cfg, false, 1_700_000_000_000);
        let json = DaemonState::config_reload_json(&cfg, false, 1_700_000_000_000);
        assert!(json.ends_with(r#""log_enabled":false}"#), "{json}");
        assert!(tabular.contains("log=false"), "{tabular}");
        assert!(json.contains(r#""min_oom_score_adj":900"#), "{json}");

        // Every tunable in RuntimeConfig must appear in the record, or a change to it is
        // undocumentable by the event that exists to document changes. Counts, not just presence:
        // a silently-added ninth field would otherwise pass unnoticed.
        // 10 = ts + event + the 8 RuntimeConfig fields.
        assert_eq!(json.matches(':').count(), 10, "one per key: {json}");
        assert_eq!(tabular.matches('=').count(), 8, "{tabular}");
    }

    #[test]
    fn test_reload_applies_the_daemon_conf_it_reads() {
        // The startup announcement is only meaningful if the reload actually read the file, and
        // that line has no other protection: deleting it leaves every other test green because the
        // daemon simply keeps its defaults and still announces them. So assert the record carries
        // values a file supplied, not values the defaults happen to have.
        let (mut d, log) = daemon_for_test("reload_file", 1_700_000_000_000);
        let (paths, base) = isolated_paths("reload_file");
        std::fs::write(
            &paths.config_file,
            "t_idle_sec = 300\nlru_protect_depth = 6\nmax_kills_per_pass = 5\n",
        )
        .expect("write daemon.conf");

        d.reload_configs_from(&paths);
        d.emit_startup_record();

        assert_eq!(d.config.t_idle_sec, 300, "the file must reach the live config");
        assert_eq!(d.config.lru_protect_depth, 6);
        assert_eq!(d.config.max_kills_per_pass, 5);
        let text = std::fs::read_to_string(&log).expect("the reload must reach the log");
        assert!(text.contains(r#""t_idle_sec":300"#), "{text}");
        assert!(text.contains(r#""lru_protect_depth":6"#), "{text}");
        assert!(text.contains(r#""max_kills_per_pass":5"#), "{text}");

        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_first_config_load_is_announced_even_without_a_change() {
        // A headless daemon prints `[CONFIG] Active: ...` to a stdout that service.sh sends to
        // /dev/null, so operations.log is the only place the applied configuration can appear. It
        // has to appear even when daemon.conf is absent or matches the defaults, and it has to
        // reach the disk rather than wait in the 8 KB buffer for an event batch this quiet run
        // never gets. It is emitted by emit_startup_record(), which run() calls only after the
        // terminal table header, so the row never lands above the header it belongs to.
        let (mut d, log) = daemon_for_test("announce", 1_700_000_000_000);
        let (paths, base) = isolated_paths("announce");
        d.config = RuntimeConfig {
            t_idle_sec: 180,
            log_enabled: true,
            ..RuntimeConfig::default()
        };

        d.reload_configs_from(&paths);
        // What run() does once the terminal table header exists. No explicit flush here on purpose:
        // the record must already be on disk when emit_startup_record() returns.
        d.emit_startup_record();
        let text = std::fs::read_to_string(&log).expect("first load must be on disk, not buffered");
        assert_eq!(
            text.matches(r#""event":"config_reload""#).count(),
            1,
            "exactly one startup record: {text}"
        );
        assert!(text.ends_with('\n'), "record is a complete line: {text}");

        // Change detection is not simply gone: a second load that moves nothing adds nothing.
        d.reload_configs_from(&paths);
        d.telemetry.flush();
        let again = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            again.matches(r#""event":"config_reload""#).count(),
            1,
            "an unchanged reload must stay silent"
        );
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_ams_protected_alive_apps_lifecycle() {
        let mut alive_apps: FastMap<String, u64> = FastMap::default();
        let old_time = 1_000_000u64;
        alive_apps.insert("com.spotify.music".to_string(), old_time);
        alive_apps.insert("org.mozilla.firefox".to_string(), old_time);

        let now = 1_300_000u64;

        // Spotify is ams_protected (e.g. oom_adj = 200 < 900): anchor refreshed, not removed.
        *alive_apps.get_mut("com.spotify.music").unwrap() = now;

        // Firefox is NOT protected (e.g. oom_adj = 900 >= 900): killed and dropped.
        alive_apps.remove("org.mozilla.firefox");

        assert_eq!(alive_apps.get("com.spotify.music"), Some(&now));
        assert!(!alive_apps.contains_key("org.mozilla.firefox"));
    }

    #[test]
    fn test_drain_logcat_lines_hygiene() {
        // CRLF stripped, `---` logcat headers suppressed, blank lines dropped,
        // partial trailing line kept in the buffer, oversize line discarded.
        let mut buf = Vec::new();
        let mut seen = Vec::new();
        let incoming = b"line1\r\n--- wiping\nline2\npartial";
        DaemonState::drain_logcat_lines(&mut buf, incoming, |l| seen.push(l.to_string()));
        assert_eq!(seen, ["line1", "line2"]);
        assert_eq!(buf, b"partial");

        seen.clear();
        DaemonState::drain_logcat_lines(&mut buf, b"tail\r\n", |l| seen.push(l.to_string()));
        assert_eq!(seen, ["partialtail"]);

        let mut big = Vec::new();
        let mut warned = Vec::new();
        DaemonState::drain_logcat_lines(&mut big, &[b'x'; 9000], |_| warned.push(()));
        assert!(big.is_empty(), "oversize unterminated line must be discarded");
    }
}
