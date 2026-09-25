# Architecture Specification & Technical Reference: mini-lmk

mini-lmk is a rootless, single-threaded, event-driven memory management daemon operating under Android shell privileges (UID 2000, API 24–37+). It preempts kernel memory thrashing and native lmkd direct-reclaim stalls by tracking application idle durations via native system log events, evicting stale background applications cleanly through Android framework APIs.

---

## 1. Architectural Foundations (Fixed Contracts)

### 1.1 Execution Boundaries & Security Context

* **Identity:** UID `2000` (`shell`), GID `2000` (`shell`), supplementary GID `1007` (`log`). Rootless execution relying on platform shell privileges (accessible via ADB or Shizuku).
* **SELinux Domain:** `u:r:shell:s0` (Stock enforcing domain; zero root, KernelSU, Magisk, or custom sepolicy modifications required).
* **Single-Threaded & Fail-Fast:** Operates strictly on a single thread. It maintains no in-process reconnection state machine: if the logcat stream closes, yields `EPOLLHUP`, or encounters unrecoverable read errors, the daemon exits immediately (`exit(1)`). Process supervision and restarts are delegated externally to an init service or shell supervisor loop.
* **Footprint:** Single native binary compiled for `aarch64-linux-android` against Bionic `libc` (< 400 KB stripped ELF; < 4 MB RSS operational footprint).

### 1.2 Minimal 2-FD Epoll Reactor

The daemon runs a push-based event loop with zero polling wakeups. The execution thread sleeps inside the kernel via `epoll_wait()`, consuming **0.0% CPU** during idle intervals across exactly **two file descriptors**:

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
| `TOKEN_LOGCAT_PIPE` | Non-blocking Pipe (`O_NONBLOCK`) | Output of child `logcat` | Log buffer write | Parse event tag; update in-memory lifecycle state; evaluate reaping gates. Exits process on EOF/HUP. |
| `TOKEN_INOTIFY` | Linux inotify | Watch on `<base>/config/` (`CLOSE_WRITE` \| `MOVED_TO` \| `CREATE` \| `DELETE`) | Config modified | Reloads `daemon.conf`, `exclude.list`, and `games.list` in memory without dropping state. |

### 1.3 Unified Native Event Stream

System context is parsed from a single non-blocking pipe connected to logd's native binary events buffer:

```bash
logcat -b events -v tag -s wm_resume_activity am_resume_activity am_proc_start am_proc_died screen_toggled device_idle_light_step -T 1
```

#### Event Handling & Canonicalization Rules

* **`wm_resume_activity` & `am_resume_activity` (Foreground Focus Transitions):**
  * *Payload:* `[<user_id>, <token>, <task_id>, <component_name>]` (Handles `am_resume_activity` on API 24–28 and `wm_resume_activity` on API 29–37+).
  * *Parsing:* Component is formatted as `<package>/<activity>`. Token matching dynamically locates the component containing `/` without relying on brittle positional indices. Substring extraction prior to `/` yields the foreground package name directly.
  * *Action:* Updates the foreground LRU history stack, stamps the outgoing package departure time, and triggers the reaping pipeline (with potential Game Mode escalation).

* **`am_proc_start` (Process Creation & Idle Baseline):**
  * *Payload:* `[<user_id>, <pid>, <uid>, <process_name>, <spawn_type>, {<component>}]`
  * *Canonicalization:* Processes often have suffixed names (`com.whatsapp:pushreceiver`, `com.android.chrome:sandboxed_process0`). The daemon normalizes the process name to the base package:

    `pkg = process_name.split(':').first()`

  * *Action:* Records the PID and base package into memory with `last_active = Instant::now()`. Distinguishes between foreground activity launches (`is_fg_launch = is_fg || ev.spawn_type == "top-activity" || ev.spawn_type == "next-top-activity"`) and background spawns:
    * If foreground activity launch: defers evaluation to `wm_resume_activity` so that evictions are masked behind UI transition animations.
    * If background spawn (`!is_fg_launch`): immediately evaluates the reaping pipeline (unconstrained marker architecture). This eliminates the single-app inactivity deadlock during prolonged foreground sessions while avoiding continuous timers or polling.

* **`am_proc_died` (Process Eviction & State Cleanup):**
  * *Payload:* `[<user_id>, <pid>, <process_name>, <oom_adj>, <reason>]`
  * *Action:* Drops the PID and package from memory tables (`pid_to_pkg`, `record.pids`) and increments background death counters. It strictly avoids invoking `evaluate_reaping_pipeline()`—process deaths return physical RAM to the kernel, so triggering evictions on process death would induce artificial eviction cascades.

* **`screen_toggled` (Display State Transitions):**
  * *Payload:* `0` (OFF) or `1` (ON).
  * *Action:* Updates internal power state metrics and timers, tracking active session vs. sleep duration and emitting `SCREEN_OFF`/`SCREEN_ON` telemetry alongside background summaries. Intentionally avoids an immediate reaping pass on display lock to preserve multitasking across brief screen lock/unlock cycles; deep background harvesting is deferred to native Doze pulses (`device_idle_light_step`).

* **`device_idle_light_step` (Native Doze Heartbeat):**
  * *Payload:* Empty.
  * *Action:* Emitted by Android's `DeviceIdleController` every ~5 minutes during light idle maintenance. When the screen is unpowered, it triggers an opportunistic background reap aligned with native Doze wakeups with zero artificial timers.

### 1.4 Dynamic Role-Based System Immunity

The daemon discovers system defaults dynamically on startup and config reloads, avoiding hardcoded vendor package names:

* **Platform Roles (`cmd role get-role-holders <ROLE>`):**
  * `android.app.role.HOME`: Active launcher.
  * `android.app.role.DIALER`: Default telephony dialer.
  * `android.app.role.SMS`: Default messaging application.
* **Active Input Method (`cmd settings get secure default_input_method`):**
  * Extracted package for active keyboard (e.g., Gboard, Fcitx5).
* **Active Wallpaper (`cmd settings get secure wallpaper_service`):**
  * Extracted and shielded only if non-null (live wallpapers).
* **Explicit Policy:** `android.app.role.BROWSER` and `android.app.role.ASSISTANT` are **deliberately excluded** from automatic immunity so multi-tab web engines and search caches can be reaped when idle.

### 1.5 Eviction Mechanism Contract

Process eviction is delegated exclusively to Android's `ActivityManagerService`:

```bash
cmd activity kill --user all <package_name>
```

* **Framework Invariant:** Invokes `killBackgroundProcesses()` inside `ActivityManagerService`.
* **Multi-User & Clone Profile Support:** Passing `--user all` (rather than hardcoded `--user 0`) ensures cached background processes across secondary users, Work Profiles (managed profiles), and cloned/dual apps (e.g., dual WhatsApp, Parallel Space, Secure Folder) are evicted simultaneously in a single IPC call without requiring per-user iteration.
* **State Preservation:** AMS terminates processes only if they reside in cached or dormant background states. Application saved instance states, notifications, push tokens, and scheduled alarms remain intact.
* **DAC/MAC Compliance:** Bypasses POSIX `kill -9` restrictions (`EPERM` under SELinux `u:r:shell:s0`).
* **Leading Hyphen Defense:** Package names beginning with `-` (or containing invalid characters) are strictly rejected at the parser and procfs ingestion boundaries to prevent CLI argument injection / option confusion in `ActivityManagerShellCommand`.

---

## 2. In-Memory Lifecycle State Machine

The daemon operates with zero `/proc` directory scanning and zero package database lookups:

```rust
struct AppRecord {
    pids: FastSet<u32>,
    last_active: Instant, // Monotonic clock
}

struct DaemonState {
    alive_apps: FastMap<String, AppRecord>, // Keyed by canonical base package
    pid_to_pkg: FastMap<u32, String>,
    fg_lru: VecDeque<String>,               // Bounded to depth 10
    dynamic_exclusions: FastSet<String>,
    user_exclusions: FastSet<String>,
    games: FastSet<String>,
}
```

* **Process Birth (`am_proc_start`):** Insert `AppRecord` into `alive_apps` with `last_active = Instant::now()`. If background spawn (`!is_fg_launch`), immediately triggers `evaluate_reaping_pipeline()`.
* **Foreground Focus (`wm_resume_activity`):**
  * Outgoing package: record `last_active = Instant::now()`.
  * Incoming package: push to head of `fg_lru`. Triggers `evaluate_reaping_pipeline()`.
* **Process Death (`am_proc_died`):** Resolves PID via `pid_to_pkg`, drops PID from `record.pids`, and increments `session_stats.bg_deaths`. When all PIDs for a package exit, removes the package from `alive_apps`. Does not trigger reaping.
* **Kill Dispatch (`evaluate_reaping_pipeline`):** Once `cmd activity kill` is dispatched for candidate `cand.pkg`, immediately purges `cand.pkg` from `alive_apps` to prevent duplicate kills across rapid back-to-back evaluations. Retains `pid_to_pkg` mappings until asynchronous `am_proc_died` arrives to record death telemetry when processes terminate.

### 2.1 Lazy / Opportunistic PID Retaining

A critical design trade-off in `mini-lmk` is the **deliberate deferral of PID liveliness checks for protected applications**:
* **Why Non-Candidates Are Not Probed:** In any running Android system, dozens of apps reside in protected states (LRU position `< depth`, or idle time `< T_idle`). Actively polling or probing `/proc/<pid>/statm` for all tracked PIDs on every event would continuously wake CPU cores, thrash VFS caches, and disrupt low-power C-states—violating `mini-lmk`'s zero-overhead design target.
* **Event-Driven Eager Cleanup:** Under normal execution, the logcat stream delivers `am_proc_died` events synchronously whenever processes exit, naturally unmapping PIDs and removing dead apps.
* **Opportunistic Candidate Reconciliation:** If an unmonitored or silent exit occurs, the PID is retained lazily in memory at zero cost until the enclosing package ages out of protection and qualifies as an eviction candidate. During candidate evaluation, targeted `/proc/<pid>/statm` probes verify PID existence. Dead PIDs and departed packages discovered during this probe are purged (`dead_pids`, `dead_pkgs`, `record.pids.retain(...)`).
* **Candidate Verification:** Candidate ranking and RSS reclamation calculations evaluate only verified alive PIDs without continuous `/proc` polling.

### 2.2 Trampoline & Ephemeral Activity Filtering

To protect user multi-tasking state against activity re-entrance and auth overlays (e.g. Google Sign-In `SignInHubActivity`, intent choosers, payment gateways, ad SDK trampolines):
* **Transient Session Detection:** When an app departs the foreground after `< 500 ms` (`prev_dur_ms < 500`), it is classified as a transient trampoline rather than an intentional user application session.
* **LRU De-pollution:** The transient package is immediately pruned from `fg_lru`. This prevents rapid flash activities from displacing user applications from the LRU protection window (`< lru_protect_depth`), preserving user app state across auth redirects and deep links.

---

## 3. Decision Pipeline & Opportunistic Evaluation

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
   │      • Is foreground app in games.list? ─────────────────────► Set T_idle = 0
   │      • Is MemAvailable < MEM_CRITICAL_PERCENT? ──────────────► Set T_idle = 0
   │      • Screen OFF + screen_off_harvest > 60s? ───────────────► Set T_idle = 30s, LRU depth = 1
   │      • Screen OFF + screen_off_harvest <= 60s? ──────────────► Set T_idle = 60s, LRU depth = 1
   │      • Screen ON + in current app > 5 min? ──────────────────► Set T_idle = 60s, LRU depth = default
   │      • Nominal multitasking / Screen OFF (!harvest): ────────► Set T_idle = T_IDLE_DEFAULT_SEC, LRU depth = default
   │
   └── 2. Evaluate alive_apps in Memory:
          │
          ├── [Check 1] In static/dynamic exclusions? ────────► SKIP (Protected Role / Exclude)
          ├── [Check 2] Position in fg_lru < effective_lru? ──► SKIP (Active App Protection)
          └── [Check 3] (Instant::now() - last_active) < T_idle? ► SKIP (Young / Warm App)
          │
          ▼
      [Eviction Candidates Identified]
          │
          ├── Targeted Read: Parse RSS from /proc/<pid>/statm for qualified candidates only
          ├── Sort candidates descending by RSS (heaviest footprint first)
          ├── [Gate 4] min(oom_score_adj) < min_oom_score_adj?
          │            ├── YES: Mark ams_protected, skip spawn, refresh last_active, emit telemetry
          │            └── NO:  Execute cmd activity kill --user all <pkg>, purge alive_apps
```

### 3.1 Multi-Gate Rules

1. **Recency Protection Gate (`effective_lru_depth`):**
   * **Screen ON (or `screen_off_harvest=false`):** The last N packages visited in `fg_lru` (default: 3) are completely protected. This prevents closing apps during active switching loops (such as copying 2FA verification codes).
   * **Screen OFF (`screen_off_harvest=true`):** Decimated to **1** (protecting only the immediate foreground app active prior to screen lock).

2. **Adaptive Idle Age Gate (`T_idle`):** The package must have departed from the foreground (or running in the background since spawn) for at least `T_idle_effective`:

   `Δt = t_now - t_last_active >= T_idle_effective`

   * Background-spawned processes (`am_proc_start`) begin their clock at spawn and qualify only after crossing `T_idle_effective`.
   * Stale unindexed processes default to `t_idle = ∞` and qualify immediately if beyond `effective_lru_depth`.

3. **OOM Score Qualification Gate (`min_oom_score_adj`):**
   * The daemon queries `/proc/<pid>/oom_score_adj` for every live PID belonging to the candidate package.
   * If `min(oom_score_adj) < min_oom_score_adj`, the package is identified as `ams_protected`. The daemon skips the process spawn (`cmd activity kill`) to eliminate wasted fork/exec overhead and futile AMS rejections, while preserving full telemetry visibility across `--observe` and `--act` modes (`ams_protected: true`, `spawn_skipped: true`).
   * The package's `last_active` timestamp is refreshed in `alive_apps`, yielding the eviction slot to subsequent candidates on future ticks while maintaining a periodic audit heartbeat every `T_idle`.
   * Tunable in `daemon.conf` between `500` and `900` (default: `900`):
     * `900` (Conservative): Restricts evictions strictly to cached/idle processes (`CACHED_APP_MIN_ADJ = 900`). Shields processes with `oom_score_adj < 900` (e.g., active background services, audio players, background downloads).
     * `500` (Aggressive): Permits eviction of background services down to AOSP's framework limit (`SERVICE_ADJ = 500`). Maximizes reclaimable RAM for gaming or constrained devices.

### 3.2 Escalation Modes

* **Game Focus Entry:** If the incoming package matches `games.list`, `T_idle` drops to `0`. All non-excluded background apps beyond `effective_lru_depth` are evicted immediately to maximize physical RAM before the game engine allocates its heap.
* **Critical Memory Starvation:** If `/proc/meminfo` reports `MemAvailable < MEM_CRITICAL_PERCENT`, `T_idle` drops to `0` across standard application switches to prevent kernel direct-reclaim page stalls.
* **Screen-Off Deep Harvest:** During opportunistic screen-off maintenance on native Doze heartbeats (`device_idle_light_step`), `effective_lru_depth` drops to 1, and `T_idle` decays from 60s to 30s (once the screen has been off for > 60s), purging accumulated background memory while the display panel is unpowered.

### 3.3 Targeted RSS Density Sorting & Burst Capping

When multiple candidate packages qualify for eviction simultaneously:

1. The daemon reads column 2 (resident pages × `PAGE_SIZE`) from `/proc/<pid>/statm` **strictly for the qualified candidates**. Any PIDs returning 0 RSS (`ENOENT` caused by process exit missed during rare logcat ring buffer drops) are opportunistically purged from `pid_to_pkg` and `record.pids`. If all PIDs for a candidate have exited, the stale package entry is deleted from `alive_apps`—reconciling in-memory state without periodic `/proc` filesystem sweeps.
2. Surviving candidates are sorted in **descending order of RSS** (reclaiming the largest memory footprints first).
3. **Hardware-Scaled Burst Limit (`max_kills_per_pass`):** Only the top N candidates are evicted in a single pass to bound reactor latency and prevent Binder thread contention in `system_server`. Defaults auto-scale by physical RAM detected from `/proc/meminfo` at startup:
   * **<= 4.5 GB RAM:** 4 kills per pass (aggressive recovery on low-RAM devices).
   * **4.5 GB - 8.5 GB RAM:** 2 kills per pass (balanced mainstream devices).
   * **> 8.5 GB RAM:** 1 kill per pass (conservative eviction on high-headroom flagships).
   * Can be overridden at runtime via `max_kills_per_pass` in `daemon.conf`.

---

## 4. Provisional Tuning Constants (Telemetry Calibration)

The following parameters in `daemon.conf` are live-calibrated via inotify. Their default values serve as baseline estimates and are subject to calibration based on empirical traces recorded in `<base>/logs/operations.log`:

| Parameter | Provisional Default | Description & Calibration Target |
|---|---|---|
| `t_idle_sec` | `180` (3 minutes) | Base background idle threshold. Tuned against user app resume latency to avoid evicting apps returned to frequently. |
| `lru_protect_depth` | `3` packages | Depth of protected foreground history window. Protects active multitasking workflows. |
| `mem_critical_percent` | `10` (% of `MemTotal`) | RAM watermark triggering emergency idle bypass (`T_idle = 0s`). Correlated against `/proc/vmstat` `allocstall_normal` and `pgscan_direct`. |
| `fg_lru_max_depth` | `10` packages | Maximum depth of the foreground history ring buffer. |
| `screen_off_harvest` | `true` | Enables opportunistic screen-off maintenance during Doze heartbeats (`device_idle_light_step`). |
| `max_kills_per_pass` | Auto-scaled (`1`–`4`) | Maximum candidate packages evicted per pass (burst cap). Auto-scales by physical RAM: `<=4.5 GB` -> 4, `4.5–8.5 GB` -> 2, `>8.5 GB` -> 1. |
| `min_oom_score_adj` | `900` (`500`–`900`) | Minimum OOM score required for eviction eligibility. Clamped to `500..=900`. `900` targets cached/idle processes; `500` extends reclamation to background services. |

---

## 5. File Layout & Inotify Isolation

To prevent inotify feedback loops where log emission re-triggers configuration reloads, configurations and runtime logs are isolated into separate directories under `<base>` (resolved dynamically to `$MODPATH/mlmk/` when running as an AxManager module, with fallback to `/data/local/tmp/mlmk/` for standalone execution):

```text
<base>/
├── config/                  <-- Watched by inotify (TOKEN_INOTIFY)
│   ├── daemon.conf          <-- Volatile runtime tuning parameters (key=value)
│   ├── exclude.list
│   └── games.list
└── logs/                    <-- Unwatched; append-only telemetry sink
    └── operations.log
```

### 5.1 Configuration Files (`config/`)

#### Runtime Parameters (`config/daemon.conf`)

Live-calibrated parameters parsed without heap allocations via a 1 KB stack buffer:

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

Package names triggering Game Mode entry flushing (`T_idle -> 0s`). **Empty by default out of the box**; populated by the user for high-demand 3D gaming workloads:

```text
# Example game targets (file ships empty by default out of the box)
# com.miHoYo.GenshinImpact
# com.proximabeta.nikke
```

### 5.2 Telemetry Logging Format (`logs/operations.log`)

The daemon writes structured Newline-Delimited JSON (NDJSON) using standard userspace line buffering (`BufWriter`) without synchronous per-line `fsync`, minimizing flash wear. Individual process lifecycle spawns and exits are aggregated into `bg_summary` events to prevent log spam and disk thrashing.

```json
{"ts":1790255284675,"event":"screen_state","state":"OFF","active_duration_sec":1850}
{"ts":1790255285117,"event":"screen_state","state":"ON","off_duration_sec":420}
{"ts":1790255285118,"event":"bg_summary","interval_sec":420,"spawns":5,"deaths":3,"spawn_rss_kb":65536}
{"ts":1790255286893,"event":"fg_switch","pkg":"com.android.settings","component":"com.android.settings/.Settings$StorageUseActivity","prev_dur_ms":1240,"is_game":false}
{"ts":1790255286893,"event":"kill","pkg":"com.facebook.katana","pids":[3939],"rss_freed_est_kb":842324,"reason":"idle_expired","idle_sec":412,"lru_pos":5,"spawned":true}
{"ts":1790255287102,"event":"game_intrusion","pid":12763,"uid":10130,"pkg":"com.google.android.calculator","proc":"com.google.android.calculator","type":"service","rss_kb":45200,"excluded":false}
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

| Target / Operation | Mechanism / Scope | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean | CFS Preemptions |
|---|---|---|---|---|---|---|---|---|---|
| `procfs::read_oom_score_adj` | self, hot cache | 10,000 | 2.85 µs | **3.31 µs** | 10.69 µs | 14.15 µs | 531.69 µs | 4.32 µs | 1 |
| `procfs::read_oom_score_adj` | multi-PID pool (cold) | 10,000 | 4.69 µs | **8.69 µs** | 23.38 µs | 38.15 µs | 999.08 µs | 11.72 µs | 3 |
| `procfs::read_statm_rss_kb` | self, hot cache | 10,000 | 3.69 µs | **4.23 µs** | 20.31 µs | 40.08 µs | 5.71 ms | 8.86 µs | 4 |
| `procfs::read_statm_rss_kb` | multi-PID pool (cold) | 10,000 | 5.00 µs | **6.23 µs** | 10.46 µs | 14.77 µs | 450.38 µs | 7.07 µs | 0 |
| `procfs::read_meminfo_kb` | `/proc/meminfo` | 10,000 | 7.69 µs | **8.00 µs** | 9.46 µs | 12.62 µs | 377.54 µs | 8.50 µs | 0 |
| `telemetry::FormattedTime` | Stack buffer format | 10,000 | 76.0 ns | **307.0 ns** | 308.0 ns | 308.0 ns | 2.54 µs | 261.3 ns | 0 |
| `Instant::now` (Overhead) | Timer baseline | 10,000 | 0.0 ns | **231.0 ns** | 385.0 ns | 539.0 ns | 2.38 µs | 226.7 ns | 0 |

#### Architectural Key Takeaways:
1. **Cold Multi-PID VFS Access:** Cycling through a pool of external PIDs across the system shifts P50 latency from 3.31 µs (self, pinned dcache) to 8.69 µs (external PID, VFS dentry traversal). Even under cold multi-PID access, candidate qualification completes in under **9 microseconds per PID**.
2. **Tail Latency Root Cause:** Multi-millisecond Max outliers (e.g. 1.2–5.7 ms) were investigated using per-iteration thread CPU tracking (`CLOCK_THREAD_CPUTIME_ID`) and involuntary context switch counters (`ru_nivcsw`). In every outlier instance, actual thread CPU time remained < 50 µs while `ru_nivcsw` incremented, confirming that outliers are caused exclusively by **Linux CFS scheduler preemption**, not kernel VFS stalling.

### 6.3 End-to-End Daemon Benchmarks: Idle vs Active App-Switching Pipeline

Evaluated via `scripts/benchmark.sh` across both steady-state idle conditions ($N = 50$) and active live app-switching workloads ($N = 15$, cycling between Settings, Home, and Browser transitions):

| Metric | Workload Mode | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean |
|---|---|---|---|---|---|---|---|---|
| **Cold-start Discovery** | Initial Boot Indexing | 50 | 181.1 ms | **251.6 ms** | 289.6 ms | 343.5 ms | 343.5 ms | 251.6 ms |
| **Steady-State Memory (PSS)** | Idle Boot State | 50 | 1,081 kB | **1,109 kB** | 1,133 kB | 1,139 kB | 1,139 kB | 1,109 kB |
| **Active Pipeline Memory (PSS)**| Live App-Switch Traffic | 15 | 1,150 kB | **1,165 kB** | 1,183 kB | 1,183 kB | 1,183 kB | 1,165 kB |
| **Resident Set Size (RSS)** | Idle Boot State | 50 | 3,744 kB | **3,848 kB** | 3,944 kB | 4,012 kB | 4,012 kB | 3,850 kB |
| **Resident Set Size (RSS)** | Live App-Switch Traffic | 15 | 3,852 kB | **3,960 kB** | 4,044 kB | 4,044 kB | 4,044 kB | 3,965 kB |
| **Inter-Event Idle CPU Wakeups** | Post-Traffic Idle Window | 65 | 0 | **0** | 0 | 0 | 0 | 0.0 |

**Pipeline Memory Growth:** Active app-switching with populated `alive_apps`, `fg_lru`, `pkg_to_pids`, and `recent_deaths` tables increases steady-state PSS by only **+56 kB** (from 1,109 kB to 1,165 kB), confirming that the daemon easily remains under its 2.5 MB PSS budget even under heavy transition load. Inter-event wakeups remain strictly **0** once traffic pauses.

---

## 7. Licensing & Distribution

This project is licensed under the **GNU General Public License v3.0** (`GPL-3.0-only`). See the [`LICENSE`](../LICENSE) file for complete terms and legal text.


