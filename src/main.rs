use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn sig_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

const TOKEN_LOGCAT_PIPE: u64 = 1;
const TOKEN_INOTIFY: u64 = 2;
const TOKEN_RECONNECT_TIMER: u64 = 3;

const MLMK_DIR: &str = "/data/local/tmp/mlmk";
const EXCLUDE_FILE: &str = "/data/local/tmp/mlmk/exclude.list";
const GAMES_FILE: &str = "/data/local/tmp/mlmk/games.list";
const TELEMETRY_LOG: &str = "/data/local/tmp/mlmk/telemetry.log";

#[allow(dead_code)]
#[derive(Debug)]
struct ProcRecord {
    pid: u32,
    uid: u32,
    pkg: String,
    spawn_type: String,
    start_time: Instant,
    initial_rss_kb: u64,
}

struct TelemetryDaemon {
    epoll_fd: i32,
    logcat_child: Option<Child>,
    logcat_fd: i32,
    inotify_fd: i32,
    reconnect_timer_fd: i32,

    // Config & exclusions
    exclude_list: HashSet<String>,
    games_list: HashSet<String>,
    dynamic_system_exclusions: HashSet<String>,

    // Tracking state
    current_fg: Option<String>,
    fg_lru: VecDeque<String>,
    app_idle_tracker: HashMap<String, Instant>,
    active_procs: HashMap<u32, ProcRecord>,

    // Screen state
    screen_on: bool,
    screen_off_start: Option<Instant>,
    screen_off_spawns: u32,
    screen_off_rss_accum_kb: u64,

    // Gaming state
    is_gaming: bool,
    game_session_start: Option<Instant>,
    game_intrusion_count: u32,

    // Telemetry log sink
    log_file: Option<File>,
}

impl TelemetryDaemon {
    fn new() -> Self {
        unsafe {
            libc::signal(libc::SIGINT, sig_handler as *const () as usize);
            libc::signal(libc::SIGTERM, sig_handler as *const () as usize);
        }

        let _ = fs::create_dir_all(MLMK_DIR);

        let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll_fd < 0 {
            panic!("epoll_create1 failed: {}", std::io::Error::last_os_error());
        }

        // Inotify setup
        let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify_fd >= 0 {
            let dir_c = CString::new(MLMK_DIR).unwrap();
            let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_DELETE;
            let wd = unsafe { libc::inotify_add_watch(inotify_fd, dir_c.as_ptr(), mask) };
            if wd < 0 {
                eprintln!("[WARN] inotify_add_watch failed: {}", std::io::Error::last_os_error());
            } else {
                let mut ev = libc::epoll_event {
                    events: libc::EPOLLIN as u32,
                    u64: TOKEN_INOTIFY,
                };
                unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, inotify_fd, &mut ev) };
            }
        }

        // Reconnect timerfd (one-shot 1s)
        let reconnect_timer_fd = unsafe {
            libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC)
        };
        if reconnect_timer_fd >= 0 {
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: TOKEN_RECONNECT_TIMER,
            };
            unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, reconnect_timer_fd, &mut ev) };
        }

        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(TELEMETRY_LOG)
            .ok();

        let mut daemon = Self {
            epoll_fd,
            logcat_child: None,
            logcat_fd: -1,
            inotify_fd,
            reconnect_timer_fd,

            exclude_list: HashSet::new(),
            games_list: HashSet::new(),
            dynamic_system_exclusions: HashSet::new(),

            current_fg: None,
            fg_lru: VecDeque::with_capacity(16),
            app_idle_tracker: HashMap::new(),
            active_procs: HashMap::new(),

            screen_on: true,
            screen_off_start: None,
            screen_off_spawns: 0,
            screen_off_rss_accum_kb: 0,

            is_gaming: false,
            game_session_start: None,
            game_intrusion_count: 0,

            log_file,
        };

        daemon.reload_exclusions();
        daemon.reload_games();
        daemon.detect_system_components();
        daemon.spawn_logcat_stream();

        daemon
    }

    fn reload_exclusions(&mut self) {
        self.exclude_list.clear();
        match fs::read_to_string(EXCLUDE_FILE) {
            Ok(content) => {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !trimmed.starts_with('#') {
                        self.exclude_list.insert(trimmed.to_string());
                    }
                }
                println!("[CONFIG] Loaded {} exclusion entries from {}", self.exclude_list.len(), EXCLUDE_FILE);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("[WARN] {} missing (defaulting to empty exclusions)", EXCLUDE_FILE);
            }
            Err(e) => {
                eprintln!("[ERROR] Unreadable {}: {}", EXCLUDE_FILE, e);
            }
        }
    }

    fn reload_games(&mut self) {
        self.games_list.clear();
        match fs::read_to_string(GAMES_FILE) {
            Ok(content) => {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !trimmed.starts_with('#') {
                        self.games_list.insert(trimmed.to_string());
                    }
                }
                println!("[CONFIG] Loaded {} games entries from {}", self.games_list.len(), GAMES_FILE);
            }
            Err(_) => {
                // games.list is optional
            }
        }
    }

    fn detect_system_components(&mut self) {
        self.dynamic_system_exclusions.clear();

        // 1. Roles via RoleManager (cmd role)
        let roles = [
            ("HOME", "android.app.role.HOME"),
            ("DIALER", "android.app.role.DIALER"),
            ("SMS", "android.app.role.SMS"),
        ];

        for (label, role) in roles {
            if let Ok(o) = Command::new("cmd").args(["role", "get-role-holders", role]).output() {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    let pkg = line.trim();
                    if !pkg.is_empty() {
                        println!("[SYSTEM] Detected {}: {}", label, pkg);
                        self.dynamic_system_exclusions.insert(pkg.to_string());
                    }
                }
            }
        }

        // 2. Active IME via cmd settings
        if let Ok(o) = Command::new("cmd").args(["settings", "get", "secure", "default_input_method"]).output() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if let Some(pkg) = stdout.trim().split('/').next() {
                if !pkg.is_empty() && pkg != "null" {
                    println!("[SYSTEM] Detected IME: {}", pkg);
                    self.dynamic_system_exclusions.insert(pkg.to_string());
                }
            }
        }

        // 3. Active Live Wallpaper via cmd settings
        if let Ok(o) = Command::new("cmd").args(["settings", "get", "secure", "wallpaper_service"]).output() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let s = stdout.trim();
            if !s.is_empty() && s != "null" {
                if let Some(pkg) = s.split('/').next() {
                    println!("[SYSTEM] Detected Live Wallpaper: {}", pkg);
                    self.dynamic_system_exclusions.insert(pkg.to_string());
                }
            }
        }
    }

    fn spawn_logcat_stream(&mut self) {
        if self.logcat_fd >= 0 {
            unsafe {
                libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_DEL, self.logcat_fd, std::ptr::null_mut());
                libc::close(self.logcat_fd);
            }
            self.logcat_fd = -1;
        }
        if let Some(mut child) = self.logcat_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        println!("[DAEMON] Spawning unified logcat events stream...");
        let mut child = match Command::new("logcat")
            .args([
                "-b", "events",
                "-v", "tag",
                "-s", "wm_resume_activity", "am_proc_start", "am_proc_died", "screen_toggled",
                "-T", "1",
            ])
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[ERROR] Failed to spawn logcat: {}", e);
                self.arm_reconnect_timer();
                return;
            }
        };

        let stdout = child.stdout.take().expect("logcat stdout");
        let raw_fd = stdout.as_raw_fd();
        // Make non-blocking
        unsafe {
            let flags = libc::fcntl(raw_fd, libc::F_GETFL, 0);
            libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR) as u32,
            u64: TOKEN_LOGCAT_PIPE,
        };
        if unsafe { libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, raw_fd, &mut ev) } < 0 {
            eprintln!("[ERROR] epoll_ctl add logcat failed: {}", std::io::Error::last_os_error());
            self.arm_reconnect_timer();
            return;
        }

        self.logcat_fd = raw_fd;
        self.logcat_child = Some(child);
        std::mem::forget(stdout); // Do not close on drop, managed manually
        println!("[DAEMON] Logcat stream active on fd={}", self.logcat_fd);
    }

    fn arm_reconnect_timer(&self) {
        if self.reconnect_timer_fd >= 0 {
            let spec = libc::itimerspec {
                it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
                it_value: libc::timespec { tv_sec: 1, tv_nsec: 0 },
            };
            unsafe { libc::timerfd_settime(self.reconnect_timer_fd, 0, &spec, std::ptr::null_mut()) };
            println!("[DAEMON] Armed 1s reconnect timer");
        }
    }

    fn log_telemetry(&mut self, json_obj: &str) {
        println!("{}", json_obj);
        if let Some(ref mut f) = self.log_file {
            let _ = writeln!(f, "{}", json_obj);
            let _ = f.flush();
        }
    }

    fn get_epoch_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn read_statm_rss_kb(pid: u32) -> u64 {
        if let Ok(statm) = fs::read_to_string(format!("/proc/{}/statm", pid)) {
            let parts: Vec<&str> = statm.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(pages) = parts[1].parse::<u64>() {
                    return pages * 4; // 4096 / 1024 = 4KB per page
                }
            }
        }
        0
    }

    fn read_oom_score_adj(pid: u32) -> i32 {
        if let Ok(adj_str) = fs::read_to_string(format!("/proc/{}/oom_score_adj", pid)) {
            if let Ok(adj) = adj_str.trim().parse::<i32>() {
                return adj;
            }
        }
        -1000
    }

    fn is_excluded(&self, pkg: &str) -> bool {
        if self.exclude_list.contains(pkg) {
            return true;
        }
        if self.dynamic_system_exclusions.contains(pkg) {
            return true;
        }
        if let Some(ref cur) = self.current_fg {
            if cur == pkg {
                return true;
            }
        }
        false
    }

    // Handlers
    fn on_resume_activity(&mut self, payload: &str) {
        // Format: [user_id, token, task_id, component]
        // E.g.: [0,27804760,240,com.android.settings/.Settings$StorageUseActivity]
        let inner = payload.trim_start_matches('[').trim_end_matches(']');
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() < 4 {
            return;
        }
        let component = parts[3].trim();
        let pkg = match component.split('/').next() {
            Some(p) => p.trim(),
            None => component,
        };

        if pkg.is_empty() {
            return;
        }

        let now = Instant::now();
        let now_epoch = Self::get_epoch_ms();

        // Check previous foreground
        let prev_fg_duration_ms = if let Some(ref prev) = self.current_fg {
            if prev != pkg {
                let dur = self.app_idle_tracker.insert(prev.clone(), now)
                    .map(|t| now.duration_since(t).as_millis() as u64)
                    .unwrap_or(0);
                dur
            } else {
                0
            }
        } else {
            0
        };

        // Update FG LRU
        if let Some(pos) = self.fg_lru.iter().position(|x| x == pkg) {
            self.fg_lru.remove(pos);
        }
        self.fg_lru.push_front(pkg.to_string());
        if self.fg_lru.len() > 10 {
            self.fg_lru.pop_back();
        }

        let was_gaming = self.is_gaming;
        let is_game = self.games_list.contains(pkg);
        self.is_gaming = is_game;

        if is_game && !was_gaming {
            self.game_session_start = Some(now);
            self.game_intrusion_count = 0;
            self.log_telemetry(&format!(
                r#"{{"ts":{},"event":"game_session_start","pkg":"{}"}}"#,
                now_epoch, pkg
            ));
        } else if !is_game && was_gaming {
            let duration_sec = self.game_session_start
                .map(|s| now.duration_since(s).as_secs())
                .unwrap_or(0);
            self.log_telemetry(&format!(
                r#"{{"ts":{},"event":"game_session_end","duration_sec":{},"intrusions":{}}}"#,
                now_epoch, duration_sec, self.game_intrusion_count
            ));
            self.game_session_start = None;
        }

        self.current_fg = Some(pkg.to_string());

        self.log_telemetry(&format!(
            r#"{{"ts":{},"event":"fg_switch","pkg":"{}","component":"{}","prev_dur_ms":{},"is_game":{}}}"#,
            now_epoch, pkg, component, prev_fg_duration_ms, is_game
        ));

        // Evaluate Simulated Kills
        self.evaluate_simulated_kills();
    }

    fn on_proc_start(&mut self, payload: &str) {
        // Format: [user_id, pid, uid, process_name, spawn_type, {component}]
        // E.g.: [0,12763,10130,com.google.android.calculator,next-top-activity,{...}]
        let inner = payload.trim_start_matches('[').trim_end_matches(']');
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() < 5 {
            return;
        }

        let pid: u32 = match parts[1].trim().parse() {
            Ok(p) => p,
            Err(_) => return,
        };
        let uid: u32 = parts[2].trim().parse().unwrap_or(0);
        let process_name = parts[3].trim().to_string();
        let spawn_type = parts[4].trim().to_string();

        let now = Instant::now();
        let now_epoch = Self::get_epoch_ms();
        let initial_rss_kb = Self::read_statm_rss_kb(pid);

        // Screen-off accounting
        if !self.screen_on {
            self.screen_off_spawns += 1;
            self.screen_off_rss_accum_kb += initial_rss_kb;
        }

        // Game intrusion accounting
        if self.is_gaming && spawn_type != "top-activity" && spawn_type != "next-top-activity" {
            self.game_intrusion_count += 1;
            let excluded = self.is_excluded(&process_name);
            self.log_telemetry(&format!(
                r#"{{"ts":{},"event":"game_intrusion","pid":{},"uid":{},"pkg":"{}","type":"{}","rss_kb":{},"excluded":{}}}"#,
                now_epoch, pid, uid, process_name, spawn_type, initial_rss_kb, excluded
            ));
            if !excluded {
                self.log_telemetry(&format!(
                    r#"{{"ts":{},"event":"simulated_kill","pkg":"{}","pid":{},"reason":"game_spawn_intercept","rss_freed_est_kb":{}}}"#,
                    now_epoch, process_name, pid, initial_rss_kb
                ));
            }
        }

        self.log_telemetry(&format!(
            r#"{{"ts":{},"event":"proc_start","pid":{},"uid":{},"pkg":"{}","type":"{}","rss_kb":{},"screen_on":{}}}"#,
            now_epoch, pid, uid, process_name, spawn_type, initial_rss_kb, self.screen_on
        ));

        self.active_procs.insert(pid, ProcRecord {
            pid,
            uid,
            pkg: process_name,
            spawn_type,
            start_time: now,
            initial_rss_kb,
        });
    }

    fn on_proc_died(&mut self, payload: &str) {
        // Format: [user_id, pid, process_name, adj, reason]
        // E.g.: [0,10857,com.shopeepay.id:CoreService,975,19]
        let inner = payload.trim_start_matches('[').trim_end_matches(']');
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() < 5 {
            return;
        }

        let pid: u32 = match parts[1].trim().parse() {
            Ok(p) => p,
            Err(_) => return,
        };
        let process_name = parts[2].trim();
        let adj: i32 = parts[3].trim().parse().unwrap_or(-1000);
        let reason = parts[4].trim();

        let now_epoch = Self::get_epoch_ms();
        let lifespan_ms = self.active_procs.remove(&pid)
            .map(|r| r.start_time.elapsed().as_millis() as u64)
            .unwrap_or(0);

        self.log_telemetry(&format!(
            r#"{{"ts":{},"event":"proc_died","pid":{},"pkg":"{}","adj":{},"reason":"{}","lifespan_ms":{}}}"#,
            now_epoch, pid, process_name, adj, reason, lifespan_ms
        ));
    }

    fn on_screen_toggled(&mut self, payload: &str) {
        // Payload: 0 or 1
        let state = payload.trim();
        let now = Instant::now();
        let now_epoch = Self::get_epoch_ms();

        if state == "0" {
            self.screen_on = false;
            self.screen_off_start = Some(now);
            self.screen_off_spawns = 0;
            self.screen_off_rss_accum_kb = 0;
            self.log_telemetry(&format!(
                r#"{{"ts":{},"event":"screen_state","state":"OFF"}}"#,
                now_epoch
            ));
        } else if state == "1" {
            let duration_sec = self.screen_off_start
                .map(|s| now.duration_since(s).as_secs())
                .unwrap_or(0);
            self.log_telemetry(&format!(
                r#"{{"ts":{},"event":"screen_state","state":"ON","off_duration_sec":{},"bg_spawns":{},"rss_accum_kb":{}}}"#,
                now_epoch, duration_sec, self.screen_off_spawns, self.screen_off_rss_accum_kb
            ));
            self.screen_on = true;
            self.screen_off_start = None;
        }
    }

    fn evaluate_simulated_kills(&mut self) {
        let now = Instant::now();
        let now_epoch = Self::get_epoch_ms();

        // Scan candidate processes
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let name = fname.to_string_lossy();
                let pid: u32 = match name.parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.uid() < 10000 {
                    continue;
                }

                let adj = Self::read_oom_score_adj(pid);
                if adj < 900 {
                    continue;
                }

                let cmdline = match fs::read(format!("/proc/{}/cmdline", pid)) {
                    Ok(b) => String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string(),
                    Err(_) => continue,
                };
                let pkg = match cmdline.split_whitespace().next() {
                    Some(p) => p.split(':').next().unwrap_or(p),
                    None => continue,
                };

                if self.is_excluded(pkg) {
                    continue;
                }

                // Check LRU position
                let lru_pos = self.fg_lru.iter().position(|x| x == pkg).unwrap_or(99);
                if lru_pos <= 3 {
                    continue; // Protected
                }

                // Check idle time
                let idle_dur_sec = self.app_idle_tracker.get(pkg)
                    .map(|t| now.duration_since(*t).as_secs())
                    .unwrap_or(9999);

                if idle_dur_sec >= 180 { // 3 minutes
                    let rss_kb = Self::read_statm_rss_kb(pid);
                    self.log_telemetry(&format!(
                        r#"{{"ts":{},"event":"simulated_kill","pkg":"{}","pid":{},"adj":{},"rss_freed_est_kb":{},"reason":"idle_expired","idle_sec":{},"lru_pos":{}}}"#,
                        now_epoch, pkg, pid, adj, rss_kb, idle_dur_sec, lru_pos
                    ));
                }
            }
        }
    }

    fn run(&mut self) {
        println!("[DAEMON] Telemetry daemon running. Press Ctrl+C to stop.");
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
        let mut logcat_buf = Vec::with_capacity(4096);
        let mut line_buf = [0u8; 1024];

        while RUNNING.load(Ordering::Relaxed) {
            let nfds = unsafe {
                libc::epoll_wait(self.epoll_fd, events.as_mut_ptr(), events.len() as i32, 1000)
            };
            if nfds < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                eprintln!("[ERROR] epoll_wait: {}", err);
                break;
            }

            for i in 0..nfds as usize {
                let token = events[i].u64;
                let revents = events[i].events;

                match token {
                    TOKEN_LOGCAT_PIPE => {
                        if revents & (libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0 {
                            println!("[DAEMON] Logcat pipe hung up or error");
                            self.spawn_logcat_stream();
                            continue;
                        }

                        // Drain non-blocking read
                        loop {
                            let n = unsafe {
                                libc::read(self.logcat_fd, line_buf.as_mut_ptr() as *mut libc::c_void, line_buf.len())
                            };
                            if n > 0 {
                                logcat_buf.extend_from_slice(&line_buf[..n as usize]);
                                // Process complete lines
                                while let Some(pos) = logcat_buf.iter().position(|&b| b == b'\n') {
                                    let line_bytes: Vec<u8> = logcat_buf.drain(..=pos).collect();
                                    let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
                                    if !line.is_empty() && !line.starts_with("---") {
                                        self.dispatch_logcat_line(&line);
                                    }
                                }
                            } else if n < 0 {
                                let err = std::io::Error::last_os_error();
                                if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                                    break; // Drained
                                } else {
                                    eprintln!("[ERROR] read logcat: {}", err);
                                    self.spawn_logcat_stream();
                                    break;
                                }
                            } else {
                                // EOF
                                println!("[DAEMON] Logcat EOF received");
                                self.spawn_logcat_stream();
                                break;
                            }
                        }
                    }
                    TOKEN_INOTIFY => {
                        let mut inotify_buf = [0u8; 1024];
                        let _ = unsafe {
                            libc::read(self.inotify_fd, inotify_buf.as_mut_ptr() as *mut libc::c_void, inotify_buf.len())
                        };
                        println!("[CONFIG] Inotify trigger on {}", MLMK_DIR);
                        self.reload_exclusions();
                        self.reload_games();
                    }
                    TOKEN_RECONNECT_TIMER => {
                        let mut expirations = 0u64;
                        let _ = unsafe {
                            libc::read(self.reconnect_timer_fd, &mut expirations as *mut u64 as *mut libc::c_void, 8)
                        };
                        println!("[DAEMON] Reconnect timer fired");
                        self.spawn_logcat_stream();
                    }
                    _ => {}
                }
            }
        }

        println!("[DAEMON] Shutting down cleanly...");
        if let Some(mut child) = self.logcat_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if self.logcat_fd >= 0 {
            unsafe { libc::close(self.logcat_fd) };
        }
        if self.inotify_fd >= 0 {
            unsafe { libc::close(self.inotify_fd) };
        }
        if self.reconnect_timer_fd >= 0 {
            unsafe { libc::close(self.reconnect_timer_fd) };
        }
        unsafe { libc::close(self.epoll_fd) };
        println!("[DAEMON] Shutdown complete.");
    }

    fn dispatch_logcat_line(&mut self, line: &str) {
        // Expected format: I/<tag>: <payload>
        if let Some(rest) = line.strip_prefix("I/") {
            if let Some(idx) = rest.find(':') {
                let tag = &rest[..idx];
                let payload = rest[idx + 1..].trim();
                match tag {
                    "wm_resume_activity" => self.on_resume_activity(payload),
                    "am_proc_start" => self.on_proc_start(payload),
                    "am_proc_died" => self.on_proc_died(payload),
                    "screen_toggled" => self.on_screen_toggled(payload),
                    _ => {}
                }
            }
        }
    }
}

fn main() {
    println!("=== MINI LMK TELEMETRY DAEMON (mlmk-telemetry) ===");
    let mut daemon = TelemetryDaemon::new();
    daemon.run();
}
