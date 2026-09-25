# mini-lmk

An event-driven userspace memory manager for Android 7.0+ (API 24–37+) running under Android shell privileges (UID 2000, non-root).

`mini-lmk` preempts kernel direct-reclaim thrashing and native `lmkd` stalls by proactively evicting stale background applications during foreground transition animations and background spawn events. Operating strictly via a single-threaded 2-file-descriptor `epoll` reactor, it maintains an operational footprint of **~1.12 MB PSS** with **0% idle CPU utilization**.

---

## Key Highlights

* **Rootless / Shell-Privileged:** Operates under standard Android shell permissions (UID `2000`, `u:r:shell:s0` via ADB or Shizuku). Requires zero root, KernelSU, Magisk, or custom SELinux modifications while retaining access to the framework's `cmd activity` IPC interface and `logcat` event buffers.
* **Zero-Allocation Hot Paths:** Parses binary logcat event streams and `/proc` metrics using stack-allocated buffers and zero-copy byte slicing. Zero heap allocations during steady-state reactor operations.
* **Animation-Masked Eviction:** Reaping evaluations triggered by application switching (`wm_resume_activity` / `am_resume_activity`) mask framework kill latency behind native 200–300 ms window transition animations.
* **Background Spawn Triggering:** Intercepts background process creation (`am_proc_start`) while ignoring foreground task launches, resolving single-app inactivity deadlocks during extended stationary sessions without synthetic polling timers.
* **Non-Blocking Asynchronous Reaping:** Dispatches `cmd activity kill --user all <pkg>` asynchronously via `posix_spawn` with non-blocking child harvesting (`libc::waitpid(-1, ..., WNOHANG)`), keeping UI render thread latency flat.
* **External Process Supervision:** Follows fail-fast systems design. If the upstream logcat pipe yields `EOF` or `EPOLLHUP`, the daemon exits immediately and cleanly, delegating process resurrection to an external supervisor loop (`run-daemon.sh`).
* **Hardware-Scaled Burst Limits:** Sets eviction burst limits (`max_kills_per_pass`) to physical RAM detected at startup, protecting system_server Binder IPC queues from saturation.
* **Bounded Disk Footprint:** Dual-output telemetry sink maintains an aligned human-readable terminal table alongside structured NDJSON operations logging with automatic 512 KB log rotation.

---

## Architecture Overview

```
┌────────────────────────────────────────────────────────┐
│                      epoll_wait()                      │
└───────────────────────────┬────────────────────────────┘
                            │
              ┌─────────────┴─────────────┐
              │                           │
              ▼                           ▼
     [TOKEN_LOGCAT_PIPE]           [TOKEN_INOTIFY]
    (logcat -b events -v tag)     (<base>/config/)
              │                           │
              ▼                           ▼
    Zero-Copy Event Parser       Hot-Reload Configs
              │
    ┌─────────┴────────────────────────────────┐
    │                                          │
Foreground Switch                      Background Spawn
(wm_resume_activity)                   (am_proc_start)
    │                                          │
    ▼                                          ▼
[Evaluate 3-Gate Pipeline]             [If !is_fg_launch: Evaluate]
    │
    ├─► Gate 1: Static & Dynamic Exclusions (IME, Launcher, Dialer, SMS, exclude.list)
    ├─► Gate 2: LRU Recency Protection Window (lru_protect_depth)
    └─► Gate 3: Adaptive Idle Age (T_idle >= 180s, or 0s on Game Mode / Low RAM)
    │
    ▼
Sort Candidates Descending by RSS (/proc/<pid>/statm)
    │
    ▼
Dispatch: cmd activity kill --user all <pkg>
    │
    ├── Immediately evicts package from alive_apps (prevents duplicate kills)
    └── Retains pid_to_pkg mappings (enables attributing am_proc_died telemetry)
```

---

## Directory Structure

```text
mini-lmk/
├── .cargo/
│   └── config.toml          # Target configuration and linker rustflags
├── Cargo.toml               # Package manifest and release profile optimizations
├── LICENSE                  # GNU General Public License v3.0
├── README.md                # Project documentation and quickstart
├── package.sh               # Multi-ABI AxManager plugin packaging script
├── run-daemon.sh            # Target hardware supervisor loop
├── docs/
│   └── ARCHITECTURE.md      # Architectural specification & reference
├── package/
│   └── axmanager/           # AxManager / Axeron module configuration and scripts
└── src/
    ├── config.rs            # Runtime configuration parsing (daemon.conf)
    ├── hasher.rs            # In-tree 64-bit FNV-1a hasher (zero-dependency)
    ├── parser.rs            # Zero-allocation event log tokenizer and fallbacks
    ├── procfs.rs            # Stack-buffered /proc readers (statm, meminfo, cmdline)
    ├── telemetry.rs         # Dual-output TelemetrySink with 512 KB log rotation
    └── main.rs              # Epoll reactor, state machine, and eviction pipeline
```

---

## Configuration

Configuration files reside under `<base>/config/` (dynamically resolved to `$MODPATH/mlmk/config/` in AxManager, or `/data/local/tmp/mlmk/config/` for standalone ADB execution) and are reloaded via `inotify` when modified:

### `daemon.conf`

Live runtime parameters:

```ini
# Base background idle timeout before eviction eligibility (seconds)
t_idle_sec=180

# Number of recently visited foreground packages immune from eviction
lru_protect_depth=3

# Low-memory watermark (percentage of MemTotal) triggering emergency T_idle=0s
mem_critical_percent=10

# Maximum depth of the foreground history ring buffer
fg_lru_max_depth=10

# Deep screen-off harvesting (drops LRU depth to 1 and accelerates idle decay)
screen_off_harvest=true

# Maximum background apps evicted per reap pass (burst cap)
# Defaults auto-scale by physical RAM: <=4.5GB -> 4, 4.5GB-8.5GB -> 2, >8.5GB -> 1
# max_kills_per_pass=2

# Minimum oom_score_adj threshold required for eviction (range: 500-900, default: 900)
# 900: Conservative (cached & idle processes only; protects all background services)
# 500: Aggressive (matches AOSP SERVICE_ADJ; reclaims background services for games/heavy loads)
min_oom_score_adj=900
```

| Parameter | Default | Range / Scale | Description |
|---|---|---|---|
| `t_idle_sec` | `180` | `u64` (seconds) | Background idle age before qualifying for eviction. |
| `lru_protect_depth` | `3` | `usize` | Number of most recently used foreground apps shielded from eviction. |
| `mem_critical_percent` | `10` | `u64` (%) | RAM watermark triggering emergency idle bypass (`T_idle = 0s`). |
| `fg_lru_max_depth` | `10` | `usize` | Maximum size of the foreground LRU ring buffer. |
| `screen_off_harvest` | `true` | `bool` | Accelerates idle decay and narrows LRU depth to 1 during screen-off Doze cycles. |
| `max_kills_per_pass` | `2` (auto-scaled) | `1`–`4` | Eviction burst cap. Scaled by RAM: `<=4.5GB` -> 4, `4.5-8.5GB` -> 2, `>8.5GB` -> 1. |
| `min_oom_score_adj` | `900` | `500`–`900` | Minimum OOM score required for eviction. `900` protects services; `500` reclaims background services. |

### `exclude.list`

Package names shielded from termination under all conditions (one per line). **Empty by default out of the box**; populated by the user as needed:

```text
# Example user exclusions (file is empty by default out of the box)
# com.spotify.music
# moe.shizuku.privileged.api
```

### `games.list`

Applications that trigger immediate Game Mode memory reclamation (`T_idle -> 0s`) upon focus. **Empty by default out of the box**; populated by the user as needed:

```text
# Example game profiles (file is empty by default out of the box)
# com.miHoYo.GenshinImpact
# com.proximabeta.nikke
```

---

## Building

Cross-compilation targets `aarch64-linux-android` using Android NDK (API 24+ compatibility):

```bash
# Add rust target
rustup target add aarch64-linux-android

# Build optimized release binary
cargo build --release --target aarch64-linux-android

# Run unit test suite on host (e.g. aarch64-unknown-linux-gnu or x86_64-unknown-linux-gnu)
cargo test --target $(rustc -vV | sed -n 's/host: //p')
```

The release profile compiles with `opt-level = "z"`, fat LTO, symbol stripping, and single codegen units, generating a stripped native ELF under 400 KB.

---

## Running on Target

Deploy to standard Android shell (`adb shell` / UID 2000):

```bash
# 1. Push binary and initialize directory structure (empty lists out of the box)
adb push target/aarch64-linux-android/release/mini-lmk /data/local/tmp/mini-lmk
adb push run-daemon.sh /data/local/tmp/mlmk/run-daemon.sh
adb shell "chmod +x /data/local/tmp/mini-lmk /data/local/tmp/mlmk/run-daemon.sh"
adb shell "mkdir -p /data/local/tmp/mlmk/config /data/local/tmp/mlmk/logs"
adb shell "touch /data/local/tmp/mlmk/config/exclude.list /data/local/tmp/mlmk/config/games.list"

# 2. Run in observation mode (simulate evictions without executing kills)
adb shell /data/local/tmp/mini-lmk --observe

# 3. Run in active enforcement mode
adb shell /data/local/tmp/mini-lmk --act

# 4. Run via background supervisor loop
adb shell "nohup /data/local/tmp/mlmk/run-daemon.sh --act > /data/local/tmp/mlmk/logs/stdout.log 2>&1 &"
```

### Command-Line Arguments

| Flag | Description |
|---|---|
| `--observe` | Run in observation mode (emits telemetry and simulates candidate kills). |
| `--act` | Run in active enforcement mode (`cmd activity kill --user all <pkg>`). |
| `--json` | Output raw NDJSON directly to stdout instead of the formatted columnar table. |
| `-h`, `--help` | Display usage and help message. |

---

## Telemetry & Monitoring

Live operations are formatted into aligned columns on standard output:

```text
# TIME         EVENT        TARGET                     DETAIL / REASON
--------------------------------------------------------------------------------
14:22:01.120   FG_SWITCH    com.shopee.id              prev=com.android.settings (14.2s)
14:22:01.126   KILL         com.google.android.youtube rss=184MB  idle=410s  lru=4 [idle_expired]
14:23:15.800   SCREEN_OFF   --                         active_session=74.6s
14:25:40.200   SCREEN_ON    --                         sleep=144s
14:25:40.201   BG_SUMMARY   --                         interval=144s  spawns=5  deaths=4  rss_delta=+12MB
```

All operations are simultaneously written to `<base>/logs/operations.log` in NDJSON format, rotating to `operations.log.old` upon exceeding 512 KB.

---

## Verified Device Performance

Profiled on physical target hardware (Android 14 API 34, Linux kernel 5.10, aarch64):

### Empirical Microbenchmark Suite: Kernel-Direct Procfs Latency Distribution

Evaluated via the standalone, criterion-free `benches/microbench.rs` harness across 1,000 warm-up cycles and 10,000 timed iterations:

| Target / Operation | Mechanism / Scope | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean | CFS Preemptions |
|---|---|---|---|---|---|---|---|---|---|
| `procfs::read_oom_score_adj` | self, hot cache | 10,000 | 2.85 µs | **3.31 µs** | 10.69 µs | 14.15 µs | 531.69 µs | 4.32 µs | 1 |
| `procfs::read_oom_score_adj` | multi-PID pool (cold) | 10,000 | 4.69 µs | **8.69 µs** | 23.38 µs | 38.15 µs | 999.08 µs | 11.72 µs | 3 |
| `procfs::read_statm_rss_kb` | self, hot cache | 10,000 | 3.69 µs | **4.23 µs** | 20.31 µs | 40.08 µs | 5.71 ms | 8.86 µs | 4 |
| `procfs::read_statm_rss_kb` | multi-PID pool (cold) | 10,000 | 5.00 µs | **6.23 µs** | 10.46 µs | 14.77 µs | 450.38 µs | 7.07 µs | 0 |
| `procfs::read_meminfo_kb` | `/proc/meminfo` | 10,000 | 7.69 µs | **8.00 µs** | 9.46 µs | 12.62 µs | 377.54 µs | 8.50 µs | 0 |
| `telemetry::FormattedTime` | Stack buffer timestamp | 10,000 | 76.0 ns | **307.0 ns** | 308.0 ns | 308.0 ns | 2.54 µs | 261.3 ns | 0 |
| `Instant::now` (Overhead) | Measurement baseline | 10,000 | 0.0 ns | **231.0 ns** | 385.0 ns | 539.0 ns | 2.38 µs | 226.7 ns | 0 |

* **Cold Multi-PID Pool Access:** Reading across a dynamic pool of external system/user PIDs incurs ~8.7 µs P50 latency (vs 3.3 µs for self), comfortably qualifying multi-PID candidate batches in microseconds.
* **Tail Latency Preemption Diagnostics:** Multi-millisecond Max outliers were verified via `CLOCK_THREAD_CPUTIME_ID` and `ru_nivcsw` counters to be caused by OS CFS scheduler thread preemption (actual thread execution time remained < 50 µs), not kernel VFS latency.

### End-to-End Daemon Benchmarks: Idle vs Active App-Switching Pipeline

Evaluated via `scripts/benchmark.sh` across both steady-state idle conditions ($N = 50$) and active live app-switching workloads ($N = 15$, cycling between Settings, Home, and Browser transitions):

| Metric | Workload Mode | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean |
|---|---|---|---|---|---|---|---|---|
| **Cold-start Discovery** | Initial Boot Indexing | 50 | 181.1 ms | **251.6 ms** | 289.6 ms | 343.5 ms | 343.5 ms | 251.6 ms |
| **Steady-State Memory (PSS)** | Idle Boot State | 50 | 1,081 kB | **1,109 kB** | 1,133 kB | 1,139 kB | 1,139 kB | 1,109 kB |
| **Active Pipeline Memory (PSS)**| Live App-Switch Traffic | 15 | 1,150 kB | **1,165 kB** | 1,183 kB | 1,183 kB | 1,183 kB | 1,165 kB |
| **Resident Set Size (RSS)** | Idle Boot State | 50 | 3,744 kB | **3,848 kB** | 3,944 kB | 4,012 kB | 4,012 kB | 3,850 kB |
| **Resident Set Size (RSS)** | Live App-Switch Traffic | 15 | 3,852 kB | **3,960 kB** | 4,044 kB | 4,044 kB | 4,044 kB | 3,965 kB |
| **Inter-Event Idle CPU Wakeups** | Post-Traffic Idle Window | 65 | 0 | **0** | 0 | 0 | 0 | 0.0 |

* **Live Workload Memory Cost:** Populating `alive_apps`, `fg_lru`, `pkg_to_pids`, and `recent_deaths` tables during active app-switching only increases steady-state PSS by **+56 kB** (to 1,165 kB), remaining well below the 2.5 MB budget.
* **Idle Wakeup Verification:** The single-threaded `epoll_wait` reactor produces strictly **0 CPU wakeups** during idle intervals between event bursts.

### Low-UID System Package Safety (Empirical AMS Verification)

Empirically verified on Android 14 that `cmd activity kill --user 0` terminates cached system packages (`uid < 10000`, e.g. `com.android.keychain` PID 29506 killed and terminated; `com.sprd.validationtools` PID 20352 killed and respawned). The daemon's fail-closed `uid >= 10000` guard prevents terminating critical platform services.

### Reproducing Benchmarks

The benchmark suite is completely automated and reproducible directly on Android hardware (API 29+, rootless `sh` compatible):

```bash
# 1. Cross-compile release daemon and standalone microbenchmark harness
cargo build --release
cargo build --release --bench microbench

# 2. Push artifacts and runner script to Android device
adb push target/aarch64-linux-android/release/mini-lmk /data/local/tmp/mini-lmk
adb push $(ls -t target/aarch64-linux-android/release/deps/microbench-* | grep -v '\.d$' | head -n 1) /data/local/tmp/microbench
adb push scripts/benchmark.sh /data/local/tmp/benchmark.sh

# 3. Execute microbenchmark suite (multi-PID pool + dumpsys IPC comparison)
adb shell "chmod 755 /data/local/tmp/mini-lmk /data/local/tmp/microbench /data/local/tmp/benchmark.sh && /data/local/tmp/microbench -n 10000 --dumpsys-iters 30"

# 4. Execute end-to-end active app-switching workload benchmark
adb shell "/data/local/tmp/benchmark.sh -n 15 -a"
```

---

## License

This project is licensed under the **GNU General Public License v3.0** (`GPL-3.0-only`). See the [`LICENSE`](LICENSE) file for the complete license terms.
