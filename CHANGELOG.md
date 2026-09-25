# Changelog

All notable changes to `mini-lmk` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [1.4.0] - 2026-09-25

### Added
- **Runtime log switch:** a `log_enabled` key in `daemon.conf` (default `true`) and a `--no-log` command-line flag stop `operations.log` writes without restarting the daemon. Both change surfaces keep printing normally, so the reason `log_enabled` exists is an unbounded growth risk on a disk-constrained device, not output. The CLI flag wins over the file and cannot be undone by a reload; the file is re-read on every `inotify` reload, so flipping `log_enabled = false` mid-session works from the same tooling that edits `t_idle_sec`.
- **`min_oom_score_adj` and `log_enabled` added to the `config_reload` NDJSON record,** which previously logged six of the seven runtime keys and omitted the one that decides whether a reap is dispatchable at all. The `log_enabled` field carries the *effective* switch — config AND not `--no-log` — because a daemon started with `--no-log` still reads `true` from a file it was told to ignore, and a record claiming the log is live while nothing is written is worse than no record. The switch is applied around the record rather than uniformly after it: when disabling, the record is emitted while the file is still open so it lands as the last line; when enabling, the file is opened first so the record announcing the enable is the first line.
- **Terminal-Gated Table Output:** `TelemetrySink` resolves `isatty(1)` once at construction and renders the aligned human-readable table only when stdout is a terminal. `--json` is exempt: its NDJSON stream is a machine contract, so it is still written when stdout is a pipe or `/dev/null`. The `=== mini-lmk daemon ===` banner now uses the table's own predicate (`!json_stdout && isatty(1)`), so a redirected stdout no longer gets one stray human header; the `Indexed` and `Monitoring FDs` banners that `scripts/benchmark.sh` waits for stay gated on nothing but `--json`.
- **Write-path microbenchmarks:** two `telemetry::write_to_log` rows in `benches/microbench.rs` time one 190-byte NDJSON record through the real sink on a temp file, comparing the previous flush-every-line policy with the shipped one, where the flush sits outside the timed call at the batch boundary (reference device, 10,000 iterations: P50 1.23 µs → 77 ns, mean 1.55 µs → 389.3 ns). An earlier version of the second row flushed every 64 lines as a stand-in for a batch; it measured a policy the daemon does not run, so it was replaced rather than kept as a proxy.

### Changed
- **Lazy Telemetry Formats:** `emit_with()` takes the JSON and terminal-table renderings as closures, so a format with no destination is never built. A headless daemon no longer allocates two `String`s per event and no longer calls `localtime_r` for a table sent to `/dev/null`; with the log closed and no `--json` the JSON closure is skipped too, and the one branch that builds neither format is a sink whose log is unavailable (closed by `log_enabled`/`--no-log`, or failed to open) and whose stdout is neither a terminal nor JSON mode.
- **Batched Log Flushing:** `write_to_log` no longer flushes after every line. The reactor flushes once per `epoll_wait` batch, termination goes through one flush-then-exit helper (`exit` skips destructors), and a kill record is flushed immediately after it is emitted so the reap decision is on disk before the handler returns to the batch.
- **One file owns "is the log open":** `TelemetrySink` dropped its `log_enabled: bool` mirror of `writer.is_some()`. The two could disagree — re-enabling after a failed open used to mark logging on while no descriptor existed — and `writer` is the thing the writes consult anyway.
- **`config_reload` stopped being the exception to lazy rendering.** `reload_configs()` built both renderings eagerly and then handed the pair to `emit_with` as `|| tabular` / `|| json`, so a run that wanted neither — or only one — still paid for both, `localtime_r` included, contradicting the rule stated in `docs/ARCHITECTURE.md` §5.2. `config_reload_parts` became `config_reload_tabular` and `config_reload_json`, each called from its own closure.
- **Non-panicking writes:** every `println!`/`eprintln!` in the daemon and config loader became `let _ = writeln!(stdout(), ..)` / `writeln!(stderr(), ..)`.
- **Benchmark tables refreshed** from a single run of the current build on the reference device (Android 14, API 34, kernel 5.10, aarch64): microbenchmark rows, cold-start discovery, PSS/RSS percentiles, and the end-to-end idle/active wakeup counts.
- **Dropped records are explicit.** A disabled or closed log discards the event rather than queueing it, matching the pre-existing behaviour of a log that failed to open; the reason is in the `config_reload` record. Rotation checks happen before an append, so re-enabling after a long disabled period rotates first if the file already passed 512 KB.

### Fixed
- **A dead shell can no longer kill the daemon.** With `SIGPIPE` ignored and `panic = "abort"` in `[profile.release]`, a `println!` failing with `EPIPE`/`EIO` — the normal outcome once the detached `adb shell` that launched the daemon goes away, and the same failure a supervisor piped into a log file would hit — escalated into `SIGABRT`. A lost log line now costs a lost log line.
- **Records are not dropped on error exits.** The `exit(1)` paths in `spawn_logcat_stream()`, `reap_terminated_children()` and `run()` previously terminated without flushing `operations.log`. All eight now call a `-> !` helper that flushes first, so a future failure path cannot exit without it; the three startup failures that precede the sink use the same helper's inner function, which has nothing to flush.
- **The applied configuration is always recorded.** `config_reload` was emitted only when a load changed something, so a headless daemon started without a `daemon.conf` — or with one matching the defaults — left `operations.log` with no trace of what it was running, and the `[CONFIG] Active` line that does print it goes to a stdout `service.sh` sends to `/dev/null`. The first load of every run now records, flushed at once rather than waiting for an event batch a quiet device never produces; later loads still record only actual changes.
- **The startup record printed above the table it belongs to.** The sink is constructed inside `DaemonState::new()`, so the always-announce `config_reload` row reached stdout before `run()` had printed the `# TIME EVENT TARGET DETAIL / REASON` header, and repeated the `[CONFIG] Active` line above it in a second format. On a terminal with the log open, that first record now goes to the file only; later reloads are events and still print, and `--no-log` keeps the row because with no file it is the only rendering of the *effective* switch (`[CONFIG] Active` reports the file's value). Reproduced on the reference device, where the row landed eleven lines above the header.
- **Re-enabling the log trusted a byte count that could not have moved.** `open_log()` compared the cached `bytes_written` against the 512 KB cap, but that counter only advances while the writer is open, so a log that grew or was replaced during a disabled gap was appended to past the cap and a truncated one was rotated early. The size is now re-read from the inode on every open — which is what `README.md` and `docs/ARCHITECTURE.md` §5.2 had been promising since the switch shipped.
- **The supervisor scripts dropped every argument but the first.** `run-daemon.sh` execed `"$BIN" "$MODE"` with `MODE="${1:---act}"`, and the `service.sh` fallback did the same, so `sh run-daemon.sh --act --no-log` reached the daemon as a bare `--act` and it kept writing `operations.log` with no warning about the flag it never saw (`--json` was swallowed identically). Both now forward the whole argument vector and apply the historical `--act` default only when called with no arguments. Verified through the shipped layout on the reference device: `--observe` creates `operations.log`, `--observe --no-log` leaves no file.

### Known Limitations
- A `SIGABRT` landing between two flushes discards the records still in the 8 KB buffer — at most the events of one `epoll_wait` batch. No `fsync` is performed, so durability against power loss is unchanged from the previous per-line policy.

### Tests
- `test_emit_with_output_gates` walks every (`json_stdout`, `isatty`, log-openable) combination and asserts which formatter closures run — including the terminal-with-the-log-off case, which proves no JSON is built when no destination wants it — that `operations.log` still receives the record in every mode that can open it, and that the two formatters never mix outputs. `test_redirected_stdout_still_logs` covers the `service.sh` case specifically: no `--json`, no terminal, log openable — the columnar closure never runs, `silent()` stays false, and the NDJSON record reaches the file.
- `test_log_disable_closes_file_and_reenable_resumes` starts with logging off and asserts the file is never created, that the sink is not `silent()` (a terminal is still an output) and that the table renders anyway; then that re-enabling resumes writing, and — the half the release claimed but did not check — that a record emitted *while* the log was closed is still absent from the file after a re-enable and a flush, which is what makes "dropped, not queued" a tested property rather than prose. `test_runtime_config_parse` covers `log_enabled` defaulting to `true`, an explicit `false`, and an unparseable value keeping the previous setting.
- `test_config_reload_renderings_report_effective_log_switch` pins the record built from a config with `log_enabled = true` but an active `--no-log`: both renderings must say the log is off, and the field count is asserted (`:` occurrences in the NDJSON, `=` occurrences in the table row) so a future `RuntimeConfig` key that forgets to join the record fails instead of becoming invisible. Reverting the payload to the raw config value makes it fail, printing the lying record.
- `test_first_config_load_is_announced_even_without_a_change` points `reload_configs_from()` at an empty temporary config tree and a daemon whose config already equals what the absent file would produce, and requires one `config_reload` line to be *on disk* without an explicit flush — which pins both the always-announce rule and the startup flush. Removing either makes it fail; a second unchanged reload must add nothing.
- `reload_configs()` read whatever `ConfigPaths::get()` resolved — `$MODPATH`, else `/data/local/tmp/mlmk` — so no test could prove the file was applied at all, and deleting the `load_from_file` call left the suite green. The reload now takes a `&ConfigPaths` (`reload_configs_from`), which `test_reload_applies_the_daemon_conf_it_reads` uses to write a real `daemon.conf` in a temp tree and require its values in the live config *and* in the emitted record.
- `test_reopen_rotates_a_log_that_grew_while_disabled` appends past the cap from outside the sink while the log is closed and asserts re-enabling rotates first rather than appending, then truncates the live file and asserts the next re-enable appends without a second rotation.

---

## [1.3.0] - 2026-09-25

### Changed
- **Event-Sourced Timekeeping:** The stream is now `logcat -b events -v epoch`, so `now` comes from each record's own `secs.msec` timestamp instead of a per-event clock read. `parse_logcat_line()` returns `(u64, LogcatEvent)`, and the `alive_apps`, `recent_deaths`, screen-state, game-session, and session anchors moved from `Instant`/`Duration` to `u64` epoch milliseconds with saturating scalar idle math. `SystemTime::now()` survives only at cold-start bootstrap and the `config_reload` telemetry timestamp.
- **Simpler State:** Dropped the single-field `AppRecord` wrapper; `alive_apps` is now `FastMap<String, u64>`.
- **Lint Clean:** `cargo clippy --release --all-targets` is warning-free (behaviour-preserving idioms only: `is_some_and`, `unsigned_abs`, `sort_by_key(Reverse)`, iterator instead of indexed loop, `c"..."` literal, redundant `OpenOptions`/`format!` borrows, struct-update config default, inner doc comments).

### Fixed
- **Bootstrap Clock Step Detected:** `last_event_epoch` starts from the bootstrap clock reading instead of the `0` sentinel, so an unsynchronized-RTC → NTP step is classified on the *first* event and the anchors stamped at startup are rebased with all the others. Previously `session_start` and the screen-state durations carried the full RTC offset (years, not seconds) until the next step; `0` (no clock at boot) remains the only value that suppresses detection.
- **Malformed Epoch Rejected:** `parse_epoch_ms` uses checked arithmetic, so a seconds field too large to scale to milliseconds yields `None` instead of panicking in debug or wrapping to a garbage epoch in release.
- **Idle Ages Surviving Wall-Clock Steps:** `clock_jump_ms()` separates a real `settimeofday()`/NTP step (backward, or the one-time pre-2020 unsynchronized-RTC jump forward) from ordinary quiet gaps between events, and `apply_clock_jump()` shifts every stored anchor by the same signed delta. This removes spurious mass reap passes right after time sync and frozen idle ages after backward corrections, while telemetry `ts` stays the raw framework timestamp.

### Tests
- Added `parse_epoch_ms` fraction-parsing and out-of-range rejection coverage, and `clock_jump_ms` step classification (including the quiet-gap regression guard); rebuilt the parser fixtures on `-v epoch` lines with timestamp propagation assertions.
- Extracted `kill_telemetry_parts()` so the `act_mode` x `ams_protected` naming matrix is shared with production; the previous test re-typed that mapping and the JSON literal locally, so it passed even when the production arms changed.
- Added `test_bootstrap_clock_step_rebases_bootstrap_anchors`, which drives the real `clock_jump_ms()` + `apply_clock_jump()` pair over a `DaemonState` assembled by a test-only constructor (inert descriptors, throwaway telemetry file), covering the pre-sync boot step and the ordinary quiet gap that must not be mistaken for one.

### Documentation
- Audited every checkable claim in `README.md`, `docs/ARCHITECTURE.md`, and this file against the sources, and corrected the false or stale ones: `Instant`/`SystemTime` usage sites (now including the bootstrap seeding of the step classifier), config parsing allocation, procfs read scope, the inotify mask, `am_proc_died` as a non-telemetry event, measured per-ABI binary sizes in place of "< 400 KB", the Doze cadence, the pre-2020 RTC band, and the microbenchmark attributions (including the harness's disjunctive outlier predicate and its pooled `50 + 15` wakeup runs).
- Fixed commands users would have copied: the standalone quickstart's `run-daemon.sh` invocation (module layout only, replaced with an inline supervisor loop), the missing cross-compile `--target` in the benchmark reproduction steps, and the never-implemented `--dumpsys-iters` flag (also dropped from `scripts/benchmark.sh`).
- Documented the four missing NDJSON records (`config_reload`, `game_session_start`, `game_session_end`, `simulated_kill`), the `IN_DELETE_SELF`/`IN_MOVE_SELF` mask bits, and the screen-ON 5-minute escalator in terms of its real `alive_apps` anchor semantics; renamed the benchmark table column to "Flagged Preemption Samples" in `benches/microbench.rs` and both documents; deleted the remaining LaTeX math spans and the unsourced "2.5 MB PSS budget".
- Retracted the 1.1.1 parser claim for the unsubscribed `am_create_activity` event, restored the two 1.1.0 bullets that the 1.2.0 "removed example template" entry had swallowed, and corrected the 1.1.0 `oom_score_adj` P95 to the `10.69 µs` this file itself reports in 1.1.1.
---

## [1.2.0] - 2026-09-25

### Added
- **Android 7.0+ (API 24+) Backward Compatibility:**
  - Expanded compatibility envelope down to Android 7.0 (API 24+) while maintaining the zero-subprocess epoll reactor intact.
  - **Launcher Discovery Fallback:** Added pre-API 29 fallback querying default `HOME` launcher via `cmd package resolve-activity --brief -c android.intent.category.HOME` when `RoleManager` is absent.
  - **Initial Screen State Fallback:** Added pre-API 29 fallback evaluating `dumpsys power` (`mHoldingDisplaySuspendBlocker=true` or `Display Power: state=ON`) when `cmd deviceidle get screen` is unsupported, defaulting safely to `true`.
  - **Legacy Event Logcat Support:** Event parser tolerates legacy Android 7.0/8.0 3-token `am_resume_activity` (`[user,task_id,component]`) payloads by locating the `/`-bearing component token instead of relying on positional indices (`am_create_activity` is never subscribed and is not parsed).
  - **NDK Toolchain Target:** Target linkers configured to `android24-clang` across all supported Android architectures (`aarch64`, `armv7`, `x86_64`, `i686`).

### Changed
- **Pipeline & Parser Streamlining:**
  - Refactored `detect_initial_screen_on` into a functional fallback chain (`.ok().filter(...).and_then(...).or_else(...).unwrap_or(true)`).
  - Modernized `parse_resolve_activity_pkg` using functional iterator pipelines with `find_map` and `split_once('/')`.
  - Replaced libc `waitpid` and redundant `fcntl(F_GETFD)` calls in `probe_logcat_stream` with safe standard library `child.try_wait()` and descriptor polling.

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
  - Added `.cargo/config.toml.example` reference template for custom NDK setups (removed again in 1.2.0 as redundant with `$PATH` resolution).
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
  - Operates non-blocking with single-digit microsecond latency (P50 = 3.31 µs, P95 = 10.69 µs across 10,000 samples).
- **Rapid Respawn Tracking:**
  - Implemented telemetry detection for rapid process churn (`proc_died` followed by `proc_start` within 120 seconds).
  - Emits structured `RESPAWN` events to assist in identifying thrashing services, with automatic TTL-based table pruning.

### Changed
- **CLI Interface & Argument Modernization:**
  - Enforced explicit CLI argument semantics: invoking with empty arguments, `-h`, or `--help` prints the help manual and exits cleanly.
  - Removed implicit default to observe mode; requires explicit `--observe` or `--act`.
  - Simplified argument scanning to a direct `std::env::args()` iterator with let-else binding instead of collecting an argument `Vec` first (one `String` allocation per argument remains).
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
