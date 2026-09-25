# Changelog

All notable changes to `mini-lmk` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [1.1.1] - 2026-09-25

### Fixed
- **Eviction Candidate Starvation & Telemetry Visibility:**
  - Eviction pass now calculates the minimum `oom_score_adj` across all live PIDs belonging to a candidate package. If any PID has an `adj` below `min_oom_score_adj`, the process spawn (`cmd activity kill`) is skipped to eliminate wasted fork/exec overhead and AMS rejections.
  - Preserved full telemetry visibility across `--observe` and `--act` modes (`ams_protected: true`, `spawn_skipped: true`) with explicit tabular tagging (`KILL_SKIP` in act mode, `(simulated, ams_protected: spawn skipped)` in observe mode).
  - Protected packages refresh their `last_active` timestamp in `alive_apps`, eliminating head-of-line blocking for subsequent candidates while maintaining a periodic audit heartbeat every `T_idle`.
- **Resilient Event Log Parsing (`parse_proc_died` & `parse_resume_activity`):**
  - Eliminated naive fallback loop in `parse_proc_died` that could misparse positive `oom_adj` scores (e.g. `900` or `100`) as dead process PIDs. Enforced strict adjacent process name verification (`is_proc_name`).
  - Hardened `parse_resume_activity` against enclosing curly braces, whitespace, and complex Intent payloads with flags (`cmp=pkg/act`), ensuring clean package extraction with zero heap allocation.

### Added
- **Zero-Allocation Pipeline Startup Probe (`probe_logcat_stream`):**
  - Added sub-millisecond non-blocking health check using `waitpid` and zero-timeout `poll` to validate child process survival and pipe descriptors immediately upon spawning `logcat`.
- **Toolchain-Agnostic Build & CI Configuration:**
  - Migrated `.cargo/config.toml` to linkers resolved via `$PATH` and removed forced target setting, enabling seamless out-of-the-box host testing (`cargo test`).
  - Added `.cargo/config.toml.example` reference template for custom NDK setups.
  - Updated GitHub Actions CI to dynamically extract and link `CHANGELOG.md` directly in release notes.
- **Standalone Microbenchmark Suite:**
  - Added `benches/microbench.rs` and `scripts/benchmark.sh` covering `parse_proc_start`, `parse_proc_died`, `read_oom_score_adj`, and timestamp formatting.

---

## [1.1.0] - 2026-09-25

### Fixed
- **Synchronized Boot Eviction Cascade (t = 180s Bug):**
  - Resolved regression where ambient system and cold-start background processes discovered at daemon startup were inserted into `alive_apps` with an artificial timestamp, causing a synchronized mass simulated kill pass at t = 180s.
  - `seed_initial_state()` is now strictly index-only (`pid_to_pkg`, `pkg_to_pids`, `pkg_to_uid`); cold-start processes are never inserted into `alive_apps` until legitimate foreground user activity occurs.
- **Fail-Closed UID Verification Guard:**
  - Empirically verified on live hardware that Android's Activity Manager Service (AMS) terminates cached low-UID system packages (`uid < 10000`, e.g., KeyChain and vendor test daemons) without restriction when targeted by `cmd activity kill`.
  - Enforced fail-closed UID checks: only packages with verified `uid >= 10000` (resolved in-memory from `pkg_to_uid`) may enter `alive_apps`. Unresolved or system UIDs fail closed and are never evicted.
- **Multi-PID Eviction Protection:**
  - Fixed safety vulnerability where multi-process applications were evaluated solely on whichever PID was indexed first.
  - Eviction pass evaluates the minimum `oom_score_adj` across all live PIDs belonging to a candidate package.

### Added
- **Zero-Allocation Pipeline Startup Probe (`probe_logcat_stream`):**
  - Added sub-millisecond non-blocking health check using `waitpid` and zero-timeout `poll` to validate child process survival and pipe descriptors immediately upon spawning `logcat`.
- **Toolchain-Agnostic Build & CI Configuration:**
  - Migrated `.cargo/config.toml` to linkers resolved via `$PATH` and removed forced target setting, enabling seamless out-of-the-box host testing (`cargo test`).
  - Added `.cargo/config.toml.example` reference template for custom NDK setups.
  - Updated GitHub Actions CI to natively utilize project `.cargo` flags and link `CHANGELOG.md` directly in release notes.
- **Configurable OOM Score Eviction Threshold (`min_oom_score_adj`):**
  - Added live-tunable parameter `min_oom_score_adj` in `daemon.conf` (validated and clamped to `500..=900`, defaulting to `900`).
  - Enables user and profile tuning between conservative reclamation (`900`, cached/idle processes only) and aggressive reclamation (`500`, reclaiming background services matching AOSP `SERVICE_ADJ`).
  - Supports live reload via `inotify` without restarting the daemon.
- **Twin Reverse Indexing (`pkg_to_pids`):**
  - Introduced in-memory reverse map `pkg_to_pids: FastMap<String, FastSet<u32>>` alongside `pid_to_pkg`.
  - Maintained across cold-start discovery, `am_proc_start`, and `am_proc_died` for O(1) package-to-PID resolution, eliminating O(N) linear process table scans.
- **Kernel-Direct OOM Score Inspection (`read_oom_score_adj`):**
  - Added direct VFS reader querying `/proc/<pid>/oom_score_adj` directly from kernel `task_struct`.
  - Operates non-blocking with single-digit microsecond latency (P50 = 3.31 µs, P95 = 6.69 µs across 10,000 samples).
- **Rapid Respawn Tracking:**
  - Implemented telemetry detection for rapid process churn (`proc_died` followed by `proc_start` within 120 seconds).
  - Emits structured `RESPAWN` events to assist in identifying thrashing services, with automatic TTL-based table pruning.

### Changed
- **CLI Interface & Argument Modernization:**
  - Enforced explicit CLI argument semantics: invoking with empty arguments, `-h`, or `--help` prints the help manual and exits cleanly.
  - Removed implicit default to observe mode; requires explicit `--observe` or `--act`.
  - Replaced heap-allocating argument vector parsing with zero-allocation direct iterator parsing and let-else bindings.
- **Telemetry Formatting:**
  - Added observed `oom_score_adj` directly into NDJSON (`"oom_score_adj": <val>`) and columnar table logs.
  - Guaranteed immediate disk visibility by enforcing line flushing on telemetry writes.

---

## [1.0.1] - 2026-09-24

### Fixed
- **Daemon Environment & Permissions:**
  - Added dynamic `MODPATH` discovery for AxManager plugin environments with fallback to `/data/local/tmp/mlmk`.
  - Enforced non-root permissions fixing (`chmod 755` for daemon binaries and scripts, `chmod 666` for log files and config paths).
- **Signal Handling & Reactor Shutdown:**
  - Hardened POSIX signal handling for `SIGINT` and `SIGTERM` using `sa_flags = 0` (no `SA_RESTART`) to ensure `epoll_wait` unblocks immediately with `EINTR` upon termination signals.
  - Handled broken pipe (`SIGPIPE`) by explicitly setting `SIG_IGN`.

### Added
- **Dynamic Configuration Watching:**
  - Integrated Linux `inotify` descriptor into the `epoll` reactor monitoring `/data/local/tmp/mlmk/config/`.
  - Supports hot-reloading `exclude.list` and `games.list` without daemon restarts.
- **Telemetry Rotation:**
  - Added size-capped telemetry log rotation (`operations.log.old`) to prevent unbounded disk usage in userspace environments.

### Documentation
- Removed LaTeX math syntax in documentation for GitHub/markdown viewer compatibility.
- Updated runtime tuning and display toggling specifications in technical reference.

---

## [1.0.0] - 2026-09-24

### Added
- **Initial Production Release:**
  - Event-driven, rootless userspace memory manager for Android 10+ (API 29+).
  - Multi-ABI target support: `aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android`, and `i686-linux-android`.
  - Zero-polling architecture using a 2-FD Linux `epoll` reactor listening to a persistent raw logcat event stream (`am_proc_start`, `am_proc_died`, `wm_resume_activity`, `screen_toggled`) and `inotify` configuration watches.
  - Dual operational modes:
    - `--observe`: Passive simulation mode logging candidates and telemetry without process termination.
    - `--act`: Active memory reclamation invoking `cmd activity kill --user all <pkg>`.
  - Zero-allocation stack readers for `/proc/<pid>/statm` and `/proc/meminfo`.
  - Dynamic system role detection (`cmd role get-role-holders`) protecting HOME, DIALER, SMS, and secure settings for default IME and live wallpaper services.
  - Strict burst suppression, LRU candidate depth tracking, and game mode escalation logic.
  - Modular AxManager / Axeron plugin packaging (`package/axmanager/`).
  - Automated multi-target CI/CD workflow and GNU General Public License v3.0 (GPL-3.0-only).
