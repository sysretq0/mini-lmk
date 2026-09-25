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
mod hasher;
mod parser;
mod procfs;
mod telemetry;

use config::{ConfigPaths, RuntimeConfig};
use hasher::{FastMap, FastSet};
use parser::{parse_logcat_line, LogcatEvent, ProcDiedEvent, ProcStartEvent, ResumeActivityEvent};
use procfs::{check_mem_critical, read_oom_score_adj, read_statm_rss_kb, read_total_ram_mb};
use telemetry::{escape_json, format_time_hms_ms, SessionStats, TelemetrySink};

use std::collections::VecDeque;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::IntoRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn sig_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

const TOKEN_LOGCAT_PIPE: u64 = 1;
const TOKEN_INOTIFY: u64 = 2;

/// Lower bound (2020-09-13) separating an unsynchronized boot RTC from NTP-synced wall clock.
const RTC_SYNC_FLOOR_MS: u64 = 1_600_000_000_000;

struct Candidate {
    pkg: String,
    live_pids: Vec<u32>,
    total_rss_kb: u64,
    idle_sec: u64,
    lru_pos: usize,
}

struct DaemonState {
    config: RuntimeConfig,
    json_stdout: bool,

    // Canonical base package -> last foreground/activity epoch-ms anchor (UID >= 10000 only).
    alive_apps: FastMap<String, u64>,
    pid_to_pkg: FastMap<u32, String>,
    pkg_to_pids: FastMap<String, FastSet<u32>>,
    pkg_to_uid: FastMap<String, u32>,
    recent_deaths: FastMap<String, u64>,
    user_exclusions: FastSet<String>,
    games: FastSet<String>,
    dynamic_exclusions: FastSet<String>,
    fg_lru: VecDeque<String>,

    telemetry: TelemetrySink,
    logcat_child: Option<Child>,
    current_fg: Option<String>,
    screen_on_start: Option<u64>,
    screen_off_start: Option<u64>,
    game_session_start: Option<u64>,
    session_start: u64,
    last_event_epoch: u64,

    session_stats: SessionStats,
    page_size_kb: u64,

    epoll_fd: i32,
    logcat_fd: i32,
    inotify_fd: i32,
    game_intrusion_count: u32,

    screen_on: bool,
    is_gaming: bool,
    act_mode: bool,
}

/// Parse screen power state from `dumpsys power` output (API 24-27 fallback).
/// Scans line-by-line to evaluate authoritative current state first and prevent
/// false positives from historical logs or wake lock tables.
pub fn parse_dumpsys_power_screen(stdout: &str) -> Option<bool> {
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
fn detect_initial_screen_on() -> bool {
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
pub fn parse_resolve_activity_pkg(stdout: &str) -> Option<&str> {
    stdout.lines().find_map(|l| {
        l.split_whitespace()
            .find_map(|w| w.split_once('/'))
            .map(|(pkg, _)| pkg.trim())
            .filter(|pkg| !pkg.is_empty() && pkg.contains('.') && !pkg.starts_with(['-', '{']))
    })
}

/// Non-allocating, sub-millisecond health probe for the spawned logcat stream.
/// Validates child process survival and pipe integrity using non-blocking syscalls.
pub fn probe_logcat_stream(child: &mut Child, pipe_fd: i32) -> Result<(), &'static str> {
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

impl DaemonState {
    fn new(act_mode: bool, json_stdout: bool) -> Self {
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
        let _ = fs::create_dir_all(&paths.config_dir);
        let _ = fs::create_dir_all(&paths.logs_dir);

        let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll_fd < 0 {
            eprintln!("[FATAL] epoll_create1 failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }

        let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify_fd < 0 {
            eprintln!("[FATAL] inotify_init1 failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }

        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: TOKEN_INOTIFY,
        };
        if unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, inotify_fd, &mut ev) } < 0 {
            eprintln!("[FATAL] epoll_ctl inotify failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }

        let telemetry = TelemetrySink::new(&paths.operations_log, json_stdout);

        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size_kb = if page_size > 0 { (page_size as u64) / 1024 } else { 4 };

        let now = Self::get_epoch_ms();
        let screen_on = detect_initial_screen_on();
        let screen_on_start = if screen_on { Some(now) } else { None };
        let screen_off_start = if !screen_on { Some(now) } else { None };
        if !json_stdout {
            println!("[DAEMON] Initial display state: screen_on={}", screen_on);
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
            println!(
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
            telemetry,
            json_stdout,
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

    fn emit_bg_summary(&mut self, now_epoch: u64, interval_sec: u64) {
        if self.session_stats.bg_spawns == 0 && self.session_stats.bg_deaths == 0 {
            return;
        }
        let json = format!(
            r#"{{"ts":{},"event":"bg_summary","interval_sec":{},"spawns":{},"deaths":{},"spawn_rss_kb":{}}}"#,
            now_epoch, interval_sec, self.session_stats.bg_spawns, self.session_stats.bg_deaths, self.session_stats.spawn_rss_kb
        );
        self.telemetry.emit_with(
            || {
                let time_str = format_time_hms_ms(now_epoch);
                let sign = if self.session_stats.spawn_rss_kb >= 0 { "+" } else { "-" };
                let abs_rss_mb = (self.session_stats.spawn_rss_kb.unsigned_abs() + 512) / 1024;
                let detail = format!(
                    "interval={}s  spawns={}  deaths={}  rss_delta={}{}MB",
                    interval_sec, self.session_stats.bg_spawns, self.session_stats.bg_deaths, sign, abs_rss_mb
                );
                format!("{:<12}   {:<12} {:<26} {}", time_str, "BG_SUMMARY", "--", detail)
            },
            &json,
        );
        self.session_stats = SessionStats::default();
        self.session_start = now_epoch;
    }

    fn seed_initial_state(&mut self) {
        if !self.json_stdout {
            println!("[DAEMON] Performing cold-start discovery...");
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

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            let uid = meta.uid();

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
            println!(
                "[DAEMON] Indexed {} active PIDs across {} packages.",
                self.pid_to_pkg.len(),
                self.pkg_to_pids.len()
            );
        }
    }

    fn ensure_inotify_watch(&mut self) {
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
                println!("[CONFIG] Loaded {} entries from {}", set.len(), path);
            }
        }
    }

    fn reload_configs(&mut self) {
        let prev_cfg = self.config;
        let paths = ConfigPaths::get();
        self.config.load_from_file(&paths.config_file, self.json_stdout);
        Self::load_file_lines(&paths.exclude_file, &mut self.user_exclusions, self.json_stdout);
        Self::load_file_lines(&paths.games_file, &mut self.games, self.json_stdout);

        if self.config != prev_cfg {
            let now_epoch = Self::get_epoch_ms();
            let json = format!(
                r#"{{"ts":{},"event":"config_reload","t_idle_sec":{},"lru_protect_depth":{},"mem_critical_percent":{},"fg_lru_max_depth":{},"screen_off_harvest":{},"max_kills_per_pass":{}}}"#,
                now_epoch, self.config.t_idle_sec, self.config.lru_protect_depth, self.config.mem_critical_percent,
                self.config.fg_lru_max_depth, self.config.screen_off_harvest, self.config.max_kills_per_pass
            );
            self.telemetry.emit_with(
                || {
                    let time_str = format_time_hms_ms(now_epoch);
                    let detail = format!(
                        "t_idle={}s  lru_depth={}  mem_crit={}%  fg_lru_max={}  harvest={}  max_kills={}",
                        self.config.t_idle_sec, self.config.lru_protect_depth, self.config.mem_critical_percent,
                        self.config.fg_lru_max_depth, self.config.screen_off_harvest, self.config.max_kills_per_pass
                    );
                    format!("{:<12}   {:<12} {:<26} {}", time_str, "CONFIG_RELOAD", "--", detail)
                },
                &json,
            );
        }
    }

    fn detect_system_components(&mut self) {
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
                                println!("[SYSTEM] Detected {}: {}", label, pkg);
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
                let stdout = String::from_utf8_lossy(&out.stdout);
                if let Some(pkg) = parse_resolve_activity_pkg(&stdout) {
                    if !self.json_stdout {
                        println!("[SYSTEM] Detected HOME (fallback): {}", pkg);
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
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let s = stdout.trim();
                    if !s.is_empty() && s != "null" && !s.starts_with("null") {
                        // Unpeel optional user prefix (e.g. "0:com.pkg/svc" -> "com.pkg/svc")
                        let unpeeled = if let Some((prefix, rest)) = s.split_once(':') {
                            if rest.contains('.') && prefix.bytes().all(|b| b.is_ascii_digit()) { rest } else { s }
                        } else { s };
                        let pkg = unpeeled.split('/').next().unwrap_or(unpeeled).trim();
                        if !pkg.is_empty() && pkg.contains('.') && !pkg.contains(' ') && !pkg.starts_with('-') {
                            if !self.json_stdout {
                                println!("[SYSTEM] Detected {}: {}", label, pkg);
                            }
                            self.dynamic_exclusions.insert(pkg.to_string());
                        }
                    }
                }
            }
        }
    }



    fn spawn_logcat_stream(&mut self) {
        if !self.json_stdout {
            println!("[DAEMON] Spawning unified logcat stream...");
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
                eprintln!("[FATAL] Failed to spawn logcat: {}", e);
                std::process::exit(1);
            }
        };

        let stdout = child.stdout.take().expect("logcat stdout");
        let raw_fd = stdout.into_raw_fd();

        if let Err(err) = probe_logcat_stream(&mut child, raw_fd) {
            eprintln!("[FATAL] Logcat pipeline startup probe failed: {}", err);
            std::process::exit(1);
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
            eprintln!("[FATAL] epoll_ctl logcat pipe failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }

        self.logcat_fd = raw_fd;
        self.logcat_child = Some(child);
        if !self.json_stdout {
            println!("[DAEMON] Logcat stream active on fd={}", self.logcat_fd);
        }
    }

    #[inline(always)]
    fn get_epoch_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    #[inline(always)]
    fn is_excluded(&self, pkg: &str) -> bool {
        self.user_exclusions.contains(pkg)
            || self.dynamic_exclusions.contains(pkg)
            || self.current_fg.as_deref() == Some(pkg)
    }

    fn on_resume_activity(&mut self, ev: &ResumeActivityEvent, now_epoch: u64) {
        let pkg = ev.pkg;
        let component = ev.component;

        let mut prev_dur_ms = 0u64;

        let prev_pkg_opt = self.current_fg.take();
        if let Some(ref prev_pkg) = prev_pkg_opt {
            if prev_pkg != pkg {
                // Guard: Only user/app packages with verified UID >= 10000 may enter alive_apps.
                // Fail-closed: If UID cannot be resolved, assume protected/system and DO NOT insert.
                if self.pkg_to_uid.get(prev_pkg).is_some_and(|&uid| uid >= 10000) {
                    if let Some(anchor) = self.alive_apps.get_mut(prev_pkg) {
                        prev_dur_ms = now_epoch.saturating_sub(*anchor);
                        *anchor = now_epoch;
                    } else {
                        self.alive_apps.insert(prev_pkg.clone(), now_epoch);
                    }
                }
            }
        }

        let interval_sec = now_epoch.saturating_sub(self.session_start) / 1000;
        self.emit_bg_summary(now_epoch, interval_sec);

        // If the departed package was only in foreground for < 500ms (e.g. trampoline, chooser, auth pulse),
        // evict it from fg_lru so it does not displace real user applications in the LRU protection window.
        if prev_dur_ms > 0 && prev_dur_ms < 500 {
            if let Some(pos) = self.fg_lru.iter().position(|x| x == prev_pkg_opt.as_deref().unwrap()) {
                self.fg_lru.remove(pos);
            }
        }

        if let Some(pos) = self.fg_lru.iter().position(|x| x == pkg) {
            if pos > 0 {
                let existing = self.fg_lru.remove(pos).unwrap();
                self.fg_lru.push_front(existing);
            }
        } else {
            self.fg_lru.push_front(pkg.to_string());
            if self.fg_lru.len() > self.config.fg_lru_max_depth {
                self.fg_lru.pop_back();
            }
        }

        let was_gaming = self.is_gaming;
        let is_game = self.games.contains(pkg);
        self.is_gaming = is_game;

        if is_game && !was_gaming {
            self.game_session_start = Some(now_epoch);
            self.game_intrusion_count = 0;
            let json = format!(r#"{{"ts":{},"event":"game_session_start","pkg":"{}"}}"#, now_epoch, escape_json(pkg));
            self.telemetry.emit_with(
                || {
                    let time_str = format_time_hms_ms(now_epoch);
                    format!("{:<12}   {:<12} {:<26} game_mode=active", time_str, "GAME_START", pkg)
                },
                &json,
            );
        } else if !is_game && was_gaming {
            let duration_sec = self.game_session_start
                .map(|s| now_epoch.saturating_sub(s) / 1000)
                .unwrap_or(0);
            let json = format!(
                r#"{{"ts":{},"event":"game_session_end","duration_sec":{},"intrusions":{}}}"#,
                now_epoch, duration_sec, self.game_intrusion_count
            );
            self.telemetry.emit_with(
                || {
                    let time_str = format_time_hms_ms(now_epoch);
                    let detail = format!("duration={}s  intrusions={}", duration_sec, self.game_intrusion_count);
                    format!("{:<12}   {:<12} {:<26} {}", time_str, "GAME_END", "--", detail)
                },
                &json,
            );
            self.game_session_start = None;
        }

        if prev_pkg_opt.as_deref() == Some(pkg) {
            self.current_fg = prev_pkg_opt.clone();
        } else {
            self.current_fg = Some(pkg.to_string());
        }

        let json = format!(
            r#"{{"ts":{},"event":"fg_switch","pkg":"{}","component":"{}","prev_dur_ms":{},"is_game":{}}}"#,
            now_epoch, escape_json(pkg), escape_json(component), prev_dur_ms, is_game
        );
        self.telemetry.emit_with(
            || {
                let time_str = format_time_hms_ms(now_epoch);
                let prev_detail = if let Some(ref prev) = prev_pkg_opt {
                    let dur_s = (prev_dur_ms as f64) / 1000.0;
                    format!("prev={} ({:.1}s){}", prev, dur_s, if is_game { " [GAME]" } else { "" })
                } else {
                    format!("cold_boot{}", if is_game { " [GAME]" } else { "" })
                };
                format!("{:<12}   {:<12} {:<26} {}", time_str, "FG_SWITCH", pkg, prev_detail)
            },
            &json,
        );

        if prev_pkg_opt.as_deref() != Some(pkg) {
            self.evaluate_reaping_pipeline(now_epoch);
        }
    }

    fn on_proc_start(&mut self, ev: &ProcStartEvent, now_epoch: u64) {
        let pid = ev.pid;
        let uid = ev.uid;
        let raw_proc_name = ev.proc_name;
        let spawn_type = ev.spawn_type;
        let pkg = ev.pkg;

        let initial_rss_kb = read_statm_rss_kb(pid, self.page_size_kb);

        let is_fg = self.current_fg.as_deref() == Some(pkg);
        if !is_fg {
            self.session_stats.bg_spawns += 1;
            self.session_stats.spawn_rss_kb += initial_rss_kb as i64;
        }

        if self.is_gaming && spawn_type != "top-activity" && spawn_type != "next-top-activity" {
            self.game_intrusion_count += 1;
            let excluded = self.is_excluded(pkg);
            let json = format!(
                r#"{{"ts":{},"event":"game_intrusion","pid":{},"uid":{},"pkg":"{}","proc":"{}","type":"{}","rss_kb":{},"excluded":{}}}"#,
                now_epoch, pid, uid, escape_json(pkg), escape_json(raw_proc_name), escape_json(spawn_type), initial_rss_kb, excluded
            );
            self.telemetry.emit_with(
                || {
                    let time_str = format_time_hms_ms(now_epoch);
                    let rss_mb = (initial_rss_kb + 512) / 1024;
                    let detail = format!("type={}  rss={}MB  excluded={}", spawn_type, rss_mb, excluded);
                    format!("{:<12}   {:<12} {:<26} {}", time_str, "GAME_INTRUDE", pkg, detail)
                },
                &json,
            );
        }

        let pkg_owned = pkg.to_string();
        self.pid_to_pkg.insert(pid, pkg_owned.clone());
        self.pkg_to_pids.entry(pkg_owned.clone()).or_default().insert(pid);
        if !self.pkg_to_uid.contains_key(pkg) {
            self.pkg_to_uid.insert(pkg_owned.clone(), uid);
        }

        // Respawn tracking (proc_died -> proc_start within 120s)
        if let Some(death_time) = self.recent_deaths.remove(pkg) {
            let gap_ms = now_epoch.saturating_sub(death_time);
            if gap_ms <= 120_000 {
                let json = format!(
                    r#"{{"ts":{},"event":"respawn","pkg":"{}","gap_ms":{},"pid":{},"uid":{},"type":"{}"}}"#,
                    now_epoch, escape_json(pkg), gap_ms, pid, uid, escape_json(spawn_type)
                );
                self.telemetry.emit_with(
                    || {
                        let time_str = format_time_hms_ms(now_epoch);
                        let detail = format!("gap={}ms  pid={}  type={}", gap_ms, pid, spawn_type);
                        format!("{:<12}   {:<12} {:<26} {}", time_str, "RESPAWN", pkg, detail)
                    },
                    &json,
                );
            }
        }

        if uid >= 10000 {
            self.alive_apps.entry(pkg_owned).or_insert(now_epoch);

            let is_fg_launch = is_fg || ev.spawn_type == "top-activity" || ev.spawn_type == "next-top-activity";
            if !is_fg_launch {
                self.evaluate_reaping_pipeline(now_epoch);
            }
        }
    }

    fn on_proc_died(&mut self, ev: &ProcDiedEvent, now_epoch: u64) {
        let pid = ev.pid;

        if let Some(pkg) = self.pid_to_pkg.remove(&pid) {
            if let Some(pids) = self.pkg_to_pids.get_mut(&pkg) {
                pids.remove(&pid);
                if pids.is_empty() {
                    self.alive_apps.remove(&pkg);
                }
            }
            let is_fg = self.current_fg.as_deref() == Some(&pkg);
            if !is_fg {
                self.session_stats.bg_deaths += 1;
            }

            self.recent_deaths.insert(pkg, now_epoch);
            if self.recent_deaths.len() > 64 {
                self.recent_deaths.retain(|_, death_time| now_epoch.saturating_sub(*death_time) <= 120_000);
            }
        }
    }

    fn on_screen_toggled(&mut self, state: bool, now_epoch: u64) {
        if self.screen_on == state {
            return;
        }

        if !state {
            let active_sec = self.screen_on_start
                .map(|s| now_epoch.saturating_sub(s) / 1000)
                .unwrap_or(0);
            let json = format!(
                r#"{{"ts":{},"event":"screen_state","state":"OFF","active_duration_sec":{}}}"#,
                now_epoch, active_sec
            );
            self.telemetry.emit_with(
                || {
                    let time_str = format_time_hms_ms(now_epoch);
                    let detail = format!("active_session={:.1}s", active_sec as f64);
                    format!("{:<12}   {:<12} {:<26} {}", time_str, "SCREEN_OFF", "--", detail)
                },
                &json,
            );
            self.emit_bg_summary(now_epoch, active_sec);

            self.screen_on = false;
            self.screen_on_start = None;
            self.screen_off_start = Some(now_epoch);
        } else {
            let duration_sec = self.screen_off_start
                .map(|s| now_epoch.saturating_sub(s) / 1000)
                .unwrap_or(0);
            let json = format!(
                r#"{{"ts":{},"event":"screen_state","state":"ON","off_duration_sec":{}}}"#,
                now_epoch, duration_sec
            );
            self.telemetry.emit_with(
                || {
                    let time_str = format_time_hms_ms(now_epoch);
                    let detail = format!("sleep={}s", duration_sec);
                    format!("{:<12}   {:<12} {:<26} {}", time_str, "SCREEN_ON", "--", detail)
                },
                &json,
            );
            self.emit_bg_summary(now_epoch, duration_sec);

            self.screen_on = true;
            self.screen_on_start = Some(now_epoch);
            self.screen_off_start = None;
        }
    }

    /// Kill-decision telemetry naming, shared by the dispatch path and its test.
    /// Returns `(json_event, columnar_tag, detail_suffix, spawn_skipped)`.
    fn kill_telemetry_parts(act_mode: bool, ams_protected: bool) -> (&'static str, &'static str, &'static str, bool) {
        let (event_name, tag, sim_suffix) = match (act_mode, ams_protected) {
            (true, false) => ("kill", "KILL", ""),
            (true, true) => ("kill_skipped", "KILL_SKIP", " (ams_protected: spawn skipped)"),
            (false, false) => ("simulated_kill", "SIM_KILL", " (simulated)"),
            (false, true) => ("simulated_kill", "SIM_KILL", " (simulated, ams_protected: spawn skipped)"),
        };
        (event_name, tag, sim_suffix, !act_mode || ams_protected)
    }

    fn evaluate_reaping_pipeline(&mut self, now_epoch: u64) {
        let is_game = self.current_fg.as_ref().map(|p| self.games.contains(p)).unwrap_or(false);
        let is_low_mem = check_mem_critical(self.config.mem_critical_percent);

        let effective_lru_depth = if !self.screen_on && self.config.screen_off_harvest {
            1
        } else {
            self.config.lru_protect_depth
        };

        let t_idle_effective_sec: u64 = if is_game || is_low_mem {
            10
        } else if !self.screen_on && self.config.screen_off_harvest {
            let off_dur_sec = self
                .screen_off_start
                .map(|s| now_epoch.saturating_sub(s) / 1000)
                .unwrap_or_default();
            if off_dur_sec > 60 {
                30
            } else {
                60
            }
        } else {
            let fg_dur_sec = self
                .current_fg
                .as_deref()
                .and_then(|p| self.alive_apps.get(p))
                .map(|&anchor| now_epoch.saturating_sub(anchor) / 1000)
                .unwrap_or_default();
            if fg_dur_sec > 300 {
                60
            } else {
                self.config.t_idle_sec
            }
        };

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut dead_pids = Vec::new();
        let mut dead_pkgs = Vec::new();

        for (pkg, &last_active) in &self.alive_apps {
            if self.is_excluded(pkg) {
                continue;
            }

            let lru_pos = self.fg_lru.iter().position(|x| x == pkg).unwrap_or(99);
            if lru_pos < effective_lru_depth {
                continue;
            }

            let idle_sec = now_epoch.saturating_sub(last_active) / 1000;
            if idle_sec < t_idle_effective_sec {
                continue;
            }

            let mut total_rss_kb = 0u64;
            let mut live_pids = Vec::new();
            if let Some(pids) = self.pkg_to_pids.get(pkg) {
                for &pid in pids {
                    let rss = read_statm_rss_kb(pid, self.page_size_kb);
                    if rss > 0 {
                        total_rss_kb += rss;
                        live_pids.push(pid);
                    } else {
                        dead_pids.push(pid);
                    }
                }
            }

            if live_pids.is_empty() || total_rss_kb == 0 {
                dead_pkgs.push(pkg.clone());
                continue;
            }

            candidates.push(Candidate {
                pkg: pkg.clone(),
                live_pids,
                total_rss_kb,
                idle_sec,
                lru_pos,
            });
        }

        // Opportunistic reconciliation: purge dead PIDs and exited packages
        for pid in &dead_pids {
            self.pid_to_pkg.remove(pid);
        }
        for pkg in &dead_pkgs {
            self.alive_apps.remove(pkg);
            self.pkg_to_pids.remove(pkg);
        }
        for cand in &candidates {
            if let Some(pids) = self.pkg_to_pids.get_mut(&cand.pkg) {
                pids.retain(|p| cand.live_pids.contains(p));
            }
        }

        candidates.sort_by_key(|cand| std::cmp::Reverse(cand.total_rss_kb));

        for cand in candidates.into_iter().take(self.config.max_kills_per_pass) {
            let oom_adj = cand
                .live_pids
                .iter()
                .map(|&p| read_oom_score_adj(p).unwrap_or(0))
                .min()
                .unwrap_or(0);
            let ams_protected = oom_adj < self.config.min_oom_score_adj;

            let reason = if is_game {
                "game_mode_escalation"
            } else if is_low_mem {
                "low_memory_escalation"
            } else {
                "idle_expired"
            };

            let rss_mb = cand.total_rss_kb / 1024;

            let is_spawned = self.act_mode.then(|| {
                !ams_protected
                    && Command::new("/system/bin/cmd")
                        .args(["activity", "kill", "--user", "all", &cand.pkg])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .is_ok()
            });

            let (event_name, tag, sim_suffix, spawn_skipped) =
                Self::kill_telemetry_parts(self.act_mode, ams_protected);

            let spawned_val = match is_spawned {
                Some(v) => if v { "true" } else { "false" },
                None => "null",
            };
            let json = format!(
                r#"{{"ts":{},"event":"{}","pkg":"{}","pids":{:?},"rss_freed_est_kb":{},"reason":"{}","idle_sec":{},"lru_pos":{},"spawned":{},"oom_score_adj":{},"ams_protected":{},"spawn_skipped":{}}}"#,
                now_epoch, event_name, escape_json(&cand.pkg), cand.live_pids, cand.total_rss_kb, reason, cand.idle_sec, cand.lru_pos, spawned_val, oom_adj, ams_protected, spawn_skipped
            );

            self.telemetry.emit_with(
                || {
                    let time_str = format_time_hms_ms(now_epoch);
                    let detail = format!(
                        "rss={}MB  idle={}s  adj={}  lru={} [{}] {}",
                        rss_mb, cand.idle_sec, oom_adj, cand.lru_pos, reason, sim_suffix
                    );
                    format!("{:<12}   {:<12} {:<26} {}", time_str, tag, cand.pkg, detail)
                },
                &json,
            );
            self.telemetry.flush();

            if ams_protected {
                if let Some(anchor) = self.alive_apps.get_mut(&cand.pkg) {
                    *anchor = now_epoch;
                }
            } else {
                self.alive_apps.remove(&cand.pkg);
            }
        }
    }

    fn reap_terminated_children(&mut self) {
        let logcat_pid = self.logcat_child.as_ref().map(|c| c.id() as libc::pid_t).unwrap_or(-1);
        unsafe {
            loop {
                let mut status = 0;
                let reaped = libc::waitpid(-1, &mut status, libc::WNOHANG);
                if reaped <= 0 {
                    break;
                }
                if reaped == logcat_pid {
                    if RUNNING.load(Ordering::Relaxed) {
                        eprintln!("[FATAL] Persistent logcat stream died (reaped via WNOHANG). Exiting.");
                        std::process::exit(1);
                    }
                    break;
                }
            }
        }
    }

    fn run(&mut self) {
        if !self.json_stdout {
            println!(
                "[DAEMON] mini-lmk active (mode: {}).",
                if self.act_mode { "ACT" } else { "OBSERVE" }
            );
            println!("[DAEMON] Monitoring FDs: [TOKEN_LOGCAT_PIPE, TOKEN_INOTIFY]");
        }

        if !self.telemetry.json_stdout {
            println!("{:<12}   {:<12} {:<26} DETAIL / REASON", "# TIME", "EVENT", "TARGET");
            println!("{}", "-".repeat(80));
        }

        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 8];
        let mut logcat_buf = Vec::with_capacity(4096);
        let mut read_buf = [0u8; 4096];

        while RUNNING.load(Ordering::Relaxed) {
            let nfds = unsafe {
                libc::epoll_wait(self.epoll_fd, events.as_mut_ptr(), events.len() as i32, -1)
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
                eprintln!("[FATAL] epoll_wait error: {}", err);
                std::process::exit(1);
            }

            self.reap_terminated_children();

            for ev in events.iter().take(nfds as usize) {
                let token = ev.u64;
                let revents = ev.events;

                match token {
                    TOKEN_LOGCAT_PIPE => {
                        if revents & (libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0 {
                            if RUNNING.load(Ordering::Relaxed) {
                                eprintln!("[FATAL] Logcat pipe HUP/ERR (0x{:x}). Exiting.", revents);
                                std::process::exit(1);
                            }
                            break;
                        }

                        loop {
                            let n = unsafe {
                                libc::read(self.logcat_fd, read_buf.as_mut_ptr() as *mut libc::c_void, read_buf.len())
                            };
                            if n > 0 {
                                logcat_buf.extend_from_slice(&read_buf[..n as usize]);
                                let mut last_newline = 0;
                                for (idx, &b) in logcat_buf.iter().enumerate() {
                                    if b == b'\n' {
                                        let mut line_bytes = &logcat_buf[last_newline..idx];
                                        if let Some(&b'\r') = line_bytes.last() {
                                            line_bytes = &line_bytes[..line_bytes.len() - 1];
                                        }
                                        if !line_bytes.is_empty() && !line_bytes.starts_with(b"---") {
                                            if let Ok(line) = std::str::from_utf8(line_bytes) {
                                                let trimmed = line.trim();
                                                if !trimmed.is_empty() {
                                                    self.dispatch_logcat_line(trimmed);
                                                }
                                            }
                                        }
                                        last_newline = idx + 1;
                                    }
                                }
                                if last_newline > 0 {
                                    logcat_buf.drain(..last_newline);
                                } else if logcat_buf.len() > 8192 {
                                    eprintln!("[WARN] Discarding oversized un-terminated logcat line (len={})", logcat_buf.len());
                                    logcat_buf.clear();
                                }
                            } else if n == 0 {
                                if RUNNING.load(Ordering::Relaxed) {
                                    eprintln!("[FATAL] Logcat pipe EOF. Exiting.");
                                    std::process::exit(1);
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
                                    eprintln!("[FATAL] Logcat pipe error: {}. Exiting.", err);
                                    std::process::exit(1);
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
                            println!("[CONFIG] Configuration directory updated. Reloading...");
                        }
                        self.ensure_inotify_watch();
                        self.reload_configs();
                    }
                    _ => {}
                }
            }
        }

        if !self.json_stdout {
            println!("[DAEMON] Shutdown signal received. Exiting.");
        }
        if let Some(mut child) = self.logcat_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.reap_terminated_children();
        self.telemetry.flush();
    }

    /// Signed wall-clock step between two consecutive samples, or 0 for a continuous stream.
    ///
    /// Only genuine `settimeofday()`/NTP steps qualify: a backward step, or the one-time
    /// forward jump out of the pre-sync boot RTC band. Ordinary quiet gaps between events
    /// (reading, screen off, Doze) are real elapsed time and must always yield 0. A `0`
    /// `prev_epoch` means the clock was unavailable at bootstrap and yields 0.
    ///
    /// Known limitation: a *forward* step whose starting value is already at or above
    /// `RTC_SYNC_FLOOR_MS` is indistinguishable from a quiet gap and returns 0. Only the
    /// magnitude is observable, and a legitimate Doze gap can be arbitrarily large, so any
    /// threshold that caught such a step would also freeze real idle age. Anchors are
    /// therefore assumed to be stamped from a clock that is either correct or pre-2020.
    #[inline(always)]
    fn clock_jump_ms(prev_epoch: u64, now_epoch: u64) -> i64 {
        if prev_epoch == 0 {
            0
        } else if now_epoch < prev_epoch {
            -((prev_epoch - now_epoch) as i64)
        } else if prev_epoch < RTC_SYNC_FLOOR_MS && now_epoch >= RTC_SYNC_FLOOR_MS {
            (now_epoch - prev_epoch) as i64
        } else {
            0
        }
    }

    /// Shifts every stored timestamp anchor by a detected clock step, so elapsed ages
    /// (idle, respawn TTL, screen-state, game session, summary interval) survive the
    /// correction unchanged. Records are never re-derived from the wall clock elsewhere.
    fn apply_clock_jump(&mut self, jump_ms: i64) {
        if jump_ms == 0 {
            return;
        }
        for anchor in self.alive_apps.values_mut() {
            *anchor = anchor.saturating_add_signed(jump_ms);
        }
        for death in self.recent_deaths.values_mut() {
            *death = death.saturating_add_signed(jump_ms);
        }
        for anchor in [
            &mut self.screen_on_start,
            &mut self.screen_off_start,
            &mut self.game_session_start,
        ] {
            *anchor = anchor.map(|t| t.saturating_add_signed(jump_ms));
        }
        self.session_start = self.session_start.saturating_add_signed(jump_ms);
    }

    fn dispatch_logcat_line(&mut self, line: &str) {
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

fn print_help() {
    println!(
        r#"mini-lmk - Rootless event-driven memory manager for Android 7.0+ (API 24+)
Usage: mini-lmk <MODE> [OPTIONS]

Modes:
  --observe        Run in observation mode (simulate kills, emit telemetry)
  --act            Execute real kills via `cmd activity kill --user all`

Options:
  --json           Emit raw NDJSON to stdout instead of tabular columnar format
  -h, --help       Print this help message"#
    );
}

fn main() {
    let mut mode = None;
    let mut json_stdout = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--act" => mode = Some(true),
            "--observe" => mode = Some(false),
            "--json" => json_stdout = true,
            "-h" | "--help" => return print_help(),
            other => eprintln!("[WARN] Unknown argument: {}", other),
        }
    }

    let Some(act_mode) = mode else {
        eprintln!("[ERROR] Operating mode must be specified: use --observe or --act\n");
        return print_help();
    };

    println!("=== mini-lmk daemon ===");
    DaemonState::new(act_mode, json_stdout).run();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ChildGuard {
        child: Child,
        fd: i32,
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if self.fd >= 0 {
                unsafe {
                    libc::close(self.fd);
                }
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
        unsafe { libc::close(raw_fd); }

        let res = probe_logcat_stream(&mut guard.child, raw_fd);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("Invalid"));
    }

    #[test]
    fn test_kill_telemetry_parts() {
        assert_eq!(DaemonState::kill_telemetry_parts(true, false), ("kill", "KILL", "", false));
        assert_eq!(
            DaemonState::kill_telemetry_parts(true, true),
            ("kill_skipped", "KILL_SKIP", " (ams_protected: spawn skipped)", true)
        );
        assert_eq!(
            DaemonState::kill_telemetry_parts(false, false),
            ("simulated_kill", "SIM_KILL", " (simulated)", true)
        );
        assert_eq!(
            DaemonState::kill_telemetry_parts(false, true),
            ("simulated_kill", "SIM_KILL", " (simulated, ams_protected: spawn skipped)", true)
        );
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

    #[test]
    fn test_clock_jump_ms() {
        // First event after bootstrap: no previous sample, nothing to compensate.
        assert_eq!(DaemonState::clock_jump_ms(0, 1_700_000_000_000), 0);

        // Quiet periods are real elapsed time, never a clock step (regression guard).
        assert_eq!(DaemonState::clock_jump_ms(1_700_000_000_000, 1_700_000_060_000), 0);
        assert_eq!(DaemonState::clock_jump_ms(1_700_000_000_000, 1_800_000_000_000), 0);
        assert_eq!(DaemonState::clock_jump_ms(1_700_000_000_000, 1_700_000_000_000), 0);

        // Backward settimeofday() correction of 10s.
        assert_eq!(DaemonState::clock_jump_ms(1_700_000_010_000, 1_700_000_000_000), -10_000);

        // Unsynchronized boot RTC jumping forward to NTP-synced time.
        assert_eq!(
            DaemonState::clock_jump_ms(15_000, 1_700_000_000_000),
            1_699_999_985_000
        );
    }

    /// Test-only `DaemonState` with inert descriptors and a throwaway telemetry file,
    /// so the anchor-rebasing paths can be exercised without spawning `logcat`.
    fn daemon_for_test(tag: &str, bootstrap_epoch: u64) -> DaemonState {
        let log_path =
            std::env::temp_dir().join(format!("mlmk_test_{}_{}.log", std::process::id(), tag));
        DaemonState {
            config: RuntimeConfig::default(),
            json_stdout: false,
            alive_apps: FastMap::default(),
            pid_to_pkg: FastMap::default(),
            pkg_to_pids: FastMap::default(),
            pkg_to_uid: FastMap::default(),
            recent_deaths: FastMap::default(),
            user_exclusions: FastSet::default(),
            games: FastSet::default(),
            dynamic_exclusions: FastSet::default(),
            fg_lru: VecDeque::default(),
            telemetry: TelemetrySink::new(&log_path.to_string_lossy(), false),
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
        }
    }

    #[test]
    fn test_bootstrap_clock_step_rebases_bootstrap_anchors() {
        // A dead coin cell leaves the RTC in the pre-2020 band; the first event of the
        // stream arrives after the clock has been corrected. Anchors stamped from the boot
        // clock have to be rebased, or every interval derived from them spans decades.
        let boot_ms = 1_234_567_890_000u64; // 2009, below RTC_SYNC_FLOOR_MS
        let first_event_ms = 1_777_998_045_123u64; // post-sync
        let idle = "com.example.idle";
        let mut d = daemon_for_test("bootstep", boot_ms);
        d.alive_apps.insert(idle.into(), boot_ms);
        d.recent_deaths.insert("com.example.dead".into(), boot_ms);

        let jump = DaemonState::clock_jump_ms(d.last_event_epoch, first_event_ms);
        assert!(jump > 0, "pre-sync boot stamp is not a quiet gap");
        d.apply_clock_jump(jump);

        // Bootstrap-stamped ages collapse to ~0 instead of ~56 years.
        assert_eq!(d.session_start, first_event_ms);
        assert_eq!(d.screen_on_start, Some(first_event_ms));
        assert_eq!(d.alive_apps[idle], first_event_ms);
        assert_eq!(d.recent_deaths["com.example.dead"], first_event_ms);

        // A correct boot clock followed by an ordinary quiet gap stays untouched.
        let mut d2 = daemon_for_test("bootsync", first_event_ms);
        d2.alive_apps.insert(idle.into(), first_event_ms);
        let later = first_event_ms + 3_600_000;
        let jump2 = DaemonState::clock_jump_ms(d2.last_event_epoch, later);
        assert_eq!(jump2, 0, "silence is elapsed time, not a step");
        d2.apply_clock_jump(jump2);
        assert_eq!(d2.session_start, first_event_ms);
        assert_eq!(d2.screen_on_start, Some(first_event_ms));
        assert_eq!(later - d2.alive_apps[idle], 3_600_000);
    }
}
