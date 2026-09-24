# Architecture Specification & Technical Reference: `mini-lmk`

`mini-lmk` is an unprivileged, event-driven, zero-polling background daemon designed to proactively manage memory pressure on Android. It operates as a cooperative layer above the kernel and below `system_server`, evicting dormant cached applications before native `lmkd` triggers disruptive memory reclaim stalls.

---

## 1. Architectural Foundations (Fixed Contracts)

### 1.1 Execution Context & Privilege Boundary
* **User & Group:** UID `2000` (`shell`), GID `2000` (`shell`), with supplementary GID `1007` (`log`).
* **SELinux Domain:** `u:r:shell:s0` (strict stock Android MAC boundaries).
* **Zero Root Dependency:** Requires no root access, KernelSU, Magisk, or modified SELinux policies (`permissive=0`).
* **Binary Footprint:** A single, statically linked native binary built for `aarch64-linux-android` against Bionic `libc` (< 700 KB stripped; < 4 MB RSS runtime memory).

### 1.2 Epoll Reactor Architecture
The daemon runs a single-threaded Linux `epoll` reactor handling exactly three file descriptors. The daemon performs zero periodic timer sweeps; CPU utilization remains at 0.0% while the system sits idle.

```
+------------------------------------------------------------------------+
|                              epoll_wait()                              |
+------------------------------------------------------------------------+
           |                                  |                        |
           v                                  v                        v
  [TOKEN_LOGCAT_PIPE]                  [TOKEN_INOTIFY]       [TOKEN_RECONNECT_TIMER]
(Event Stream: logcat)              (/data/local/tmp/mlmk/)     (One-shot timerfd)
           |                                  |                        |
           v                                  v                        v
Lifecycle & Focus State               Hot-Reload Configs      Respawn Logcat Pipe
```

| Token | FD Type | Target / Mechanism | Trigger Condition | Reactor Action |
|---|---|---|---|---|
| `TOKEN_LOGCAT_PIPE` | Non-blocking Pipe | Child `logcat` process stdout | Incoming event payload | Parse tag; drive lifecycle state machine & reaping pipeline. |
| `TOKEN_INOTIFY` | Linux inotify | Watch on `/data/local/tmp/mlmk/` (`CLOSE_WRITE`, `MOVED_TO`) | File modification | Hot-reload `exclude.list` and `games.list` without restart. |
| `TOKEN_RECONNECT_TIMER` | Linux `timerfd` | Monotonic one-shot timer (`1000ms`) | Pipe termination / `EPOLLHUP` | Respawn child `logcat`, re-register stdout, disarm timer. |

### 1.3 Unified Native Event Stream
Rather than running multi-threaded log readers or spawning Java-based command-line wrappers (`cmd activity observe-foreground-process`), the daemon consumes a single unified pipe connected directly to `logd`'s native events ring buffer:

```bash
logcat -b events -v tag -s wm_resume_activity am_proc_start am_proc_died screen_toggled -T 1
```

#### Stream Parser Mechanics
Each log entry is parsed using zero-copy prefix matching (`I/<tag>: `):

1. **Foreground Focus (`wm_resume_activity`):**
   * **Raw Entry:** `I/wm_resume_activity: [0,86685218,244,com.whatsapp/.home.ui.HomeActivity]`
   * **Payload Structure:** `[<user_id>, <token>, <task_id>, <component_name>]`
   * **Extraction:** Component is formatted as `<package>/<activity>`. Extracting the substring prior to `/` yields the package name (`com.whatsapp`) with zero `/proc` reads.
   * **Role:** Updates the foreground LRU cache, measures previous foreground duration, and triggers the reaping pipeline.

2. **Process Spawn (`am_proc_start`):**
   * **Raw Entry:** `I/am_proc_start: [0,12763,10130,com.google.android.calculator,next-top-activity,{...}]`
   * **Payload Structure:** `[<user_id>, <pid>, <uid>, <process_name>, <spawn_type>, {<component>}]`
   * **Role:** Tracks process creation timestamp and initial memory footprint (`/proc/<pid>/statm`) without periodic directory scans. Distinguishes interactive launches (`top-activity`, `next-top-activity`) from background wakeups (`broadcast`, `service`, `content provider`).

3. **Process Eviction (`am_proc_died`):**
   * **Raw Entry:** `I/am_proc_died: [0,10857,com.shopeepay.id:CoreService,975,19]`
   * **Payload Structure:** `[<user_id>, <pid>, <process_name>, <oom_adj>, <reason>]`
   * **Role:** Automatically purges tracking records and calculates process lifespan ($\Delta t = t_{\text{died}} - t_{\text{start}}$) upon termination.

4. **Power State (`screen_toggled`):**
   * **Raw Entry:** `I/screen_toggled: 0` (Screen OFF) or `1` (Screen ON)
   * **Role:** Marks interactive vs. screen-off states. Enables accounting of background wakeups and memory creep during sleep periods.

### 1.4 Dynamic Role-Based System Exclusions
To prevent misfires across vendor skins (OneUI, HyperOS, ColorOS) and third-party customizations (Nova Launcher, FlorisBoard, Fcitx5), the daemon queries Android's `RoleManager` and `SettingsService` at startup and upon package modifications:

```
                          Dynamic Exclusions
                                  │
         ┌────────────────────────┼────────────────────────┐
         ▼                        ▼                        ▼
     cmd role                  cmd role              cmd settings
 android.app.role.HOME    android.app.role.DIALER    default_input_method
 (e.g. Nova Launcher)     (e.g. Google Phone)        (e.g. Gboard, Fcitx5)
         │                        │                        │
         └────────────────────────┼────────────────────────┘
                                  ▼
                   Merged Daemon Exclusions Cache
                   (+ User-defined exclude.list)
```

* **Live Defaults Discovered via `cmd role get-role-holders <ROLE>`:**
  * `android.app.role.HOME`: Active launcher application.
  * `android.app.role.DIALER`: Default telephony dialer.
  * `android.app.role.SMS`: Default messaging application.
* **Active Input Method (IME):** Discovered via `cmd settings get secure default_input_method`.
* **Live Wallpaper:** Discovered via `cmd settings get secure wallpaper_service` (exempted only if non-null).
* **Explicit Role Policy:** High-memory consumers such as `android.app.role.BROWSER` and `android.app.role.ASSISTANT` are **deliberately excluded** from automatic immunity. They remain subject to standard LRU depth and background idle thresholds unless explicitly protected by the user in `exclude.list`.

### 1.5 Process Eviction Contract
Termination is delegated to Android's `ActivityManagerService` via:

```bash
cmd activity kill --user 0 <package_name>
```

#### Behavioral Contract
* **Safety Invariant:** Invokes `killBackgroundProcesses()` inside `ActivityManagerService`. 
* **State Preservation:** AMS terminates processes only if they reside in dormant states (`curAdj >= 900` or idle services). App saved instance states, job schedules, pending syncs, and push notification tokens remain completely intact.
* **DAC/MAC Compliance:** Unlike raw POSIX `kill -9` (which produces `EPERM` for UID 2000 against app UIDs), `cmd activity kill` interacts directly over Binder IPC and is fully authorized for `shell`.

---

## 2. Decision Pipeline & Lifecycle Logic

Eviction evaluations execute **strictly upon context switch transitions** (`wm_resume_activity`), shifting processing overhead into natural UI transition windows.

```
Foreground Transition (wm_resume_activity)
   │
   ├─► 1. Update Focus History (push to fg_lru; prune > 10)
   ├─► 2. Update Departure Time (record Instant::now() for outgoing app)
   │
   ├─► 3. Assess Pressure Escalator
   │      └─► Is MemAvailable < MEM_CRITICAL_PERCENT?
   │            ├─► YES: Set T_idle = 0 (Emergency Drain)
   │            └─► NO:  Set T_idle = 180s (Proactive Drain)
   │
   ├─► 4. Is Incoming Package in games.list?
   │      └─► YES: Set T_idle = 0 (Game Entry Sweep)
   │
   └─► 5. Candidate Evaluation (/proc/<pid>)
          │
          ├── [Gate 1] UID >= 10000? ─────────────────────────► NO  (Skip system/root)
          ├── [Gate 2] In dynamic/static exclusions? ────────► YES (Skip protected)
          ├── [Gate 3] Position in fg_lru <= LRU_PROTECT_DEPTH? ► YES (Skip recent)
          ├── [Gate 4] Background idle time < T_idle? ────────► YES (Skip warm app)
          │
          ▼
      [Eviction Candidate Identified]
          │
          ├─► Read RSS from /proc/<pid>/statm
          ├─► Sort candidates descending by RSS
          └─► Execute: cmd activity kill --user 0 <pkg>
```

### 2.1 Dual-Gate Qualification Rules

#### Gate 1: Identity & Role Immunity
* **System Process Check:** `stat("/proc/<pid>").st_uid < 10000` is immediately ignored.
* **Exclusion Check:** If the package exists in `exclude.list` or `dynamic_system_exclusions` (Launcher, IME, Dialer, SMS, Live Wallpaper), it is bypassed.
* **Active App Protection:** The active foreground package (`current_fg`) is invariant and never eligible for reaping.

#### Gate 2: Recency & Depth Protection
* **Recent Task Immunity:** The daemon maintains a bounded queue `fg_lru` (depth $\le 10$). If candidate package position is $\le \text{LRU\_PROTECT\_DEPTH}$ (default: 3), it is immune. This guarantees that multi-tasking workflows (e.g., copying a 2FA OTP from an authenticator app into a web browser) suffer zero eviction.

#### Gate 3: Background Idle Age Threshold ($T_{\text{idle}}$)
* **Idle Duration:** Candidate package must have been departed from the foreground for at least $T_{\text{idle}}$ seconds ($\Delta t = \text{now} - t_{\text{departed}} \ge T_{\text{idle}}$).
* **Background Spawns:** Processes started via `am_proc_start` without taking the foreground are timestamped with their start time and must also satisfy $T_{\text{idle}}$ before qualification.

### 2.2 Escalators & Overrides

| Trigger Scenario | Condition | Reaping Threshold Override | Rationale |
|---|---|---|---|
| **Standard Multi-Tasking** | System memory nominal | $T_{\text{idle}} = 180\text{s}$, $\text{LRU} > 3$ | Allows normal back-and-forth task switching without thrashing. |
| **Game Session Start** | Target app matches `games.list` | $T_{\text{idle}} = 0\text{s}$, $\text{LRU} > 0$ | Flushes all background cached memory before the game allocates its heap. |
| **Critical RAM Starvation** | `MemAvailable < 10% MemTotal` | $T_{\text{idle}} = 0\text{s}$, $\text{LRU} > 3$ | Emergency eviction of stale apps before kernel enters direct reclaim. |

### 2.3 Candidate Ranking & Eviction Sort
When multiple candidate packages qualify for reaping during an evaluation window:
1. The daemon reads resident set size (RSS) via `/proc/<pid>/statm`.
2. Candidates are sorted in **descending order of RSS** ($\text{RSS}_{\text{cand}} \ge \dots$).
3. Eviction issues per-package kills (`cmd activity kill --user 0 <pkg>`) sequentially, reclaiming maximum physical memory with the fewest distinct IPC calls.

---

## 3. Provisional Tuning Constants (Explicitly Parameterized)

The following parameters are implemented as provisional configuration variables. Default values are baseline estimates subject to calibration following the initial 24–48 hour `telemetry.log` dataset collection:

| Parameter Constant | Provisional Value | Evaluation Metric / Tuning Hypothesis |
|---|---|---|
| `T_IDLE_DEFAULT_SEC` | `180` (3 minutes) | Target idle threshold. Monitored against app re-engagement latency: if user re-opens apps between 3–5 minutes frequently, increase to `300s`. |
| `LRU_PROTECT_DEPTH` | `3` apps | Protects the $N$ most recently visited applications. Tested against 2FA switching workflows. |
| `MEM_CRITICAL_PERCENT` | `10%` of `MemTotal` | Threshold where memory availability is declared critical. Monitored against `/proc/vmstat` `allocstall_normal` and `pgscan_direct` inflection points. |
| `GAME_SWEEP_DELAY_MS` | `60000` (60 seconds) | Sustained gameplay duration before re-verifying background state to clear delayed background creep. |
| `SPAWN_INTERCEPT_DELAY_MS` | `800` ms | Delay window before issuing an eviction against a mid-game background spawn (`broadcast`/`service`), allowing AMS to complete active binder handling. |
| `RECONNECT_BACKOFF_MS` | `1000` ms | Epoll timer delay before attempting to re-establish `logcat` stream following an unhandled EOF or child termination. |

---

## 4. Configuration Specification (`/data/local/tmp/mlmk/`)

The daemon manages configuration files in `/data/local/tmp/mlmk/`. Both files are dynamically watched via inotify (`TOKEN_INOTIFY`):

### 4.1 Exclusions (`exclude.list`)
Exact package names (one per line) exempt from termination under all conditions:
```text
# Important background services
com.tailscale.ipn
moe.shizuku.privileged.api
com.terminal
```
* Blank lines and lines beginning with `#` are ignored.
* If missing: empty exclusions are applied with a warning log.
* If unreadable: evictions are temporarily halted as a safety precaution.

### 4.2 Game Profiles (`games.list`)
Package names that trigger Game Mode entry flushing and game timer management:
```text
# High-priority game targets
com.shopee.id
com.miHoYo.GenshinImpact
```

---

## 5. Telemetry & Observation Schema (`telemetry.log`)

The daemon outputs structured Newline-Delimited JSON (NDJSON) to `/data/local/tmp/mlmk/telemetry.log`. Each event includes a millisecond epoch timestamp (`ts`):

```json
{"ts":1790255284675,"event":"screen_state","state":"OFF"}
{"ts":1790255285117,"event":"screen_state","state":"ON","off_duration_sec":0,"bg_spawns":0,"rss_accum_kb":0}
{"ts":1790255286893,"event":"fg_switch","pkg":"com.android.settings","component":"com.android.settings/.Settings$StorageUseActivity","prev_dur_ms":1240,"is_game":false}
{"ts":1790255286893,"event":"simulated_kill","pkg":"com.facebook.katana","pid":3939,"adj":905,"rss_freed_est_kb":842324,"reason":"idle_expired","idle_sec":9999,"lru_pos":99}
{"ts":1790255287102,"event":"proc_start","pid":12763,"uid":10130,"pkg":"com.google.android.calculator","type":"next-top-activity","rss_kb":45200,"screen_on":true}
{"ts":1790255290451,"event":"proc_died","pid":12763,"pkg":"com.google.android.calculator","adj":900,"reason":"kill background","lifespan_ms":3349}
```
