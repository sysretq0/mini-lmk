# Architecture Specification & Technical Reference: mini-lmk

mini-lmk is a rootless, single-threaded, event-driven memory management daemon operating under Android shell privileges (UID 2000, API 24+). It preempts kernel memory thrashing and native lmkd direct-reclaim stalls by tracking application idle durations via native system log events, evicting stale background applications cleanly through Android framework APIs.

---

## 1. Architectural Foundations (Fixed Contracts)

### 1.1 Execution Boundaries & Security Context

* **Identity:** Runs as non-root Android shell (`shell` / UID 2000, accessible via ADB or Shizuku).
* **SELinux Domain:** `u:r:shell:s0` (Stock enforcing domain; zero root, KernelSU, Magisk, or custom sepolicy modifications required).
* **Single-Threaded & Fail-Fast:** Operates strictly on a single thread. It maintains no in-process reconnection state machine: if the upstream logcat pipe closes, yields `EPOLLHUP`, or encounters unrecoverable read errors, the daemon exits immediately (`exit(1)`). Process supervision and restarts are delegated externally to an init service or shell supervisor loop (`run-daemon.sh`).
* **Multi-ABI Target Footprint:** Native binary compiled across all primary Android architectures (`aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android`, and `i686-linux-android`) against Bionic `libc` with `android24-clang` compatibility (stripped ELFs: 296 KB armv7, 405 KB aarch64, 428 KB i686, 446 KB x86_64; < 4 MB RSS operational footprint).

### 1.2 Minimal 2-FD Epoll Reactor & Sub-Millisecond Health Probe

The daemon runs a push-based event loop with zero polling wakeups. The execution thread sleeps inside the kernel via `epoll_wait()` (infinite `timeout = -1`), consuming **0.0% CPU** during idle intervals across exactly **two file descriptors**:

```
+------------------------------------------------------------------------+
|                              epoll_wait()                              |
+------------------------------------------------------------------------+
                        |                        |
                        v                        v
               [TOKEN_LOGCAT_PIPE]        [TOKEN_INOTIFY]
              (Native Event Stream)       (<base>/config/)
                        |                        |
                        v                        v
               Lifecycle State Machine    Hot-Reload Configs
```

| Token | Descriptor Type | Source / Path | Trigger Condition | Reactor Action |
|---|---|---|---|---|
| `TOKEN_LOGCAT_PIPE` | Non-blocking Pipe (`O_NONBLOCK`) | Output of child `logcat` | Log buffer write | Parse event tag; update in-memory lifecycle state; evaluate 4-gate reaping pipeline. Exits process on EOF/HUP. |
| `TOKEN_INOTIFY` | Linux inotify | Watch on `<base>/config/` (`CLOSE_WRITE` \| `MOVED_TO` \| `CREATE` \| `DELETE` \| `DELETE_SELF` \| `MOVE_SELF`) | Config modified. The last two mask the watched directory itself being removed or moved; on any event `ensure_inotify_watch()` re-creates the directory if it vanished and re-adds the watch to the existing descriptor. | Reloads `daemon.conf`, `exclude.list`, and `games.list` in memory without dropping state. |

#### Startup Health Probe (`probe_logcat_stream`)
Prior to entering `epoll_wait()`, the daemon executes an instantaneous sanity check:
1. Validates that the child `logcat` process did not immediately exit via `child.try_wait()` (catching missing binaries, invalid flags, or SELinux execution denials).
2. Performs a non-blocking `libc::poll` on `logcat_fd` with zero timeout (`POLLIN | POLLHUP | POLLERR`) to catch immediate broken pipes before allocating epoll resources.

#### Signal Handling & Child Reaping
* POSIX signals `SIGINT` and `SIGTERM` are registered with `sa_flags = 0` (strictly **no `SA_RESTART`**), ensuring `epoll_wait()` returns `EINTR` immediately upon signal delivery for clean shutdown.
* Broken pipe (`SIGPIPE`) is explicitly ignored (`SIG_IGN`).
* Terminated child processes (`logcat` or asynchronous `cmd activity kill` invocations) are reaped non-blocking after `epoll_wait()` unblocks via `libc::waitpid(-1, &mut status, WNOHANG)`.

### 1.3 Unified Native Event Stream

System context is parsed from a single non-blocking pipe connected to the stdout of a child `logcat` process streaming the `events` buffer with epoch-seconds formatting (`-v epoch`), which prefixes every record with the framework's own wall-clock timestamp:

```bash
logcat -b events -v epoch -T 1 -s wm_resume_activity:V am_resume_activity:V am_proc_start:V am_proc_died:V screen_toggled:V device_idle_light_step:V
```

#### Line Format & Timestamp Extraction

```text
<epoch_sec>.<ms>  <pid>  <tid> <level> <tag>: <payload>
1695411709.231    2415   2415  I       am_proc_start: [0,12763,10130,com.example.app,service,{}]
```

`parse_logcat_line()` performs a single `split_once(':')` on the line, tokenizes the header with `split_whitespace()`, reads the leading token as `u64` epoch milliseconds (`secs.checked_mul(1000)? + 3-digit fraction`, so an out-of-range seconds field yields `None` rather than panicking in debug or wrapping in release), and takes the trailing header token as the tag. It returns `Option<(u64, LogcatEvent<'_>)>`, so the event's timestamp is delivered together with its payload and no `clock_gettime` is sampled on the hot path.

#### Event Handling & Canonicalization Rules

* **`wm_resume_activity` & `am_resume_activity` (Foreground Focus Transitions):**
  * *Payload Formats:*
    * Standard AOSP (API 29+): `[<user_id>, <token>, <task_id>, <component_name>]`
    * Legacy Android 7.0–9.0 (API 24–28): Standard 4-token `[<user_id>, <token>, <task_id>, <component>]` or 3-token `[<user_id>, <task_id>, <component>]`.
  * *Parsing & Sanitization:* Component names are formatted as `<package>/<activity>`. Token matching dynamically locates the token containing `/` without relying on brittle positional indices. The parser strips enclosing curly braces `{...}`, quotes, and Intent syntax (e.g. `{act=... cmp=pkg/act flg=...}`). Package names starting with `-` or invalid characters are strictly rejected.
  * *Action:* Updates the foreground LRU history stack (`fg_lru`), stamps the outgoing package departure time into `alive_apps` (subject to `uid >= 10000` verification), applies trampoline filtering (< 500 ms), and triggers the 4-gate reaping pipeline (with potential Game Mode escalation).

* **`am_proc_start` (Process Creation & Idle Baseline):**
  * *Payload:* `[<user_id>, <pid>, <uid>, <process_name>, <spawn_type>, {<component>}]`
  * *Canonicalization:* Normalizes process names with process-specific suffixes (`com.whatsapp:pushreceiver`, `com.android.chrome:sandboxed_process0`) to their base package: `pkg = process_name.split(':').first()`.
  * *Action:* Records the PID and base package into `pid_to_pkg`, `pkg_to_pids`, and `pkg_to_uid`.
    * **Rapid Respawn Tracking:** Checks if the package died within the preceding 120 seconds in `recent_deaths`. If so, logs a structured `RESPAWN` telemetry event.
    * **Game Intrusion Detection:** If currently in a gaming session and the spawn is non-foreground, increments `game_intrusion_count` and emits `GAME_INTRUDE` telemetry.
    * **Lifecycle Tracking:** If verified `uid >= 10000`, inserts into `alive_apps` with `last_active = now_epoch` (the event's own timestamp). Distinguishes between foreground activity launches (`is_fg_launch = is_fg || ev.spawn_type == "top-activity" || ev.spawn_type == "next-top-activity"`) and background spawns:
      * Foreground launches defer evaluation to `wm_resume_activity` so that candidate evaluation occurs on the subsequent focus switch.
      * Background spawns (`!is_fg_launch`) immediately evaluate the reaping pipeline, resolving single-app inactivity deadlocks during extended stationary sessions without synthetic polling timers.

* **`am_proc_died` (Process Eviction & State Cleanup):**
  * *Payload:* Standard AOSP `[<user_id>, <pid>, <process_name>, <oom_adj>, <reason>]` or legacy OEM `[<pid>, <process_name>]`.
  * *Parsing:* Uses strict adjacent token verification (`is_proc_name`) to eliminate false matches from positive `oom_adj` or state scores.
  * *Action:* Drops the PID from `pid_to_pkg` and `pkg_to_pids`. If all PIDs for the package have terminated, removes the package from `alive_apps`. Records death timestamp in `recent_deaths` (TTL 120s) and increments `session_stats.bg_deaths`. Strictly avoids triggering `evaluate_reaping_pipeline()` to prevent artificial eviction cascades.

* **`screen_toggled` (Display State Transitions):**
  * *Payload:* `0` (OFF) or `1` (ON).
  * *Action:* Updates internal power state metrics and timers, tracking active session vs. sleep duration and emitting `screen_state` records (`state: "OFF"` carries `active_duration_sec`, `state: "ON"` carries `off_duration_sec`) alongside the aggregated `bg_summary` counts. Intentionally avoids an immediate reaping pass on display lock to preserve multitasking across brief screen lock/unlock cycles; deep background harvesting is deferred to native Doze pulses (`device_idle_light_step`).

* **`device_idle_light_step` (Native Doze Heartbeat):**
  * *Payload:* Empty.
  * *Action:* Emitted by Android's `DeviceIdleController` during light-doze maintenance (cadence is vendor- and backoff-dependent, roughly every few minutes while the device is idle). When the screen is unpowered and `screen_off_harvest` is active, it triggers an opportunistic background reap aligned with native Doze wakeups with zero artificial timers.

### 1.4 Dynamic Role-Based System Immunity & Pre-API 29 Fallbacks

The daemon discovers system defaults dynamically on startup and config reloads, avoiding hardcoded vendor package names:

* **Platform Roles (`cmd role get-role-holders <ROLE>`):**
  * `android.app.role.HOME`: Active launcher.
  * `android.app.role.DIALER`: Default telephony dialer.
  * `android.app.role.SMS`: Default messaging application.
* **Pre-API 29 Launcher Fallback (API 24–28):**
  * On Android 7.0–9.0 where `RoleManager` does not exist, the daemon falls back to resolving the default HOME category:
    ```bash
    cmd package resolve-activity --brief -c android.intent.category.HOME
    ```
    Parsed via functional iterator pipelines (`find_map` and `split_once('/')`).
* **Active Input Method (`cmd settings get secure default_input_method`):**
  * Extracted package for active keyboard (e.g., Gboard, Fcitx5). Automatically unpeels optional multi-user prefixes (e.g. `0:com.google.android.inputmethod.latin/...`).
* **Active Wallpaper (`cmd settings get secure wallpaper_service`):**
  * Extracted and shielded only if non-null (live wallpapers).
* **Initial Screen State Discovery Fallback:**
  * Fast path (API 28+): `cmd deviceidle get screen`.
  * Fallback (API 24–27): Parses `dumpsys power` line-by-line (`mHoldingDisplaySuspendBlocker=true` or `Display Power: state=ON`), safely defaulting to `true` (screen on).
* **Explicit Policy:** `android.app.role.BROWSER` and `android.app.role.ASSISTANT` are **deliberately excluded** from automatic immunity so multi-tab web engines and search caches can be reaped when idle.

### 1.5 Eviction Mechanism Contract & Fail-Closed Guard

Process eviction is delegated exclusively to Android's `ActivityManagerService`:

```bash
cmd activity kill --user all <package_name>
```

* **Framework Invariant:** Invokes `killBackgroundProcesses()` inside `ActivityManagerService`.
* **Multi-User & Clone Profile Support:** Passing `--user all` ensures cached background processes across secondary users, Work Profiles (managed profiles), and cloned/dual apps (e.g., dual WhatsApp, Parallel Space, Secure Folder) are evicted simultaneously in a single IPC call without requiring per-user iteration.
* **Fail-Closed UID Guard:** Empirically verified on live hardware that AMS will terminate cached low-UID system packages (`uid < 10000`, e.g., KeyChain or vendor daemons). mini-lmk enforces a strict fail-closed guard: only packages with verified `uid >= 10000` (resolved from `pkg_to_uid`) may ever enter `alive_apps`. Unknown or system UIDs fail closed and are never targeted.
* **State Preservation:** AMS terminates processes only if they reside in cached or dormant background states. Application saved instance states, notifications, push tokens, and scheduled alarms remain intact.
* **SELinux Permission Boundary:** Under stock Android SELinux policy (`u:r:shell:s0`), direct signal delivery via `libc::kill` to third-party app domains is denied (`EPERM`). Delegating eviction to `cmd activity kill` routes the request through `ActivityManagerService`, which holds platform permissions to stop background processes.
* **Leading Hyphen Defense:** Package names beginning with `-` (or containing invalid characters) are strictly rejected at the parser and at the `/proc` ingestion boundary to prevent CLI argument injection / option confusion in `ActivityManagerShellCommand`.

---

## 2. In-Memory Lifecycle State Machine

The daemon operates with a clear separation between bootstrap indexing and steady-state event handling:
* **Bootstrap Indexing (`seed_initial_state`):** Performs a single one-time cold-start sweep of `/proc` reading `/proc/<pid>/cmdline` and metadata UID to populate initial process tables (`pid_to_pkg`, `pkg_to_pids`, `pkg_to_uid`). **Crucial Invariant:** cold-start processes are *index-only* and are deliberately **never** inserted into `alive_apps` — otherwise every boot process would expire on one synchronized `t_idle` (180 s) boundary and trigger an eviction cascade.
* **Steady-State Reactor:** Operates strictly event-driven without periodic `/proc` scans; every procfs read is caused by an arriving event, never by a timer. §3.3 lists the individual read sites.

```rust
struct DaemonState {
    config: RuntimeConfig,
    screen_on: bool,
    is_gaming: bool,
    act_mode: bool,                            // false = --observe (simulate only)
    alive_apps: FastMap<String, u64>,          // Base package -> last-active epoch ms (UID >= 10000 only)
    pid_to_pkg: FastMap<u32, String>,          // O(1) PID to base package resolution
    pkg_to_pids: FastMap<String, FastSet<u32>>,// Twin reverse index for O(1) package to PIDs
    pkg_to_uid: FastMap<String, u32>,          // In-memory cache for fail-closed UID validation
    recent_deaths: FastMap<String, u64>,       // Bounded rapid-respawn tracker (capacity 64, TTL 120s)
    last_event_epoch: u64,                     // Previous event timestamp, for wall-clock step detection
    fg_lru: VecDeque<String>,                  // Bounded to fg_lru_max_depth (default 10)
    dynamic_exclusions: FastSet<String>,
    user_exclusions: FastSet<String>,
    games: FastSet<String>,
    current_fg: Option<String>,
    screen_on_start: Option<u64>,              // Epoch ms anchors for screen_state durations
    screen_off_start: Option<u64>,
    game_session_start: Option<u64>,
    session_start: u64,                        // bg_summary aggregation origin
    telemetry: TelemetrySink,                  // + session_stats, intrusion counter, reactor fds
}
```

Only the fields the surrounding sections reason about are listed; see `src/main.rs` for the complete set (the remaining entries are the aggregated `SessionStats` counters, `game_intrusion_count`, `page_size_kb`, `json_stdout`, the `epoll`/`logcat`/`inotify` descriptors, and the spawned `logcat` child).

Every timestamp field above is a `u64` epoch millisecond; the four anchor fields (`screen_on_start`, `screen_off_start`, `game_session_start`, `session_start`) are rebased together with `alive_apps` and `recent_deaths` by `apply_clock_jump` (§2.3).

All lifecycle handlers (`on_resume_activity`, `on_proc_start`, `on_proc_died`, `on_screen_toggled`, `evaluate_reaping_pipeline`) receive the pre-parsed `now_epoch: u64` instead of sampling a clock, and idle ages are computed with saturating scalar arithmetic (`now_epoch.saturating_sub(last_active) / 1000`). `SystemTime::now()` survives at exactly two call sites of `get_epoch_ms()`: cold-start bootstrap in `DaemonState::new()` and the `ts` of the `inotify` `config_reload` telemetry record (an inotify event carries no framework timestamp). The bootstrap reading also seeds the jump classifier (`last_event_epoch`), so the step from an unsynchronized boot clock to a synced event stream is caught on the *first* event and the anchors stamped from it are rebased with all the others; a boot reading of `0` (clock unavailable) is the only value that suppresses detection. Because `logcat -T 1` replays roughly the last second of buffer, the first line may shift the anchors back by up to that replay window — real elapsed time, not a correction. Every other timestamp — including the `bg_summary` window derived from `session_start` — is event-sourced. `Instant` is no longer used anywhere in `src/`; the subprocess startup health probe uses `child.try_wait()` plus a zero-timeout `libc::poll`.

* **Process Birth (`am_proc_start`):** If `uid >= 10000`, inserts `alive_apps[pkg] = now_epoch`. If background spawn (`!is_fg_launch`), immediately triggers `evaluate_reaping_pipeline(now_epoch)`.
* **Foreground Focus (`wm_resume_activity`):**
  * Outgoing package: if `uid >= 10000`, stamps or refreshes `alive_apps[pkg] = now_epoch`.
  * Incoming package: push to head of `fg_lru`. Triggers `evaluate_reaping_pipeline()`.
* **Process Death (`am_proc_died`):** Resolves PID via `pid_to_pkg`, drops PID from `pkg_to_pids`, and increments `session_stats.bg_deaths`. When all PIDs for a package exit, removes the package from `alive_apps`. Does not trigger reaping.
* **Kill Dispatch (`evaluate_reaping_pipeline`):**
  * If candidate qualifies and is not `ams_protected`: dispatches `cmd activity kill`, immediately purges `cand.pkg` from `alive_apps` (preventing duplicate kills), and retains `pid_to_pkg` mappings so the asynchronous `am_proc_died` can resolve the PID, finish state cleanup, and seed `recent_deaths` for respawn detection (`am_proc_died` itself records no telemetry event).
  * If candidate is `ams_protected`: skips kill dispatch, refreshes `alive_apps[pkg] = now_epoch` to yield head-of-line priority to other candidates, and logs `KILL_SKIP` / `SIM_KILL` telemetry.

### 2.1 Lazy / Opportunistic PID Retaining

A critical design trade-off in `mini-lmk` is the **deliberate deferral of PID liveliness checks for protected applications**:
* **Why Non-Candidates Are Not Probed:** In any running Android system, dozens of apps reside in protected states (LRU position `< depth`, or idle time `< T_idle`). Actively polling or probing `/proc/<pid>/statm` for all tracked PIDs on every event would continuously wake CPU cores, thrash VFS caches, and disrupt low-power C-states—violating `mini-lmk`'s zero-overhead design target.
* **Event-Driven Eager Cleanup:** Under normal execution the logcat stream delivers `am_proc_died` events as processes exit, naturally unmapping PIDs and removing dead apps.
* **Opportunistic Candidate Reconciliation:** If an unmonitored or silent exit occurs, the PID is retained lazily in memory at zero cost until the enclosing package ages out of protection and qualifies as an eviction candidate. During candidate evaluation, targeted `/proc/<pid>/statm` probes verify PID existence. Dead PIDs and departed packages discovered during this probe are purged (`dead_pids`, `dead_pkgs`, `pkg_to_pids.get_mut(...).retain(...)`).
* **Candidate Verification:** Candidate ranking and RSS reclamation calculations evaluate only verified alive PIDs without continuous `/proc` polling.

### 2.2 Trampoline & Ephemeral Activity Filtering

To protect user multi-tasking state against activity re-entrance and auth overlays (e.g. Google Sign-In `SignInHubActivity`, intent choosers, payment gateways, ad SDK trampolines):
* **Transient Session Detection:** When an app departs the foreground after `< 500 ms` (`prev_dur_ms < 500`), it is classified as a transient trampoline rather than an intentional user application session.
* **LRU De-pollution:** The transient package is immediately pruned from `fg_lru`. This prevents rapid flash activities from displacing user applications from the LRU protection window (`< lru_protect_depth`), preserving user app state across auth redirects and deep links.

### 2.3 Wall-Clock Step Compensation

Idle ages are anchored to `CLOCK_REALTIME` values carried by the event stream, so a `settimeofday()`/NTP step must not silently rewrite elapsed time. `dispatch_logcat_line()` runs two small production helpers (unit-tested directly, never re-typed in the test):

* **`clock_jump_ms(prev_epoch, now_epoch) -> i64`** classifies the delta between consecutive samples. It returns `0` when the clock was unavailable at bootstrap, for any forward gap (a quiet 60 s or a quiet hour is real elapsed time, not a step), and for equal timestamps. It returns a negative delta for a backward step, and a positive delta only for the forward jump out of the pre-2020 unsynchronized-RTC band (`prev < RTC_SYNC_FLOOR_MS <= now`). Known limitation: a *forward* step starting from a clock already at or above the floor is indistinguishable from a quiet gap and returns `0` — only magnitude is observable, and a threshold that caught such a step would also freeze real idle age across legitimate Doze gaps, so anchors are assumed to be stamped from a clock that is either correct or pre-2020.
* **`apply_clock_jump(jump_ms)`** shifts *every* stored anchor by that signed delta with `saturating_add_signed`: `alive_apps`, `recent_deaths`, `screen_on_start`, `screen_off_start`, `game_session_start`, and `session_start`. Rebasing the whole set together keeps the respawn TTL, screen-state durations, and `bg_summary` interval as consistent as idle age across a correction.
* **Net effect:** elapsed ages are invariant under clock steps, while the `ts` written to telemetry stays the raw framework timestamp (the shift is applied to stored anchors, never to `now_epoch` itself). Rewriting a few dozen anchors on a rare step costs nanoseconds and keeps the hot path free of any per-event clock syscall.

---

## 3. Decision Pipeline & 4-Gate Evaluation

The reaping evaluation runs on:
1. **Foreground Focus Transitions (`wm_resume_activity` / `am_resume_activity`)**: Standard application switching triggers evaluation against escalators (Game Mode entry or critical low RAM).
2. **Background Process Spawns (`am_proc_start` where `!is_fg_launch`)**: Intercepts asynchronous background jobs and service launches while stationary in an app, triggering the pipeline to evict stale apps that exceeded the idle threshold without requiring synthetic polling timers.
3. **Opportunistic Screen-Off Maintenance**: While `!screen_on && screen_off_harvest`, native Doze heartbeats (`device_idle_light_step`) trigger opportunistic reaping passes without scheduling kernel hrtimers or disrupting CPU C-states. Display lock (`screen_toggled: 0`) records sleep timestamps passively without executing an immediate reaping pass, ensuring brief lock/unlock cycles preserve multitasking state.

```
Reap Evaluation Triggers:
  • wm_resume_activity / am_resume_activity (Focus change)
  • am_proc_start (Background spawns; is_fg_launch=false)
  • device_idle_light_step (Doze heartbeat when screen_off_harvest=true)
   │
   ├── 1. Assess Escalators & Power Context:
   │      • Is foreground app in games.list? ─────────────────────► Set T_idle = 10s (Grace Window)
   │      • Is MemAvailable < MEM_CRITICAL_PERCENT? ──────────────► Set T_idle = 10s (Grace Window)
   │      • Screen OFF + screen_off_harvest > 60s? ───────────────► Set T_idle = 30s, LRU depth = 1
   │      • Screen OFF + screen_off_harvest <= 60s? ──────────────► Set T_idle = 60s, LRU depth = 1
   │      • Screen ON + alive_apps[current_fg] age > 5 min? ─► Set T_idle = 60s, LRU depth = default
   │      • Nominal multitasking / Screen OFF (!harvest): ────────► Set T_idle = T_IDLE_DEFAULT_SEC, LRU depth = default
   │
   └── 2. Evaluate alive_apps in Memory:
          │
          ├── [Gate 1] In static/dynamic exclusions? ────────► SKIP (Protected Role / Exclude / Active FG)
          ├── [Gate 2] Position in fg_lru < effective_lru? ──► SKIP (Active App Protection)
          └── [Gate 3] (now_epoch - last_active) < T_idle? ► SKIP (Young / Warm App)
          │
          ▼
      [Eviction Candidates Identified]
          │
          ├── Targeted Read: Parse RSS from /proc/<pid>/statm for qualified candidates only
          ├── Opportunistic dead PID reconciliation (purge dead PIDs & empty packages)
          ├── Sort candidates descending by RSS (heaviest footprint first)
          ├── Take top candidates bounded by max_kills_per_pass
          │
          ▼
      [Gate 4: OOM Score Qualification]
      min(oom_score_adj) < min_oom_score_adj?
          │
          ├── YES (ams_protected):
          │    • Skip cmd activity kill spawn (save fork/exec IPC overhead)
          │    • Refresh last_active in alive_apps (relinquish slot, retain audit heartbeat)
          │    • Emit KILL_SKIP / SIM_KILL telemetry (ams_protected: true, spawn_skipped: true)
          │
          └── NO (eligible):
               • Dispatch cmd activity kill --user all <pkg>
               • Immediately remove package from alive_apps (prevent duplicate kills)
               • Retain pid_to_pkg mappings: the asynchronous am_proc_died resolves the PID back to its
                 package to drop it from pkg_to_pids / alive_apps and seed recent_deaths for respawn detection
```

### 3.1 Multi-Gate Rules

1. **Gate 1: Static & Dynamic Exclusions (`is_excluded`):**
   * Package matches `user_exclusions` (`exclude.list`).
   * Package matches `dynamic_exclusions` (HOME, DIALER, SMS, active IME, live wallpaper).
   * Package is the currently active foreground application.

2. **Gate 2: Recency Protection Gate (`effective_lru_depth`):**
   * **Screen ON (or `screen_off_harvest=false`):** The last N packages visited in `fg_lru` (default: 3) are completely protected. This prevents closing apps during active switching loops (such as copying 2FA verification codes).
   * **Screen OFF (`screen_off_harvest=true`):** Decimated to **1** (protecting only the immediate foreground app active prior to screen lock).

3. **Gate 3: Adaptive Idle Age Gate (`T_idle`):** The package must have departed from the foreground (or running in the background since spawn) for at least `t_idle_effective`, i.e. `now_epoch - alive_apps[pkg] >= t_idle_effective` in epoch milliseconds:
   * Background-spawned processes (`am_proc_start`) begin their clock at spawn and qualify only after crossing `t_idle_effective`.
   * Unindexed processes discovered during initial boot are not in `alive_apps` and cannot qualify until legitimate foreground user activity occurs.

4. **Gate 4: OOM Score Qualification Gate (`min_oom_score_adj`):**
   * The daemon queries `/proc/<pid>/oom_score_adj` for every live PID belonging to the candidate package.
   * If `min(oom_score_adj) < min_oom_score_adj`, the package is identified as `ams_protected`. The daemon skips the process spawn (`cmd activity kill`) to eliminate wasted fork/exec overhead and futile AMS rejections, while preserving full telemetry visibility across `--observe` and `--act` modes (`ams_protected: true`, `spawn_skipped: true`).
   * The package's last-active anchor is refreshed in `alive_apps`, yielding the eviction slot to subsequent candidates on future passes while maintaining a periodic audit heartbeat every `t_idle_effective`.
   * Tunable in `daemon.conf` between `500` and `900` (default: `900`):
     * `900` (Conservative): Restricts evictions strictly to cached/idle processes (`CACHED_APP_MIN_ADJ = 900`). Shields processes with `oom_score_adj < 900` (e.g., active background services, audio players, background downloads).
     * `500` (Aggressive): Permits eviction of background services down to AOSP's framework limit (`SERVICE_ADJ = 500`). Maximizes reclaimable RAM for gaming or constrained devices.

### 3.2 Escalation Modes & 10-Second Grace Window

* **Game Focus Entry:** If the incoming package matches `games.list`, `t_idle_effective` drops to **10 seconds**. Rather than an instantaneous 0s wipeout, this 10-second grace window prevents killing game launchers, authentication activities, or companion apps that backgrounded seconds before the game engine took focus. All background apps idle for > 10s beyond `effective_lru_depth` are evicted immediately.
* **Critical Memory Starvation:** If `/proc/meminfo` reports `MemAvailable < MEM_CRITICAL_PERCENT`, `t_idle_effective` drops to **10 seconds** across standard application switches to aggressively reclaim RAM while avoiding thrashing newly backgrounded tasks.
* **Screen-Off Deep Harvest:** During opportunistic screen-off maintenance on native Doze heartbeats (`device_idle_light_step`), `effective_lru_depth` drops to 1, and `T_idle` decays from 60s to 30s (once the screen has been off for > 60s), purging accumulated background memory while the display panel is unpowered.
* **Stationary Session Relaxation (Screen ON):** If the *current* foreground package still has an `alive_apps` anchor older than 5 minutes, `T_idle` relaxes from `t_idle_sec` (default 180s) to 60s. Note the anchor semantics: any spawn of a UID >= 10000 package indexes its anchor (`entry().or_insert()`), and afterward the value is written in only two places — when the package *leaves* the foreground (§1.4) and when an `ams_protected` candidate is re-anchored after a skipped kill (§2, Kill Dispatch). So a package that stays in front accumulates anchor age from its first indexed spawn, which is precisely the stationary session this branch describes. A package that has never been stamped yields age `0` (see §1.3).

### 3.3 Targeted RSS Density Sorting & Burst Capping

When multiple candidate packages qualify for eviction simultaneously:

1. The daemon reads column 2 (resident pages × `PAGE_SIZE`) from `/proc/<pid>/statm` **strictly for the qualified candidates**. Any PIDs returning 0 RSS (`ENOENT` caused by process exit missed during rare logcat ring buffer drops) are opportunistically purged from `pid_to_pkg` and `pkg_to_pids`. If all PIDs for a candidate have exited, the stale package entry is deleted from `alive_apps`—reconciling in-memory state without periodic `/proc` filesystem sweeps.
2. Surviving candidates are sorted in **descending order of RSS** (reclaiming the largest memory footprints first).
3. **Hardware-Scaled Burst Limit (`max_kills_per_pass`):** Only the top N candidates are evicted in a single pass to bound reactor latency and prevent Binder thread contention in `system_server`. Defaults auto-scale by physical RAM detected from `/proc/meminfo` at startup:
   * **<= 4.5 GB RAM:** 4 kills per pass (aggressive recovery on low-RAM devices).
   * **4.5 GB - 8.5 GB RAM:** 2 kills per pass (balanced mainstream devices).
   * **> 8.5 GB RAM:** 1 kill per pass (conservative eviction on high-headroom flagships).
   * Can be overridden at runtime via `max_kills_per_pass` in `daemon.conf`.

---

## 4. Runtime Configuration & Tuning Parameters

The following parameters in `daemon.conf` are live-calibrated via inotify. Their default values serve as baseline estimates and are subject to calibration based on empirical traces recorded in `<base>/logs/operations.log`:

| Parameter | Default | Range / Scale | Description & Calibration Target |
|---|---|---|---|
| `t_idle_sec` | `180` | `u64` (seconds) | Base background idle threshold. Tuned against user app resume latency to avoid evicting apps returned to frequently. |
| `lru_protect_depth` | `3` | `usize` (packages) | Depth of protected foreground history window. Protects active multitasking workflows. |
| `mem_critical_percent` | `10` | `u64` (% of `MemTotal`) | RAM watermark triggering emergency idle bypass (`T_idle = 10s`). Correlated against `/proc/vmstat` `allocstall_normal` and `pgscan_direct`. |
| `fg_lru_max_depth` | `10` | `usize` (packages) | Maximum depth of the foreground history ring buffer. |
| `screen_off_harvest` | `true` | `bool` | Enables opportunistic screen-off maintenance during Doze heartbeats (`device_idle_light_step`). |
| `max_kills_per_pass` | Auto-scaled at bootstrap (`4` / `2` / `1` by RAM; `2` in `RuntimeConfig::default()`) | `usize` >= 1 (floored at 1, no upper clamp) | Eviction burst cap. Auto-scales by physical RAM: `<=4.5 GB` -> 4, `4.5–8.5 GB` -> 2, `>8.5 GB` -> 1. |
| `min_oom_score_adj` | `900` | `i32` (`500`–`900`) | Minimum OOM score required for eviction eligibility. Clamped to `500..=900`. `900` targets cached/idle processes; `500` extends reclamation to background services. |

---

## 5. File Layout & Inotify Isolation

To prevent inotify feedback loops where log emission re-triggers configuration reloads, configurations and runtime logs are isolated into separate directories under `<base>` (resolved dynamically to `$MODPATH/mlmk/` when `MODPATH` is present in the environment — the installed-module case, where `run-daemon.sh` exports it — with fallback to `/data/local/tmp/mlmk/` when the daemon is executed directly):

```text
<base>/
├── config/                  <-- Watched by inotify (TOKEN_INOTIFY)
│   ├── daemon.conf          <-- Volatile runtime tuning parameters (key=value); not created by the installer
│   ├── exclude.list         <-- Created empty by customize.sh
│   └── games.list           <-- Created empty by customize.sh
└── logs/                    <-- Unwatched; append-only telemetry sink
    ├── operations.log       <-- Live NDJSON event stream (512 KB max)
    └── operations.log.old   <-- Rotated backup file
```

### 5.1 Configuration Files (`config/`)

#### Runtime Parameters (`config/daemon.conf`)

When `daemon.conf` is absent or unreadable, `load_from_file` leaves every compiled default in place and reports nothing.

Parameters are read with `fs::read_to_string` into a single `String`, then walked line-by-line with `lines()` and `split_once('=')` (no regex, no parser crate) — this path runs only at bootstrap and on `inotify` reload, never per event, so the one heap allocation is deliberate. Per-key validation, clamping, and unknown-key behavior are specified in the §4 table and implemented in `RuntimeConfig::parse_str`.

```ini
# Live-tunable volatile parameters (auto-reloaded via inotify)
t_idle_sec=180
lru_protect_depth=3
mem_critical_percent=10
fg_lru_max_depth=10
screen_off_harvest=true

# Maximum background apps evicted per reap pass (burst cap)
# Defaults auto-scale by physical RAM: <=4.5GB -> 4, 4.5GB-8.5GB -> 2, >8.5GB -> 1
# max_kills_per_pass=2

# Minimum oom_score_adj threshold required for eviction (range: 500-900, default: 900)
# 900: Conservative (cached & idle processes only; protects all background services)
# 500: Aggressive (matches AOSP SERVICE_ADJ; reclaims background services for games/heavy loads)
min_oom_score_adj=900
```

#### Exclusions (`config/exclude.list`)

Package names immune from eviction under all conditions (one per line). **Empty by default out of the box**; populated by the user for critical background services or tools:

```text
# Example user exclusions (file ships empty by default out of the box)
# com.tailscale.ipn
# moe.shizuku.privileged.api
```

#### Game Profiles (`config/games.list`)

Package names triggering Game Mode entry flushing (`T_idle -> 10s`). **Empty by default out of the box**; populated by the user for high-demand 3D gaming workloads:

```text
# Example game targets (file ships empty by default out of the box)
# com.miHoYo.GenshinImpact
# com.proximabeta.nikke
```

### 5.2 Telemetry Logging Format (`logs/operations.log`)

The daemon writes structured Newline-Delimited JSON (NDJSON) through a `BufWriter` that is flushed after every line (so the buffer mainly serves partial-write coalescing, not deferred I/O), with automatic 512 KB log rotation (`operations.log.old`, permissions `0666` on Unix). Individual process lifecycle spawns and exits are aggregated into `bg_summary` events to prevent log spam and disk thrashing: the record is suppressed entirely when both counters are zero, and its `interval_sec` is the window being summarized — time since the previous summary on a foreground switch, or the active/sleep duration of the screen session that just ended on a `screen_state` transition.

The emitted event vocabulary is exactly `kill`, `kill_skipped`, `simulated_kill`, `fg_switch`, `screen_state`, `bg_summary`, `respawn`, `game_intrusion`, `game_session_start`, `game_session_end`, and `config_reload`. `am_proc_died` produces **no** record of its own: deaths only surface as the `deaths` counter inside the next `bg_summary`, or as a `respawn` record when the package returns within 120 s.

The examples below are one representative record per event. Note that `config_reload` is emitted only when a reload actually changed a value, and its `ts` is the one non-event-sourced timestamp in the stream (§1.3); `game_session_start` / `game_session_end` require a `games.list` match.

```json
{"ts":1790255284675,"event":"screen_state","state":"OFF","active_duration_sec":1850}
{"ts":1790255285117,"event":"screen_state","state":"ON","off_duration_sec":420}
{"ts":1790255285118,"event":"bg_summary","interval_sec":420,"spawns":5,"deaths":3,"spawn_rss_kb":65536}
{"ts":1790255286893,"event":"fg_switch","pkg":"com.android.settings","component":"com.android.settings/.Settings$StorageUseActivity","prev_dur_ms":1240,"is_game":false}
{"ts":1790255286893,"event":"kill","pkg":"com.facebook.katana","pids":[3939],"rss_freed_est_kb":842324,"reason":"idle_expired","idle_sec":412,"lru_pos":5,"spawned":true,"oom_score_adj":950,"ams_protected":false,"spawn_skipped":false}
{"ts":1790255286894,"event":"kill_skipped","pkg":"com.spotify.music","pids":[12345],"rss_freed_est_kb":312000,"reason":"idle_expired","idle_sec":200,"lru_pos":4,"spawned":false,"oom_score_adj":200,"ams_protected":true,"spawn_skipped":true}
{"ts":1790255287102,"event":"game_intrusion","pid":12763,"uid":10130,"pkg":"com.google.android.calculator","proc":"com.google.android.calculator","type":"service","rss_kb":45200,"excluded":false}
{"ts":1790255288500,"event":"respawn","pkg":"com.google.android.youtube","gap_ms":4250,"pid":14520,"uid":10150,"type":"service"}
{"ts":1790255289004,"event":"game_session_start","pkg":"com.miHoYo.GenshinImpact"}
{"ts":1790255291006,"event":"game_session_end","duration_sec":142,"intrusions":2}
{"ts":1790255292210,"event":"simulated_kill","pkg":"com.twitter.android","pids":[15120],"rss_freed_est_kb":208896,"reason":"idle_expired","idle_sec":322,"lru_pos":6,"spawned":false,"oom_score_adj":950,"ams_protected":false,"spawn_skipped":false}
{"ts":1790255293004,"event":"config_reload","t_idle_sec":180,"lru_protect_depth":3,"mem_critical_percent":10,"fg_lru_max_depth":10,"screen_off_harvest":true,"max_kills_per_pass":2}
```

---

## 6. Empirical Performance & Latency Profile

Measured on physical target environment (Android 14 API 34, Linux kernel 5.10, `aarch64`):

### 6.1 Low-UID System Package Termination: Empirical AMS Verification

To verify the necessity of the `uid >= 10000` fail-closed candidate guard, `cmd activity kill --user 0` was tested directly on Android 14 against running low-UID system packages:

* **Cached System App (`com.android.keychain`, UID 1000):** Backgrounded with `oom_score_adj = 905`. Upon dispatching `cmd activity kill --user 0 com.android.keychain`, AMS immediately terminated the process (PID 29506 died and was purged from the system).
* **Vendor Daemon (`com.sprd.validationtools`, UID 1000):** Upon dispatching `cmd activity kill`, AMS killed PID 20352 (which immediately triggered a zygote respawn as PID 4788).
* **Persistent System Service (`com.android.se`, UID 1068):** Running as an active system service with low OOM adj; AMS ignored the kill command (PID 2143 remained alive).

**Conclusion:** Activity Manager Service (AMS) **does kill** low-UID system packages if they reside in cached/background states. Without the daemon's in-memory `uid >= 10000` guard, targeting low-UID packages would terminate critical framework dependencies (e.g., KeyChain or vendor daemons).

### 6.2 Microbenchmark Suite: Kernel-Direct Procfs Latency Distribution

Evaluated via `benches/microbench.rs` across 1,000 warm-up cycles and 10,000 timed iterations, comparing hot-cache self inspection against real-world cold multi-PID pool traversal:

| Target / Operation | Mechanism / Scope | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean | Flagged Preemption Samples |
|---|---|---|---|---|---|---|---|---|---|
| `procfs::read_oom_score_adj` | self, hot cache | 10,000 | 2.85 µs | **3.31 µs** | 10.69 µs | 14.15 µs | 531.69 µs | 4.32 µs | 1 |
| `procfs::read_oom_score_adj` | multi-PID pool (cold) | 10,000 | 4.69 µs | **8.69 µs** | 23.38 µs | 38.15 µs | 999.08 µs | 11.72 µs | 3 |
| `procfs::read_statm_rss_kb` | self, hot cache | 10,000 | 3.69 µs | **4.23 µs** | 20.31 µs | 40.08 µs | 5.71 ms | 8.86 µs | 4 |
| `procfs::read_statm_rss_kb` | multi-PID pool (cold) | 10,000 | 5.00 µs | **6.23 µs** | 10.46 µs | 14.77 µs | 450.38 µs | 7.07 µs | 0 |
| `procfs::read_meminfo_kb` | `/proc/meminfo` | 10,000 | 7.69 µs | **8.00 µs** | 9.46 µs | 12.62 µs | 377.54 µs | 8.50 µs | 0 |
| `telemetry::FormattedTime` | Stack buffer format | 10,000 | 76.0 ns | **307.0 ns** | 308.0 ns | 308.0 ns | 2.54 µs | 261.3 ns | 0 |
| `Instant::now` (Overhead) | Harness baseline (not on daemon hot path) | 10,000 | 0.0 ns | **231.0 ns** | 385.0 ns | 539.0 ns | 2.38 µs | 226.7 ns | 0 |

#### Architectural Key Takeaways:
1. **Cold Multi-PID VFS Access:** Cycling through a pool of external PIDs across the system shifts P50 latency from 3.31 µs (self, pinned dcache) to 8.69 µs (external PID, VFS dentry traversal). Even under cold multi-PID access, candidate qualification completes in under **9 microseconds per PID**.
2. **Tail Latency Root Cause:** `microbench.rs` flags a sample when its wall time exceeds 500 µs **and** either `ru_nivcsw` incremented or `CLOCK_THREAD_CPUTIME_ID` stayed below 50 µs; the per-row counts are in the last column. Because the predicate is a disjunction, a flagged sample proves scheduler disturbance, sub-50 µs on-CPU time, or both — enough to rule out a kernel VFS stall (every mean stays between 4.3 µs and 11.7 µs) and consistent with **Linux CFS scheduler preemption**, but not proof that a context switch occurred in each individual flagged sample.

### 6.3 End-to-End Daemon Benchmarks: Idle vs Active App-Switching Pipeline

Evaluated via `scripts/benchmark.sh` across both steady-state idle conditions (`N = 50`) and active live app-switching workloads (`N = 15`, cycling between Settings, Home, and Browser transitions):

| Metric | Workload Mode | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean |
|---|---|---|---|---|---|---|---|---|
| **Cold-start Discovery** | Initial Boot Indexing | 50 | 181.1 ms | **251.6 ms** | 289.6 ms | 343.5 ms | 343.5 ms | 251.6 ms |
| **Steady-State Memory (PSS)** | Idle Boot State | 50 | 1,081 kB | **1,109 kB** | 1,133 kB | 1,139 kB | 1,139 kB | 1,109 kB |
| **Active Pipeline Memory (PSS)**| Live App-Switch Traffic | 15 | 1,150 kB | **1,165 kB** | 1,183 kB | 1,183 kB | 1,183 kB | 1,165 kB |
| **Resident Set Size (RSS)** | Idle Boot State | 50 | 3,744 kB | **3,848 kB** | 3,944 kB | 4,012 kB | 4,012 kB | 3,850 kB |
| **Resident Set Size (RSS)** | Live App-Switch Traffic | 15 | 3,852 kB | **3,960 kB** | 4,044 kB | 4,044 kB | 4,044 kB | 3,965 kB |
| **Inter-Event Idle CPU Wakeups** | Post-Traffic Idle Window | 50 + 15 | 0 | **0** | 0 | 0 | 0 | 0.0 |

**Pipeline Memory Growth:** Active app-switching with populated `alive_apps`, `fg_lru`, `pkg_to_pids`, and `recent_deaths` tables increases steady-state PSS by only **+56 kB** (from 1,109 kB to 1,165 kB). The wakeup row pools the `N = 50` idle and `N = 15` active runs, which is why each sample is 0 and the mean is 0.0. Inter-event wakeups remain strictly **0** once traffic pauses.

---

## 7. Licensing & Distribution

This project is licensed under the **GNU General Public License v3.0** (`GPL-3.0-only`). See the [`LICENSE`](../LICENSE) file for complete terms and legal text.
