# mini-lmk

An event-driven userspace memory manager for Android 10+ (API 29–37+) running under Android shell privileges (UID 2000, non-root).

`mini-lmk` preempts kernel direct-reclaim thrashing and native `lmkd` stalls by proactively evicting stale background applications during natural foreground transition animations and background spawn events. Operating strictly via a single-threaded 2-file-descriptor `epoll` reactor, it maintains an operational footprint of **~1.12 MB PSS** with **0% idle CPU utilization**.

---

## Key Highlights

* **Rootless / Shell-Privileged:** Operates under standard Android shell permissions (UID `2000`, `u:r:shell:s0` via ADB or Shizuku). Requires zero root, KernelSU, Magisk, or custom SELinux modifications while retaining access to the framework's `cmd activity` IPC interface and `logcat` event buffers.
* **Zero-Allocation Hot Paths:** Parses binary logcat event streams and `/proc` metrics using stack-allocated buffers and zero-copy byte slicing. Zero heap allocations during steady-state reactor operations.
* **Animation-Masked Eviction:** Reaping evaluations triggered by application switching (`wm_resume_activity` / `am_resume_activity`) mask framework kill latency behind native 200–300 ms window transition animations.
* **Unconstrained Marker Architecture:** Intercepts background process creation (`am_proc_start`) while ignoring foreground task launches, resolving single-app inactivity deadlocks during extended stationary sessions without synthetic polling timers.
* **Non-Blocking Asynchronous Reaping:** Dispatches `cmd activity kill --user all <pkg>` asynchronously via `posix_spawn` with non-blocking child harvesting (`libc::waitpid(-1, ..., WNOHANG)`), keeping UI render thread latency flat.
* **External Process Supervision:** Follows fail-fast systems design. If the upstream logcat pipe yields `EOF` or `EPOLLHUP`, the daemon exits immediately and cleanly, delegating process resurrection to an external supervisor loop (`run-daemon.sh`).
* **Hardware-Scaled Burst Limits:** Automatically tunes eviction burst limits (`max_kills_per_pass`) to physical RAM detected at startup, protecting system_server Binder IPC queues from saturation.
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
    (logcat -b events -v tag)  (/data/local/tmp/mlmk/config/)
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
    └── Retains pid_to_pkg mappings (guarantees accurate am_proc_died telemetry)
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
├── run-daemon.sh            # Target hardware supervisor loop
├── docs/
│   └── ARCHITECTURE.md      # Complete architectural specification & reference
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

Configuration files reside under `/data/local/tmp/mlmk/config/` and are automatically hot-reloaded via `inotify` when modified:

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
```

### `exclude.list`

Package names shielded from termination under all conditions (one per line). **Empty by default out of the box**; populated by the user as needed:

```text
# Example user exclusions (file is empty by default out of the box)
# com.spotify.music
# moe.shizuku.privileged.api
```

### `games.list`

Applications that trigger immediate Game Mode memory reclamation ($T_{\text{idle}} \to 0\text{s}$) upon focus. **Empty by default out of the box**; populated by the user as needed:

```text
# Example game profiles (file is empty by default out of the box)
# com.miHoYo.GenshinImpact
# com.proximabeta.nikke
```

---

## Building

Cross-compilation targets `aarch64-linux-android` using Android NDK (API 29+ compatibility):

```bash
# Add rust target
rustup target add aarch64-linux-android

# Build optimized release binary
cargo build --release --target aarch64-linux-android

# Run unit test suite
cargo test --target aarch64-unknown-linux-gnu
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
| `--observe` | Run in observation mode (emits telemetry and simulates candidate kills; default). |
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
14:25:40.200   SCREEN_ON    --                         sleep=144s  bg_spawns=5  rss_added=+64MB
14:25:40.201   BG_SUMMARY   --                         interval=144s  spawns=5  deaths=4  rss_delta=+12MB
```

All operations are simultaneously written to `/data/local/tmp/mlmk/logs/operations.log` in NDJSON format, automatically rotating to `operations.log.old` upon exceeding 512 KB.

---

## Verified Device Performance

Profiled on physical target hardware (MediaTek MT6789 / Helio G99, Android 14 API 34, aarch64):

| Metric | Measured Baseline | Target Budget |
|---|---|---|
| **Steady-State PSS** | **1,120 kB (~1.12 MB)** | < 2,500 kB |
| **Private Dirty RAM** | **704 kB** | < 1,000 kB |
| **Idle CPU Utilization** | **0.017% (task-clock 3.4 ms / 20s)** | < 0.10% |
| **Event Loop Sleeping Ratio** | **99.97% (epoll_wait)** | > 98.0% |
| **Reap Loop Reactor Impact** | **~5.2 ms** (via non-blocking spawn) | < 20 ms |
| **99th Percentile UI Frame Time** | **22–36 ms** (13.8% drop in UI jank) | < 50 ms |
| **Steady-State Steady Allocations** | **0 bytes** (zero heap churn) | 0 bytes |

---

## License

This project is licensed under the **GNU General Public License v3.0** (`GPL-3.0-only`). See the [`LICENSE`](LICENSE) file for the complete license terms.
