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
use procfs::{check_mem_critical, read_statm_rss_kb, read_total_ram_mb};
use telemetry::{escape_json, format_time_hms_ms, SessionStats, TelemetrySink};

use std::collections::VecDeque;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::IntoRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn sig_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

const TOKEN_LOGCAT_PIPE: u64 = 1;
const TOKEN_INOTIFY: u64 = 2;

#[derive(Debug)]
struct AppRecord {
    pids: FastSet<u32>,
    last_active: Instant,
}

struct Candidate {
    pkg: String,
    live_pids: Vec<u32>,
    total_rss_kb: u64,
    idle_sec: u64,
    lru_pos: usize,
}

struct DaemonState {
    config: RuntimeConfig,

    alive_apps: FastMap<String, AppRecord>,
    pid_to_pkg: FastMap<u32, String>,
    user_exclusions: FastSet<String>,
    games: FastSet<String>,
    dynamic_exclusions: FastSet<String>,
    fg_lru: VecDeque<String>,

    telemetry: TelemetrySink,
    logcat_child: Option<Child>,
    current_fg: Option<String>,
    screen_on_start: Option<Instant>,
    screen_off_start: Option<Instant>,
    game_session_start: Option<Instant>,
    session_start: Instant,

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

#[inline(always)]
fn detect_initial_screen_on() -> bool {
    Command::new("/system/bin/cmd")
        .args(["deviceidle", "get", "screen"])
        .output()
        .map(|o| o.status.success() && o.stdout.starts_with(b"true"))
        .unwrap_or(true)
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

        let now = Instant::now();
        let screen_on = detect_initial_screen_on();
        let screen_on_start = if screen_on { Some(now) } else { None };
        let screen_off_start = if !screen_on { Some(now) } else { None };
        println!("[DAEMON] Initial display state: screen_on={}", screen_on);

        let total_ram_mb = read_total_ram_mb();
        let detected_default_kills = if total_ram_mb <= 4608 {
            4 // <= 4GB RAM devices (accounting for reserved kernel RAM)
        } else if total_ram_mb <= 8704 {
            2 // 6GB - 8GB RAM devices (up to 8.5GB accounting for carveouts)
        } else {
            1 // > 8.5GB RAM devices (12GB+ configurations)
        };
        println!(
            "[DAEMON] Hardware profile: total_ram={}MB (default max_kills={})",
            total_ram_mb, detected_default_kills
        );

        let mut config = RuntimeConfig::default();
        config.max_kills_per_pass = detected_default_kills;

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
            session_start: now,
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
                let abs_rss_mb = (self.session_stats.spawn_rss_kb.abs() as u64 + 512) / 1024;
                let detail = format!(
                    "interval={}s  spawns={}  deaths={}  rss_delta={}{}MB",
                    interval_sec, self.session_stats.bg_spawns, self.session_stats.bg_deaths, sign, abs_rss_mb
                );
                format!("{:<12}   {:<12} {:<26} {}", time_str, "BG_SUMMARY", "--", detail)
            },
            &json,
        );
        self.session_stats = SessionStats::default();
        self.session_start = Instant::now();
    }

    fn seed_initial_state(&mut self) {
        println!("[DAEMON] Performing cold-start discovery...");
        let now = Instant::now();
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

            if meta.uid() < 10000 {
                continue;
            }

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

                        let app = self.alive_apps.entry(pkg).or_insert_with(|| AppRecord {
                            pids: FastSet::default(),
                            last_active: now,
                        });
                        app.pids.insert(pid);
                    }
                }
            }
        }
        println!("[DAEMON] Indexed {} active app packages.", self.alive_apps.len());
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

    fn load_file_lines(path: &str, set: &mut FastSet<String>) {
        set.clear();
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    set.insert(trimmed.to_string());
                }
            }
            println!("[CONFIG] Loaded {} entries from {}", set.len(), path);
        }
    }

    fn reload_configs(&mut self) {
        let prev_cfg = self.config;
        let paths = ConfigPaths::get();
        self.config.load_from_file(&paths.config_file);
        Self::load_file_lines(&paths.exclude_file, &mut self.user_exclusions);
        Self::load_file_lines(&paths.games_file, &mut self.games);

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
                            println!("[SYSTEM] Detected {}: {}", label, pkg);
                            self.dynamic_exclusions.insert(pkg.to_string());
                        }
                    }
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
                            println!("[SYSTEM] Detected {}: {}", label, pkg);
                            self.dynamic_exclusions.insert(pkg.to_string());
                        }
                    }
                }
            }
        }
    }

    fn spawn_logcat_stream(&mut self) {
        println!("[DAEMON] Spawning unified logcat stream...");
        let mut child = match Command::new("/system/bin/logcat")
            .args([
                "-b", "events",
                "-v", "tag",
                "-s", "wm_resume_activity", "am_resume_activity", "am_proc_start", "am_proc_died", "screen_toggled", "device_idle_light_step",
                "-T", "1",
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
        println!("[DAEMON] Logcat stream active on fd={}", self.logcat_fd);
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

    fn on_resume_activity(&mut self, ev: &ResumeActivityEvent) {
        let pkg = ev.pkg;
        let component = ev.component;

        let now = Instant::now();
        let now_epoch = Self::get_epoch_ms();
        let mut prev_dur_ms = 0u64;

        let prev_pkg_opt = self.current_fg.clone();
        if let Some(ref prev_pkg) = prev_pkg_opt {
            if prev_pkg != pkg {
                if let Some(rec) = self.alive_apps.get_mut(prev_pkg) {
                    prev_dur_ms = now.duration_since(rec.last_active).as_millis() as u64;
                    rec.last_active = now;
                } else {
                    self.alive_apps.insert(prev_pkg.clone(), AppRecord {
                        pids: FastSet::default(),
                        last_active: now,
                    });
                }
            }
        }

        // Emit background summary accumulated during previous foreground window
        let interval_sec = now.duration_since(self.session_start).as_secs();
        self.emit_bg_summary(now_epoch, interval_sec);

        // If the departed package was only in foreground for < 500ms (e.g. trampoline, chooser, auth pulse),
        // evict it from fg_lru so it does not displace real user applications in the LRU protection window.
        if prev_dur_ms > 0 && prev_dur_ms < 500 {
            if let Some(ref prev) = prev_pkg_opt {
                if let Some(pos) = self.fg_lru.iter().position(|x| x == prev) {
                    self.fg_lru.remove(pos);
                }
            }
        }

        if let Some(pos) = self.fg_lru.iter().position(|x| x == pkg) {
            self.fg_lru.remove(pos);
        }
        self.fg_lru.push_front(pkg.to_string());
        if self.fg_lru.len() > self.config.fg_lru_max_depth {
            self.fg_lru.pop_back();
        }

        let was_gaming = self.is_gaming;
        let is_game = self.games.contains(pkg);
        self.is_gaming = is_game;

        if is_game && !was_gaming {
            self.game_session_start = Some(now);
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
                .map(|s| now.duration_since(s).as_secs())
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

        self.current_fg = Some(pkg.to_string());

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

        self.evaluate_reaping_pipeline();
    }

    fn on_proc_start(&mut self, ev: &ProcStartEvent) {
        let pid = ev.pid;
        let uid = ev.uid;
        let raw_proc_name = ev.proc_name;
        let spawn_type = ev.spawn_type;
        let pkg = ev.pkg;

        let now = Instant::now();
        let now_epoch = Self::get_epoch_ms();
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

        self.pid_to_pkg.insert(pid, pkg.to_string());
        self.alive_apps
            .entry(pkg.to_string())
            .or_insert_with(|| AppRecord {
                pids: FastSet::default(),
                last_active: now,
            })
            .pids
            .insert(pid);

        let is_fg_launch = is_fg || ev.spawn_type == "top-activity" || ev.spawn_type == "next-top-activity";
        if !is_fg_launch {
            self.evaluate_reaping_pipeline();
        }
    }

    fn on_proc_died(&mut self, ev: &ProcDiedEvent) {
        let pid = ev.pid;

        if let Some(pkg) = self.pid_to_pkg.remove(&pid) {
            let mut is_empty = false;
            if let Some(record) = self.alive_apps.get_mut(&pkg) {
                record.pids.remove(&pid);
                is_empty = record.pids.is_empty();
            }
            if is_empty {
                self.alive_apps.remove(&pkg);
            }
            let is_fg = self.current_fg.as_deref() == Some(&pkg);
            if !is_fg {
                self.session_stats.bg_deaths += 1;
            }
        }
    }

    fn on_screen_toggled(&mut self, state: bool) {
        if self.screen_on == state {
            return;
        }

        let now = Instant::now();
        let now_epoch = Self::get_epoch_ms();
        if !state {
            let active_sec = self.screen_on_start
                .map(|s| now.duration_since(s).as_secs())
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
            self.screen_off_start = Some(now);
        } else {
            let duration_sec = self.screen_off_start
                .map(|s| now.duration_since(s).as_secs())
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
            self.screen_on_start = Some(now);
            self.screen_off_start = None;
        }
    }

    fn evaluate_reaping_pipeline(&mut self) {
        let now = Instant::now();
        let now_epoch = Self::get_epoch_ms();

        let is_game = self.current_fg.as_ref().map(|p| self.games.contains(p)).unwrap_or(false);
        let is_low_mem = check_mem_critical(self.config.mem_critical_percent);

        let effective_lru_depth = if !self.screen_on && self.config.screen_off_harvest {
            1
        } else {
            self.config.lru_protect_depth
        };

        let t_idle_effective = if is_game || is_low_mem {
            Duration::ZERO
        } else if !self.screen_on && self.config.screen_off_harvest {
            let off_dur = self
                .screen_off_start
                .map(|s| now.duration_since(s))
                .unwrap_or_default();
            if off_dur > Duration::from_secs(60) {
                Duration::from_secs(30)
            } else {
                Duration::from_secs(60)
            }
        } else {
            let fg_dur = self
                .current_fg
                .as_deref()
                .and_then(|p| self.alive_apps.get(p))
                .map(|r| now.duration_since(r.last_active))
                .unwrap_or_default();
            if fg_dur > Duration::from_secs(300) {
                Duration::from_secs(60)
            } else {
                Duration::from_secs(self.config.t_idle_sec)
            }
        };

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut dead_pids = Vec::new();
        let mut dead_pkgs = Vec::new();

        for (pkg, record) in &self.alive_apps {
            if self.is_excluded(pkg) {
                continue;
            }

            let lru_pos = self.fg_lru.iter().position(|x| x == pkg).unwrap_or(99);
            if lru_pos < effective_lru_depth {
                continue;
            }

            let idle_duration = now.duration_since(record.last_active);
            if idle_duration < t_idle_effective {
                continue;
            }

            let mut total_rss_kb = 0u64;
            let mut live_pids = Vec::new();
            for &pid in &record.pids {
                let rss = read_statm_rss_kb(pid, self.page_size_kb);
                if rss > 0 {
                    total_rss_kb += rss;
                    live_pids.push(pid);
                } else {
                    dead_pids.push(pid);
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
                idle_sec: idle_duration.as_secs(),
                lru_pos,
            });
        }

        // Opportunistic reconciliation: purge dead PIDs and exited packages
        for pid in &dead_pids {
            self.pid_to_pkg.remove(pid);
        }
        for pkg in &dead_pkgs {
            self.alive_apps.remove(pkg);
        }
        for cand in &candidates {
            if let Some(record) = self.alive_apps.get_mut(&cand.pkg) {
                record.pids.retain(|p| cand.live_pids.contains(p));
            }
        }

        candidates.sort_by(|a, b| b.total_rss_kb.cmp(&a.total_rss_kb));

        for cand in candidates.into_iter().take(self.config.max_kills_per_pass) {
            let reason = if is_game {
                "game_mode_escalation"
            } else if is_low_mem {
                "low_memory_escalation"
            } else {
                "idle_expired"
            };

            let rss_mb = cand.total_rss_kb / 1024;

            let is_spawned = if self.act_mode {
                Some(
                    Command::new("/system/bin/cmd")
                        .args(["activity", "kill", "--user", "all", &cand.pkg])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .is_ok(),
                )
            } else {
                None
            };

            let (event_name, tag, sim_suffix) = match is_spawned {
                Some(_) => ("kill", "KILL", ""),
                None => ("simulated_kill", "SIM_KILL", " (simulated)"),
            };

            let json = match is_spawned {
                Some(spawned) => format!(
                    r#"{{"ts":{},"event":"{}","pkg":"{}","pids":{:?},"rss_freed_est_kb":{},"reason":"{}","idle_sec":{},"lru_pos":{},"spawned":{}}}"#,
                    now_epoch, event_name, escape_json(&cand.pkg), cand.live_pids, cand.total_rss_kb, reason, cand.idle_sec, cand.lru_pos, spawned
                ),
                None => format!(
                    r#"{{"ts":{},"event":"{}","pkg":"{}","pids":{:?},"rss_freed_est_kb":{},"reason":"{}","idle_sec":{},"lru_pos":{}}}"#,
                    now_epoch, event_name, escape_json(&cand.pkg), cand.live_pids, cand.total_rss_kb, reason, cand.idle_sec, cand.lru_pos
                ),
            };

            self.telemetry.emit_with(
                || {
                    let time_str = format_time_hms_ms(now_epoch);
                    let detail = format!("rss={}MB  idle={}s  lru={} [{}] {}", rss_mb, cand.idle_sec, cand.lru_pos, reason, sim_suffix);
                    format!("{:<12}   {:<12} {:<26} {}", time_str, tag, cand.pkg, detail)
                },
                &json,
            );
            self.telemetry.flush();

            self.alive_apps.remove(&cand.pkg);
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
        println!(
            "[DAEMON] mini-lmk active (mode: {}).",
            if self.act_mode { "ACT" } else { "OBSERVE" }
        );
        println!("[DAEMON] Monitoring FDs: [TOKEN_LOGCAT_PIPE, TOKEN_INOTIFY]");

        if !self.telemetry.json_stdout {
            println!("{:<12}   {:<12} {:<26} {}", "# TIME", "EVENT", "TARGET", "DETAIL / REASON");
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

            // Non-blocking reap of any background child processes (e.g. cmd activity kill)
            self.reap_terminated_children();

            for i in 0..nfds as usize {
                let token = events[i].u64;
                let revents = events[i].events;

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
                        println!("[CONFIG] Configuration directory updated. Reloading...");
                        self.ensure_inotify_watch();
                        self.reload_configs();
                    }
                    _ => {}
                }
            }
        }

        println!("[DAEMON] Shutdown signal received. Exiting.");
        if let Some(mut child) = self.logcat_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.reap_terminated_children();
        self.telemetry.flush();
    }

    fn dispatch_logcat_line(&mut self, line: &str) {
        if let Some(event) = parse_logcat_line(line) {
            match event {
                LogcatEvent::ResumeActivity(ev) => self.on_resume_activity(&ev),
                LogcatEvent::ProcStart(ev) => self.on_proc_start(&ev),
                LogcatEvent::ProcDied(ev) => self.on_proc_died(&ev),
                LogcatEvent::ScreenToggled(state) => self.on_screen_toggled(state),
                LogcatEvent::DeviceIdleLightStep => {
                    if !self.screen_on && self.config.screen_off_harvest {
                        self.evaluate_reaping_pipeline();
                    }
                }
            }
        }
    }
}

fn main() {
    let mut act_mode = false;
    let mut json_stdout = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--act" => act_mode = true,
            "--observe" => act_mode = false,
            "--json" => json_stdout = true,
            "-h" | "--help" => {
                println!("mini-lmk - Rootless event-driven memory manager for Android 10+ (API 29+)");
                println!("Usage: mini-lmk [OPTIONS]\n");
                println!("Options:");
                println!("  --observe        Run in observation mode (simulate kills, emit telemetry) [default]");
                println!("  --act            Execute real kills via `cmd activity kill --user all`");
                println!("  --json           Emit raw NDJSON to stdout instead of tabular columnar format");
                println!("  -h, --help       Print this help message");
                return;
            }
            other => eprintln!("[WARN] Unknown argument: {}", other),
        }
    }

    println!("=== mini-lmk daemon ===");
    DaemonState::new(act_mode, json_stdout).run();
}
