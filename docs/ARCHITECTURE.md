# Architecture Specification & Technical Reference: mini-lmk

mini-lmk is a rootless, 100% single-threaded, event-driven memory management daemon operating under Android shell privileges (UID 2000, API 29–37+). It preempts kernel memory thrashing and native lmkd direct-reclaim stalls by tracking application idle durations via native system log events, evicting stale background applications cleanly through Android framework APIs.

---

## 1. Architectural Foundations (Fixed Contracts)

### 1.1 Execution Boundaries & Security Context

* **Identity:** UID `2000` (`shell`), GID `2000` (`shell`), supplementary GID `1007` (`log`). Rootless execution relying on platform shell privileges (accessible via ADB or Shizuku).
* **SELinux Domain:** `u:r:shell:s0` (Stock enforcing domain; zero root, KernelSU, Magisk, or custom sepolicy modifications required).
* **Single-Threaded & Fail-Fast:** Operates strictly on a single thread. It maintains no complex in-process reconnection state machines: if the logcat stream closes, yields `EPOLLHUP`, or encounters unrecoverable read errors, the daemon exits immediately (`exit(1)`). Process supervision and restarts are delegated externally to an init service or shell supervisor loop.
* **Footprint:** Single native binary compiled for `aarch64-linux-android` against Bionic `libc` (< 400 KB stripped ELF; < 4 MB RSS operational footprint).

### 1.2 Minimal 2-FD Epoll Reactor

The daemon runs a pure push-based event loop with zero polling wakeups. The execution thread sleeps inside the kernel via `epoll_wait()`, consuming **0.0% CPU** during idle intervals across exactly **two file descriptors**:

```
+------------------------------------------------------------------------+
|                              epoll_wait()                              |
+------------------------------------------------------------------------+
                        |                        |
                        v                        v
               [TOKEN_LOGCAT_PIPE]        [TOKEN_INOTIFY]
              (Native Event Stream)    (/data/local/tmp/mlmk/config/)
                        |                        |
                        v                        v
               Lifecycle State Machine    Hot-Reload Configs
```

| Token | Descriptor Type | Source / Path | Trigger Condition | Reactor Action |
|---|---|---|---|---|
| `TOKEN_LOGCAT_PIPE` | Non-blocking Pipe (`O_NONBLOCK`) | Output of child `logcat` | Log buffer write | Parse event tag; update in-memory lifecycle state; evaluate reaping gates. Exits process on EOF/HUP. |
| `TOKEN_INOTIFY` | Linux inotify | Watch on `/data/local/tmp/mlmk/config/` (`CLOSE_WRITE` \| `MOVED_TO`) | Config modified | Instantly reloads `exclude.list` and `games.list` in memory without dropping state. |

### 1.3 Unified Native Event Stream

System context is parsed from a single non-blocking pipe connected to logd's native binary events buffer:

```bash
logcat -b events -v tag -s wm_resume_activity am_resume_activity am_proc_start am_proc_died screen_toggled device_idle_light_step -T 1
```

#### Event Handling & Canonicalization Rules

* **`wm_resume_activity` & `am_resume_activity` (Foreground Focus Transitions):**
  * *Payload:* `[<user_id>, <token>, <task_id>, <component_name>]` (Handles `am_resume_activity` on API 29 and `wm_resume_activity` on API 29–37+).
  * *Parsing:* Component is formatted as `<package>/<activity>`. Token matching dynamically locates the component containing `/` without relying on brittle positional indices. Substring extraction prior to `/` yields the foreground package name directly.
  * *Action:* Updates the foreground LRU history stack, stamps the outgoing package departure time, and triggers the reaping pipeline (with potential Game Mode escalation).

* **`am_proc_start` (Process Creation & Idle Baseline):**
  * *Payload:* `[<user_id>, <pid>, <uid>, <process_name>, <spawn_type>, {<component>}]`
  * *Canonicalization:* Processes often have suffixed names (`com.whatsapp:pushreceiver`, `com.android.chrome:sandboxed_process0`). The daemon normalizes the process name to the base package:

    `pkg = process_name.split(':').first()`

  * *Action:* Records the PID and base package into memory with `last_active = Instant::now()`. Distinguishes between foreground activity launches (`is_fg_launch = is_fg || ev.spawn_type == "top-activity" || ev.spawn_type == "next-top-activity"`) and background spawns:
    * If foreground activity launch: defers evaluation to `wm_resume_activity` so that evictions are masked behind UI transition animations.
    * If background spawn (`!is_fg_launch`): immediately evaluates the reaping pipeline (unconstrained marker architecture). This eliminates the single-app inactivity deadlock during prolonged foreground sessions while avoiding continuous timers or polling.

* **`am_proc_died` (Process Eviction & Pure State Cleanup):**
  * *Payload:* `[<user_id>, <pid>, <process_name>, <oom_adj>, <reason>]`
  * *Action:* Drops the PID and package from memory tables (`pid_to_pkg`, `record.pids`) and increments background death counters. It strictly avoids invoking `evaluate_reaping_pipeline()`—process deaths return physical RAM to the kernel, so triggering evictions on process death would induce artificial eviction cascades.

* **`screen_toggled` (Display State Transitions):**
  * *Payload:* `0` (OFF) or `1` (ON).
  * *Action:* Updates internal power state metrics and timers. On screen power-off (`0`), immediately triggers a reaping pass with `lru_protect_depth = 1` to harvest stale background memory while the display is unpowered.

* **`device_idle_light_step` (Native Doze Heartbeat):**
  * *Payload:* Empty.
  * *Action:* Emitted by Android's `DeviceIdleController` every ~5 minutes during light idle maintenance. When the screen is unpowered, it triggers an opportunistic background reap aligned with native Doze wakeups with zero artificial timers.

### 1.4 Dynamic Role-Based System Immunity

The daemon discovers system defaults dynamically on startup and config reloads, completely avoiding hardcoded vendor package names:

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
* **Kill Dispatch (`evaluate_reaping_pipeline`):** Once `cmd activity kill` is dispatched for candidate `cand.pkg`, immediately purges `cand.pkg` from `alive_apps` to prevent duplicate kills across rapid back-to-back evaluations. Intentionally retains `pid_to_pkg` mappings until asynchronous `am_proc_died` arrives to ensure 100% accurate death telemetry.

### 2.1 Lazy / Opportunistic PID Retaining

A critical design trade-off in `mini-lmk` is the **deliberate deferral of PID liveliness checks for protected applications**:
* **Why Non-Candidates Are Not Probed:** In any running Android system, dozens of apps reside in protected states (LRU position $< \text{depth}$, or idle time $< T_{\text{idle}}$). Actively polling or probing `/proc/<pid>/statm` for all tracked PIDs on every event would continuously wake CPU cores, thrash VFS caches, and disrupt low-power C-states—violating `mini-lmk`'s zero-overhead guarantee.
* **Event-Driven Eager Cleanup:** Under normal execution, the logcat stream delivers `am_proc_died` events synchronously whenever processes exit, naturally unmapping PIDs and removing dead apps.
* **Opportunistic Candidate Reconciliation:** If an unmonitored or silent exit occurs, the PID is retained lazily in memory at zero cost until the enclosing package ages out of protection and qualifies as an eviction candidate. During candidate evaluation, targeted `/proc/<pid>/statm` probes verify PID existence. Dead PIDs and departed packages discovered during this probe are instantly purged (`dead_pids`, `dead_pkgs`, `record.pids.retain(...)`).
* **Guarantee:** Candidate ranking and RSS reclamation calculations are strictly based on verified alive PIDs, with 0% steady-state CPU overhead.

### 2.2 Trampoline & Ephemeral Activity Filtering

To protect user multi-tasking state against activity re-entrance and auth overlays (e.g. Google Sign-In `SignInHubActivity`, intent choosers, payment gateways, ad SDK trampolines):
* **Transient Session Detection:** When an app departs the foreground after $< 500\text{ ms}$ (`prev_dur_ms < 500`), it is classified as a transient trampoline rather than an intentional user application session.
* **LRU De-pollution:** The transient package is immediately pruned from `fg_lru`. This prevents rapid flash activities from displacing genuine user applications from the LRU protection window ($< \text{lru\_protect\_depth}$), preserving user app state across auth redirects and deep links.

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
          ├── [Check 3] (Instant::now() - last_active) < T_idle? ► SKIP (Young / Warm App)
          │
          ▼
      [Eviction Candidates Identified]
          │
          ├── Targeted Read: Parse RSS from /proc/<pid>/statm for qualified candidates only
          ├── Sort candidates descending by RSS (heaviest footprint first)
          └── Execute: cmd activity kill --user all <pkg>
```

### 3.1 Dual-Gate Rules

1. **Recency Protection Gate (`effective_lru_depth`):**
   * **Screen ON (or `screen_off_harvest=false`):** The last N packages visited in `fg_lru` (default: 3) are completely protected. This prevents closing apps during active switching loops (such as copying 2FA verification codes).
   * **Screen OFF (`screen_off_harvest=true`):** Decimated to **1** (protecting only the immediate foreground app active prior to screen lock).

2. **Adaptive Idle Age Gate (`T_idle`):** The package must have departed from the foreground (or running in the background since spawn) for at least `T_idle_effective`:

   `Δt = t_now - t_last_active >= T_idle_effective`

   * Background-spawned processes (`am_proc_start`) begin their clock at spawn and qualify only after crossing `T_idle_effective`.
   * Stale unindexed processes default to `t_idle = ∞` and qualify immediately if beyond `effective_lru_depth`.

### 3.2 Escalation Modes

* **Game Focus Entry:** If the incoming package matches `games.list`, `T_idle` drops to `0`. All non-excluded background apps beyond `effective_lru_depth` are evicted immediately to maximize physical RAM before the game engine allocates its heap.
* **Critical Memory Starvation:** If `/proc/meminfo` reports `MemAvailable < MEM_CRITICAL_PERCENT`, `T_idle` drops to `0` across standard application switches to prevent kernel direct-reclaim page stalls.
* **Screen-Off Deep Harvest:** When the display powers down, `lru_protect_depth` drops to 1, and `T_idle` decays from 60s to 30s, purging accumulated background memory while the display panel is off.

### 3.3 Targeted RSS Density Sorting & Burst Capping

When multiple candidate packages qualify for eviction simultaneously:

1. The daemon reads column 2 (resident pages × `PAGE_SIZE`) from `/proc/<pid>/statm` **strictly for the qualified candidates**. Any PIDs returning 0 RSS (`ENOENT` caused by process exit missed during rare logcat ring buffer drops) are opportunistically purged from `pid_to_pkg` and `record.pids`. If all PIDs for a candidate have exited, the stale package entry is deleted from `alive_apps`—achieving self-healing state reconciliation with zero periodic `/proc` filesystem sweeps.
2. Surviving candidates are sorted in **descending order of RSS** (reclaiming the largest memory footprints first).
3. **Hardware-Scaled Burst Limit (`max_kills_per_pass`):** Only the top $N$ candidates are evicted in a single pass to bound reactor latency and prevent Binder thread contention in `system_server`. Defaults auto-scale by physical RAM detected from `/proc/meminfo` at startup:
   * **$\le 4.5\text{ GB}$ RAM:** 4 kills per pass (aggressive recovery on low-RAM devices).
   * **$4.5\text{ GB} - 8.5\text{ GB}$ RAM:** 2 kills per pass (balanced mainstream devices).
   * **$> 8.5\text{ GB}$ RAM:** 1 kill per pass (conservative eviction on high-headroom flagships).
   * Can be overridden at runtime via `max_kills_per_pass` in `daemon.conf`.

---

## 4. Provisional Tuning Constants (Telemetry Calibration)

The following parameters are provisional configuration variables. Their default values serve as baseline estimates and are subject to calibration based on empirical traces recorded in `/data/local/tmp/mlmk/logs/operations.log`:

| Parameter | Provisional Default | Description & Calibration Target |
|---|---|---|
| `T_IDLE_DEFAULT_SEC` | `180` (3 minutes) | Base background idle threshold. Tuned against user app resume latency to avoid evicting apps returned to frequently. |
| `LRU_PROTECT_DEPTH` | `3` packages | Depth of protected foreground history window. Protects active multitasking workflows. |
| `MEM_CRITICAL_PERCENT` | `10%` of `MemTotal` | RAM watermark triggering emergency idle bypass. Correlated against `/proc/vmstat` `allocstall_normal` and `pgscan_direct`. |

---

## 5. File Layout & Inotify Isolation (`/data/local/tmp/mlmk/`)

To prevent inotify feedback loops where log emission re-triggers configuration reloads, configurations and runtime logs are isolated into separate directories:

```text
/data/local/tmp/mlmk/
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
```

#### Exclusions (`config/exclude.list`)

Package names immune from eviction under all conditions (one per line). **Empty by default out of the box**; populated by the user for critical background services or tools:

```text
# Example user exclusions (file ships empty by default out of the box)
# com.tailscale.ipn
# moe.shizuku.privileged.api
```

#### Game Profiles (`config/games.list`)

Package names triggering Game Mode entry flushing ($T_{\text{idle}} \to 0\text{s}$). **Empty by default out of the box**; populated by the user for high-demand 3D gaming workloads:

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

## 6. Licensing & Distribution

This project is licensed under the **GNU General Public License v3.0** (`GPL-3.0-only`). See the [`LICENSE`](../LICENSE) file for complete terms and legal text.

