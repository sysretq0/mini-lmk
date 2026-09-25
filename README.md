# mini-lmk

An event-driven userspace memory manager for Android 7.0+ (API 24+) running under Android shell privileges (UID 2000, non-root).

`mini-lmk` preempts kernel direct-reclaim thrashing and native `lmkd` stalls by proactively evicting stale background applications during foreground transition animations and background spawn events. Operating strictly via a single-threaded 2-file-descriptor `epoll` reactor, it maintains an operational footprint of **~1.07 MB PSS** (P50 1,099 kB, see [Verified Device Performance](#verified-device-performance)) with **0 idle CPU wakeups** between events.

---

## Key Highlights

* **Rootless / Shell-Privileged:** Operates under standard Android shell permissions (UID `2000`, `u:r:shell:s0` via ADB or Shizuku). Requires zero root, KernelSU, Magisk, or custom SELinux modifications while retaining access to the framework's `cmd activity` IPC interface and `logcat` event buffers.
* **Stack-Buffered Tokenizer & Procfs Readers:** Parses incoming logcat event lines and `/proc` metrics (`statm`, `meminfo`, `oom_score_adj`) using fixed stack-allocated buffers and string slicing, avoiding heap allocations in the log tokenizing and procfs query paths.
* **Event-Sourced Timekeeping (Zero Clock Syscalls per Event):** Consumes `logcat -b events -v epoch` and derives `now` from the framework event's own millisecond timestamp, so no `clock_gettime` is sampled per dispatched event and telemetry `ts` is exactly aligned with Activity Manager event time. Idle-age, LRU, and respawn math runs entirely on `u64` epoch milliseconds (line format and clock-step semantics: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) §1.3, §2.3).
* **Wall-Clock Jump Compensation:** A `clock_jump_ms` detector distinguishes real `settimeofday()`/NTP steps from ordinary quiet gaps between events, and shifts every stored anchor (`alive_apps`, `recent_deaths`, screen-state, game-session, session-start) by the same delta, so elapsed ages survive the correction without a spurious reap pass; the detector is seeded from the bootstrap clock read, so the RTC-to-NTP step is caught on the first event.
* **Transition-Triggered Eviction:** Reaping evaluations are event-driven, triggered by foreground activity switches (`wm_resume_activity` / `am_resume_activity`), background process creations (`am_proc_start`), and Doze light-idle steps (`device_idle_light_step`) rather than periodic polling timers.
* **Background Spawn Triggering:** Intercepts background process creation (`am_proc_start`) while ignoring foreground task launches, resolving single-app inactivity deadlocks during extended stationary sessions without synthetic polling timers.
* **Non-Blocking Asynchronous Reaping:** Dispatches eviction commands (`cmd activity kill --user all <pkg>`) asynchronously via the standard library (`std::process::Command::spawn()`) and reaps child processes non-blocking (`libc::waitpid(-1, ..., WNOHANG)`), preventing subprocess execution from blocking the epoll reactor.
* **Fail-Closed System Safety:** Index-only cold boot discovery and strict `uid >= 10000` guards guarantee low-UID system daemons and platform services are never terminated.
* **External Process Supervision:** Follows fail-fast systems design. If the upstream logcat pipe yields `EOF` or `EPOLLHUP`, the daemon exits immediately and cleanly, delegating process resurrection to an external supervisor loop (`run-daemon.sh`).
* **Hardware-Scaled Burst Limits:** Sets eviction burst limits (`max_kills_per_pass`) to physical RAM detected at startup, protecting system_server Binder IPC queues from saturation.
* **Output-Gated Telemetry:** The dual-output sink builds both formatter halves lazily, so each one costs nothing unless it has somewhere to go: the aligned terminal table runs only when fd 1 is actually a terminal, and the JSON only when `--json` was asked for or the log is open. A background daemon that redirects stdout to `/dev/null` therefore pays no `localtime_r` and no table `String`, and `--no-log` additionally drops the JSON formatting. `operations.log` is flushed once per `epoll_wait` batch — and on every exit path — instead of once per record, 512 KB rotation bounds disk usage, and `log_enabled` / `--no-log` switch the file off entirely without touching any decision the daemon makes.

---

## Architecture Overview

```text
┌────────────────────────────────────────────────────────┐
│                      epoll_wait()                      │
└───────────────────────────┬────────────────────────────┘
                            │
              ┌─────────────┴─────────────┐
              │                           │
              ▼                           ▼
     [TOKEN_LOGCAT_PIPE]           [TOKEN_INOTIFY]
    (logcat -b events -v epoch)   (<base>/config/)
              │                           │
              ▼                           ▼
 Epoch + Event Parser            Hot-Reload Configs
 (zero-copy, no clock syscall)
              │
    ┌─────────┴────────────────────────────────┐
    │                                          │
Foreground Switch                      Background Spawn
(wm_resume_activity)                   (am_proc_start)
    │                                          │
    ▼                                          ▼
[Evaluate 4-Gate Pipeline]             [If !is_fg_launch: Evaluate]
    │
    ├─► Gate 1: Static & Dynamic Exclusions (IME, Launcher, Dialer, SMS, exclude.list)
    ├─► Gate 2: LRU Recency Protection Window (lru_protect_depth)
    ├─► Gate 3: Adaptive Idle Age (T_idle >= 180s, 10s grace window on Game / Low RAM)
    │
    ▼
Sort Candidates Descending by RSS (/proc/<pid>/statm)
    │
    ▼
[Gate 4: OOM Score Qualification] (min(oom_score_adj) < min_oom_score_adj?)
    │
    ├── YES: Candidate is ams_protected
    │        ├── Skip cmd activity kill spawn (zero fork/exec IPC overhead)
    │        ├── Refresh last_active in alive_apps (yields slot, retains audit heartbeat)
    │        └── Emit KILL_SKIP / SIM_KILL telemetry
    │
    └── NO:  Dispatch: cmd activity kill --user all <pkg>
             ├── Immediately evicts package from alive_apps (prevents duplicate kills)
             └── Retains pid_to_pkg mappings (async am_proc_died consumes them for state cleanup and respawn detection)
```

---

## Directory Structure

```text
mini-lmk/
├── .cargo/
│   └── config.toml          # Target configuration and linker rustflags
├── .github/
│   └── workflows/
│       └── build.yml        # Multi-ABI CI/CD build & packaging workflow
├── benches/
│   └── microbench.rs        # Standalone kernel-direct procfs microbenchmark suite
├── docs/
│   └── ARCHITECTURE.md      # Architectural specification & reference
├── package/
│   └── axmanager/           # AxManager / Axeron module configuration and scripts
├── scripts/
│   └── benchmark.sh         # On-device end-to-end workload benchmarking runner
├── src/
│   ├── config.rs            # Runtime configuration parsing (daemon.conf)
│   ├── hasher.rs            # In-tree 64-bit FNV-1a hasher (zero-dependency)
│   ├── parser.rs            # Zero-copy epoch logcat dispatcher and event fallbacks
│   ├── procfs.rs            # Stack-buffered /proc readers (statm, meminfo, oom_score_adj)
│   ├── telemetry.rs         # Dual-output TelemetrySink: isatty-gated table, 512 KB rotation, log_enabled off switch
│   └── main.rs              # Epoll reactor, state machine, and eviction pipeline
├── Cargo.lock               # Deterministic dependency manifest
├── Cargo.toml               # Package manifest and release profile optimizations
├── CHANGELOG.md             # Project release history & changelog
├── LICENSE                  # GNU General Public License v3.0
├── package.sh               # Multi-ABI AxManager plugin packaging script
├── README.md                # Project documentation and quickstart
└── run-daemon.sh            # Module supervisor loop (invoked by the installer's service.sh)
```

---

## Configuration

Configuration files reside under `<base>/config/` (dynamically resolved to `$MODPATH/mlmk/config/` in AxManager, or `/data/local/tmp/mlmk/config/` for standalone ADB execution) and are reloaded via `inotify` when modified. The installer creates `exclude.list` and `games.list` (both empty); `daemon.conf` is **not** shipped — until you create it, the compiled defaults in the table below are in effect, and an unreadable file silently keeps them. The module zip itself ships no configuration: `customize.sh` recreates `config/` and `logs/` and touches the two lists on every flash, so treat a hand-written `daemon.conf` (and any `log_enabled` in it) as not surviving a module upgrade and re-check it after flashing; the standalone `/data/local/tmp/mlmk/` tree is outside the module directory and is left alone.

### `daemon.conf`

Live runtime parameters. The first seven are memory-management policy; `log_enabled` is the telemetry switch:

```ini
# Base background idle timeout before eviction eligibility (seconds)
t_idle_sec=180

# Number of recently visited foreground packages immune from eviction
lru_protect_depth=3

# Low-memory watermark (percentage of MemTotal) triggering emergency T_idle=10s grace window
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

# Append records to logs/operations.log. Telemetry only: kill decisions, the terminal
# table and the --json stream are unaffected. --no-log overrides this per process.
# log_enabled=true
```

| Parameter | Default | Range / Scale | Description |
|---|---|---|---|
| `t_idle_sec` | `180` | `u64` (seconds) | Background idle age before qualifying for eviction. |
| `lru_protect_depth` | `3` | `usize` | Number of most recently used foreground apps shielded from eviction. |
| `mem_critical_percent` | `10` | `u64` (%) | RAM watermark triggering emergency idle bypass (`T_idle = 10s` grace window). |
| `fg_lru_max_depth` | `10` | `usize` | Maximum size of the foreground LRU ring buffer. |
| `screen_off_harvest` | `true` | `bool` | Accelerates idle decay and narrows LRU depth to 1 during screen-off Doze cycles. |
| `max_kills_per_pass` | `2` (auto-scaled) | `usize` >= 1 | Eviction burst cap. Default scales by RAM: `<=4.5GB` -> 4, `4.5-8.5GB` -> 2, `>8.5GB` -> 1. |
| `min_oom_score_adj` | `900` | `500`–`900` | Minimum OOM score required for eviction. `900` protects services; `500` reclaims background services. |
| `log_enabled` | `true` | `bool` | Whether `operations.log` is written. The one non-policy key in the file — see [Turning the Log Off](#turning-the-log-off). |

### `exclude.list`

Package names shielded from termination under all conditions (one per line). **Empty by default out of the box**; populated by the user as needed:

```text
# Example user exclusions (file is empty by default out of the box)
# com.spotify.music
# moe.shizuku.privileged.api
```

### `games.list`

Applications that trigger immediate Game Mode memory reclamation (`T_idle -> 10s` grace window) upon focus. **Empty by default out of the box**; populated by the user as needed:

```text
# Example game profiles (file is empty by default out of the box)
# com.miHoYo.GenshinImpact
# com.proximabeta.nikke
```

---

## Building

Cross-compilation targets `aarch64-linux-android` (alongside `armv7-linux-androideabi`, `x86_64-linux-android`, and `i686-linux-android`) using Android NDK (API 24+ compatibility):

```bash
# Add rust target
rustup target add aarch64-linux-android

# Build optimized release binary
cargo build --release --target aarch64-linux-android

# Run unit test suite on host (e.g. aarch64-unknown-linux-gnu or x86_64-unknown-linux-gnu)
cargo test --target $(rustc -vV | sed -n 's/host: //p')
```

The release profile compiles with `opt-level = "z"`, fat LTO, `panic = "abort"`, symbol stripping, and single codegen units. Stripped ELF size bounds per ABI are in `docs/ARCHITECTURE.md` §1.

---

## Running on Target

Deploy to standard Android shell (`adb shell` / UID 2000):

```bash
# 1. Push binary and initialize directory structure (empty lists out of the box)
adb push target/aarch64-linux-android/release/mini-lmk /data/local/tmp/mini-lmk
adb shell "chmod +x /data/local/tmp/mini-lmk"
adb shell "mkdir -p /data/local/tmp/mlmk/config /data/local/tmp/mlmk/logs"
adb shell "touch /data/local/tmp/mlmk/config/exclude.list /data/local/tmp/mlmk/config/games.list"

# 2. Run in observation mode (simulate evictions without executing kills)
adb shell /data/local/tmp/mini-lmk --observe

# 3. Run in active enforcement mode
adb shell /data/local/tmp/mini-lmk --act

# 4. Run under a background supervisor loop (restarts the daemon every 2s after an exit)
adb shell "nohup sh -c 'while /data/local/tmp/mini-lmk --act; do sleep 2; done' > /data/local/tmp/mlmk/logs/stdout.log 2>&1 &"
```

> **Note on `run-daemon.sh`:** this supervisor script targets the installed module layout only — it resolves the binary from `$MODPATH/system/bin/mini-lmk` or `$MODPATH/bin/<abi>/mini-lmk` and exports `MODPATH` (which also moves the daemon's base directory to `<script dir>/mlmk`). Copied next a bare binary it exits `binary not found`; use the inline loop above for standalone ADB runs. Every argument is forwarded to the binary, so `sh run-daemon.sh --act --no-log` reaches the daemon intact; with no arguments it starts `--act`, which is what the installer's `service.sh` gets.

### Command-Line Arguments

An operating mode (`--observe` or `--act`) must be explicitly specified:

| Flag | Description |
|---|---|
| `--observe` | Run in observation mode (emits telemetry and simulates candidate kills; safe mode). |
| `--act` | Run in active enforcement mode (`cmd activity kill --user all <pkg>`). |
| `--json` | Send NDJSON to stdout instead of the columnar table. stdout is a machine contract in this mode: no banner, no table, one record per line. Without it, the table is written only when fd 1 is a terminal (§ [Telemetry & Monitoring](#telemetry--monitoring)). |
| `--no-log` | Never write `operations.log` (see [Telemetry & Monitoring](#telemetry--monitoring)). Overrides `log_enabled` in `daemon.conf` for this process; stdout surfaces are unaffected. |
| `-h`, `--help` | Display usage and help manual. |

---

## Telemetry & Monitoring

Live operations are formatted into aligned columns on standard output:

```text
# TIME         EVENT        TARGET                     DETAIL / REASON
--------------------------------------------------------------------------------
14:22:01.120   FG_SWITCH    com.shopee.id              prev=com.android.settings (14.2s)
14:22:01.126   KILL         com.google.android.youtube rss=184MB  idle=410s  adj=950  lru=4 [idle_expired]
14:22:01.127   KILL_SKIP    com.spotify.music          rss=312MB  idle=200s  adj=200  lru=5 [idle_expired] (ams_protected: spawn skipped)
14:23:15.800   SCREEN_OFF   --                         active_session=74.6s
14:25:40.200   SCREEN_ON    --                         sleep=144s
14:25:40.201   BG_SUMMARY   --                         interval=144s  spawns=5  deaths=4  rss_delta=+12MB
```

All operations are simultaneously written to `<base>/logs/operations.log` in structured NDJSON format (including the event-sourced `ts` epoch-millisecond timestamp, `oom_score_adj`, `ams_protected`, and `spawn_skipped` fields), rotating to `operations.log.old` upon exceeding 512 KB, unless switched off by `log_enabled` or `--no-log` (see [Turning the Log Off](#turning-the-log-off)).

### Where Each Format Actually Goes

The table above is for a human at a terminal. Both formatter halves are closures, so a format that has nowhere to go is never built:

| Invocation | stdout | `operations.log` | stdout `write(2)` | log `write(2)` |
|---|---|---|---|---|
| `mini-lmk --observe` on a terminal | terminal table | NDJSON | 1 per record | 1 per event batch |
| `mini-lmk --observe --json` on a terminal | NDJSON | NDJSON | 1 per record | 1 per event batch |
| `service.sh` (stdout → `/dev/null`) | *nothing* | NDJSON | 0 | 1 per event batch |
| `mini-lmk --observe --json \| axcore` | NDJSON | NDJSON | 1 per record | 1 per event batch |
| `mini-lmk --observe --no-log` | terminal table (or NDJSON with `--json`) | *nothing* | 1 per record | 0 |

An *event batch* is one `epoll_wait` wakeup: the pipe read loop drains everything logcat has buffered, dispatches every complete line, and the reactor flushes once at the end of the batch, so a burst of lifecycle events costs a single `write(2)` — unless it overflows the 8 KB buffer, which adds one write per 8 KB (~40 records) — and a wakeup that dispatched nothing costs none, because flushing an empty buffer performs no syscall.

`isatty(1)` decides the human surface — the column header and the event rows — and nothing else. `--json` is a machine contract, so it stays on even when stdout is a pipe or `/dev/null`, and the `Indexed` / `Monitoring FDs` progress banners are gated on nothing else (which is how `scripts/benchmark.sh` still times cold-start discovery from a redirected stdout). The `=== mini-lmk daemon ===` line follows the table rather than the banners: it prints only where the table prints, so a redirected stdout gets the progress banners or — under `--json` — the NDJSON contract, never a stray human header.

### Turning the Log Off

A `log_enabled` key in `daemon.conf` (default `true`) and a `--no-log` flag decide whether `operations.log` is opened at all, and nothing else. No kill decision, no terminal table, and no `--json` stream changes when the file stops, so the only reason to own an off switch is an unbounded growth risk the 512 KB rotation does not cover — a device where the `/data` partition is tight, or a diagnosis that should not leave a trail. The config value is re-read on every `inotify` reload, so it can be flipped while the daemon runs from the same tooling that edits `t_idle_sec`; the flag wins over the file and a reload cannot undo it, because a daemon started with `--no-log` is a decision about that process.

Records emitted while the log is off are dropped, not queued — the same thing that already happens when the file cannot be opened — and the `config_reload` record announcing the change is emitted while the switch is still in its previous state when disabling and after it when enabling, so it always lands in the file it describes. Re-enabling re-reads the file size from the inode rather than trusting the daemon's own byte count, which cannot move while the log is closed, so a file that grew or was replaced during a disabled gap still rotates at the cap and a truncated one is not rotated early — checked on the device by growing `operations.log` to 600 KB while the switch was off: re-enabling moved it to `operations.log.old` and started the new file at one record. The first config load of every run is recorded even when `daemon.conf` is absent or matches the defaults, so the first line of the file is always the configuration actually in force. That record is *held* until `run()` has printed the table header rather than emitted where it is discovered — emitting it during construction put a data row above the header it belongs to, eleven lines up among the startup banners — so on a terminal it appears as the first row of the table, and a daemon that dies during initialisation still gets it onto disk through the fatal path.

What the switch actually buys, measured on the reference device: a daemon started with `log_enabled=false` or `--no-log` never creates `operations.log` at all. With the file open, `strace` sees exactly one `write(2)` on the log descriptor for the whole of a 70 s idle run — the 224-byte `config_reload` record written at startup — and nothing afterwards until an event happens; the other 15 writes in that trace go nowhere near the log: 14 are stdout lines redirected to `/dev/null` (the 13 startup `[CONFIG]`/`[SYSTEM]`/`[DAEMON]` lines, plus `[DAEMON] Shutdown signal received` once the run is stopped) and one is a `logcat` child diagnostic on its own descriptor. Three `daemon.conf` edits that each changed a value produced three records and three writes, and a fourth rewrite that changed nothing produced neither: one `inotify` reload is one batch, so a sparse event costs one flush. Logging costs disk only in proportion to real activity, which is why the 512 KB cap and the off switch are belt-and-braces rather than the main defence.

### Write Discipline

`write_to_log` appends into an 8 KB `BufWriter` and no longer flushes per line. The reactor flushes once after each `epoll_wait` batch, and termination goes through a single flush-then-exit helper because `std::process::exit` skips destructors — a failure path that forgot to flush would lose the run's last records silently, and there is no TTY on a daemon to notice. The bounded cost is that a `panic = "abort"` crash between the two can lose the records still in the buffer — at most the events of the batch being dispatched. `fsync` is never called, so durability against power loss is unchanged from the previous per-line policy.

Measured on the reference device (`benches/microbench.rs`, 10,000 iterations, one 190-byte `fg_switch` record per iteration; the second row is the shipped record path — the flush lands on the batch boundary, outside the timed call):

| Policy | P50 | Mean | P99 |
|---|---|---|---|
| flush every line (previous) | 1.23 µs | 1.55 µs | 5.23 µs |
| buffered, per-batch flush (current) | 77 ns | 389.3 ns | 8.54 µs |

The current policy moves the cost off the median record and onto the batch boundary, which is why its P99 is the higher of the two.

---

## Verified Device Performance

Profiled on physical target hardware (Android 14 API 34, Linux kernel 5.10, aarch64):

### Empirical Microbenchmark Suite: Kernel-Direct Procfs Latency Distribution

Evaluated via the standalone, criterion-free `benches/microbench.rs` harness across 1,000 warm-up cycles and 10,000 timed iterations:

| Target / Operation | Mechanism / Scope | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean | Flagged Preemption Samples |
|---|---|---|---|---|---|---|---|---|---|
| `procfs::read_oom_score_adj` | self, hot cache | 10,000 | 3.15 µs | **3.69 µs** | 5.23 µs | 8.00 µs | 1.05 ms | 4.29 µs | 2 |
| `procfs::read_oom_score_adj` | multi-PID pool (cold) | 10,000 | 6.23 µs | **8.62 µs** | 22.85 µs | 30.15 µs | 3.75 ms | 11.67 µs | 4 |
| `procfs::read_statm_rss_kb` | self, hot cache | 10,000 | 4.00 µs | **4.62 µs** | 5.46 µs | 7.38 µs | 712.31 µs | 5.05 µs | 4 |
| `procfs::read_statm_rss_kb` | multi-PID pool (cold) | 10,000 | 6.46 µs | **8.31 µs** | 10.77 µs | 12.85 µs | 851.08 µs | 9.23 µs | 4 |
| `procfs::read_meminfo_kb` | `/proc/meminfo` | 10,000 | 8.31 µs | **9.23 µs** | 10.54 µs | 23.31 µs | 610.23 µs | 9.90 µs | 2 |
| `telemetry::FormattedTime` | Stack buffer timestamp | 10,000 | 76.0 ns | **308.0 ns** | 308.0 ns | 308.0 ns | 3.46 µs | 281.5 ns | 0 |
| `telemetry::write_to_log` | NDJSON append, flushed every line (previous policy) | 10,000 | 1.00 µs | **1.23 µs** | 1.46 µs | 5.23 µs | 541.85 µs | 1.55 µs | 0 |
| `telemetry::write_to_log` | NDJSON append, no per-line flush (current) | 10,000 | 0.0 ns | **77.0 ns** | 231.0 ns | 8.54 µs | 406.23 µs | 389.3 ns | 0 |
| `Instant::now` (Overhead) | Harness baseline (not on daemon hot path) | 10,000 | 0.0 ns | **231.0 ns** | 308.0 ns | 308.0 ns | 1.00 µs | 214.9 ns | 0 |

* **Cold Multi-PID Pool Access:** Reading across a dynamic pool of external system/user PIDs incurs ~8.6 µs P50 latency (vs 3.7 µs for self), comfortably qualifying multi-PID candidate batches in microseconds.
* **Tail Latency Preemption Diagnostics:** Outliers are flagged when wall time exceeds 500 µs **and** either `ru_nivcsw` incremented or on-CPU time stayed below 50 µs — a disjunction, so a flagged sample shows scheduler disturbance, low on-CPU time, or both. See `docs/ARCHITECTURE.md` §6.2 for the attribution and its limits.
### End-to-End Daemon Benchmarks: Idle vs Active App-Switching Pipeline

Evaluated via `scripts/benchmark.sh` across both steady-state idle conditions (`N = 50`) and active live app-switching workloads (`N = 15`, cycling between Settings, Home, and Browser transitions):

| Metric | Workload Mode | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean |
|---|---|---|---|---|---|---|---|---|
| **Cold-start Discovery** | Initial Boot Indexing | 50 | 246.6 ms | **293.8 ms** | 315.1 ms | 316.1 ms | 316.1 ms | 283.7 ms |
| **Steady-State Memory (PSS)** | Idle Boot State | 50 | 1,064 kB | **1,099 kB** | 1,117 kB | 1,120 kB | 1,120 kB | 1,097 kB |
| **Active Pipeline Memory (PSS)**| Live App-Switch Traffic | 15 | 1,069 kB | **1,093 kB** | 1,117 kB | 1,117 kB | 1,117 kB | 1,093 kB |
| **Resident Set Size (RSS)** | Idle Boot State | 50 | 3,764 kB | **3,876 kB** | 4,004 kB | 4,020 kB | 4,020 kB | 3,882 kB |
| **Resident Set Size (RSS)** | Live App-Switch Traffic | 15 | 3,816 kB | **3,884 kB** | 3,948 kB | 3,948 kB | 3,948 kB | 3,887 kB |
| **Inter-Event Idle CPU Wakeups** | Post-Traffic Idle Window | 50 + 15 | 0 | **0** | 0 | 0 | 0 | 0.0 |

* **Live Workload Memory Cost:** Populating `alive_apps`, `fg_lru`, `pkg_to_pids`, and `recent_deaths` tables during active app-switching is not measurable at this resolution: PSS P50 is 1,093 kB under app-switch traffic versus 1,099 kB idle, and RSS moves 3,876 kB → 3,884 kB, staying under the < 4 MB RSS target.
* **Idle Wakeup Verification:** The single-threaded `epoll_wait` reactor (infinite timeout, no armed timers) produces strictly **0 CPU wakeups** during idle intervals between event bursts, and the same holds for the batched log flush: a quiet device issues no `write(2)` at all.

### Low-UID System Package Safety (Empirical AMS Verification)

Empirically verified on Android 14 that `cmd activity kill --user 0` terminates cached system packages (`uid < 10000`, e.g. `com.android.keychain` PID 29506 killed and terminated; `com.sprd.validationtools` PID 20352 killed and respawned). The daemon's fail-closed `uid >= 10000` guard prevents terminating critical platform services.

### Reproducing Benchmarks

The benchmark suite is completely automated and reproducible directly on Android hardware (API 24+, rootless `sh` compatible):

```bash
# 1. Cross-compile the release daemon and the standalone microbenchmark harness for aarch64
rustup target add aarch64-linux-android
cargo build --release --target aarch64-linux-android
cargo build --release --target aarch64-linux-android --bench microbench

# 2. Push artifacts and runner script to Android device
adb push target/aarch64-linux-android/release/mini-lmk /data/local/tmp/mini-lmk
adb push $(ls -t target/aarch64-linux-android/release/deps/microbench-* | grep -v '\.d$' | head -n 1) /data/local/tmp/microbench
adb push scripts/benchmark.sh /data/local/tmp/benchmark.sh

# 3. Execute microbenchmark suite (self + multi-PID pool procfs latency; flags: -n, -w, --json, --csv, --markdown)
adb shell "chmod 755 /data/local/tmp/mini-lmk /data/local/tmp/microbench /data/local/tmp/benchmark.sh && /data/local/tmp/microbench -n 10000"

# 4. Execute end-to-end active app-switching workload benchmark
adb shell "/data/local/tmp/benchmark.sh -n 15 -a"
```

---

## License

This project is licensed under the **GNU General Public License v3.0** (`GPL-3.0-only`). See the [`LICENSE`](LICENSE) file for the complete license terms.
