#!/system/bin/sh
# Copyright (C) 2026 sysretq0
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.
#
# SPDX-License-Identifier: GPL-3.0-only
#
# On-device end-to-end benchmark suite for mini-lmk.
# Evaluates cold-start discovery latency, memory footprint (PSS/RSS),
# active app-switch pipeline overhead, and event-loop idle CPU wakeups.

set -e

# Default settings
DEFAULT_ITERS=50
DEFAULT_BIN="/data/local/tmp/mini-lmk"
DEFAULT_MICROBENCH="/data/local/tmp/microbench"
TMP_DIR="/data/local/tmp"

ITERS=$DEFAULT_ITERS
DAEMON_BIN=""
RUN_MICRO=0
ACTIVE_WORKLOAD=0
# Which daemon mode the census measures. The default keeps the harness non-destructive:
# --observe never reaches spawn_kill(), so it proves the startup descriptor and the thread
# count and nothing about a dispatch. DAEMON_MODE=act is the real dispatch census, and it
# kills the apps under test, so it is opt-in on a device where that is acceptable.
DAEMON_MODE="${DAEMON_MODE:-observe}"
CSV_OUT=0
JSON_OUT=0
MD_ONLY=0
IDLE_SLEEP=0.2

print_help() {
    cat << 'EOF'
mini-lmk On-Device Benchmark Suite
Usage: benchmark.sh [OPTIONS]

Options:
  -n, --iterations <N>   Number of test iterations (default: 50)
  -b, --bin <PATH>       Path to mini-lmk binary (default: /data/local/tmp/mini-lmk)
  -a, --active           Drive active app-switching workload during sampling
  -D, --daemon-mode <M>  Daemon mode to sample: observe (default) or act.
                         Only act reaches spawn_kill(), so only act tests a dispatch leak.
  -m, --micro            Also run kernel-direct microbenchmark harness if present
  --csv                  Emit output as structured CSV
  --json                 Emit output as JSON
  --markdown             Emit Markdown summary tables only
  -h, --help             Show this help message
EOF
}

# Parse CLI arguments
while [ $# -gt 0 ]; do
    case "$1" in
        -n|--iterations)
            ITERS="$2"
            shift 2
            ;;
        -b|--bin)
            DAEMON_BIN="$2"
            shift 2
            ;;
        -a|--active)
            ACTIVE_WORKLOAD=1
            shift
            ;;
        -D|--daemon-mode)
            DAEMON_MODE="$2"
            shift 2
            ;;
        -m|--micro)
            RUN_MICRO=1
            shift
            ;;
        --csv)
            CSV_OUT=1
            shift
            ;;
        --json)
            JSON_OUT=1
            shift
            ;;
        --markdown)
            MD_ONLY=1
            shift
            ;;
        -h|--help)
            print_help
            exit 0
            ;;
        *)
            if [ -z "$DAEMON_BIN" ] && [ -f "$1" ]; then
                DAEMON_BIN="$1"
            else
                case "$1" in
                    ''|*[!0-9]*)
                        echo "[WARN] Unknown argument: $1" >&2
                        ;;
                    *)
                        ITERS="$1"
                        ;;
                esac
            fi
            shift
            ;;
    esac
done

# Reject a mode that would silently measure something other than what it names.
case "$DAEMON_MODE" in
    observe|act) ;;
    *)
        echo "[ERROR] --daemon-mode must be observe or act, got: $DAEMON_MODE" >&2
        exit 2
        ;;
esac

# Resolve binary
if [ -z "$DAEMON_BIN" ]; then
    if [ -x "$DEFAULT_BIN" ]; then
        DAEMON_BIN="$DEFAULT_BIN"
    elif [ -x "./mini-lmk" ]; then
        DAEMON_BIN="./mini-lmk"
    else
        DAEMON_BIN=$(command -v mini-lmk 2>/dev/null || true)
    fi
fi

if [ -z "$DAEMON_BIN" ] || [ ! -x "$DAEMON_BIN" ]; then
    echo "[ERROR] mini-lmk binary not found or not executable: ${DAEMON_BIN:-<none>}" >&2
    echo "Please specify binary location with -b /path/to/mini-lmk" >&2
    exit 1
fi

LOG_FILE="${TMP_DIR}/mlmk_bench_$$.log"
DATA_DIR="${TMP_DIR}/mlmk_bench_data_$$"
mkdir -p "$DATA_DIR"

cleanup() {
    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -9 "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -f "$LOG_FILE"
    rm -rf "$DATA_DIR"
}
trap cleanup EXIT INT TERM

FILE_DISCOVERY="${DATA_DIR}/discovery.txt"
FILE_PSS="${DATA_DIR}/pss.txt"
FILE_RSS="${DATA_DIR}/rss.txt"
FILE_WAKEUPS="${DATA_DIR}/wakeups.txt"
# v1.5.0 regression watch (docs/ROADMAP.md 8). The kill dispatcher adds exactly one
# descriptor for the whole process lifetime -- the /dev/null it dup2's into the child. It is
# opened by DaemonState::new(), so it is present in --observe too, which is the mode this
# harness runs the daemon in (a benchmark that killed the apps it measures would not be
# repeatable). What this census therefore covers is the startup descriptor accounting and
# steady-state drift; what it does NOT cover is "a dispatch leaks nothing", because observe
# mode never reaches spawn_kill(). That half is tested by
# src/spawn.rs::the_cached_descriptor_is_ours_and_survives_every_dispatch (20 real dispatches,
# identity asserted). An on-device dispatch census needs --daemon-mode act AND candidates that
# reach the dispatch loop (memory pressure, or a screen-off harvest past t_idle); it does NOT
# need root, because a daemon reads its own /proc/<pid>/fd, because the daemon is designed to run
# rootless as UID 2000 (docs/ARCHITECTURE.md 1.1), and because the AMS gate is a config threshold
# on oom_score_adj, not a UID check (roadmap 7.5).
# The property asserted here is *flatness*, not the count: if min != max across the run,
# something is leaking or the daemon grew a thread, and both invalidate every latency number
# in this file.
FILE_FDS="${DATA_DIR}/fds.txt"
FILE_THREADS="${DATA_DIR}/threads.txt"

if [ "$CSV_OUT" -eq 0 ] && [ "$JSON_OUT" -eq 0 ] && [ "$MD_ONLY" -eq 0 ]; then
    echo "============================================================"
    echo " mini-lmk On-Device End-to-End Benchmark Harness"
    echo " Binary:     $DAEMON_BIN"
    echo " Workload:   $( [ "$ACTIVE_WORKLOAD" -eq 1 ] && echo "Active app-switching transitions" || echo "Steady-state idle" )"
    echo " Daemon mode: --${DAEMON_MODE}  (only --act reaches the dispatcher)"
    echo " Iterations: $ITERS runs"
    echo " Device:     $(getprop ro.product.model 2>/dev/null || uname -m) (Android $(getprop ro.build.version.release 2>/dev/null || echo '?'))"
    echo " Kernel:     $(uname -r)"
    echo "============================================================"
fi

# Run benchmark loop
i=1
while [ "$i" -le "$ITERS" ]; do
    rm -f "$LOG_FILE"

    T_START=$(date +%s%N)
    "$DAEMON_BIN" "--$DAEMON_MODE" > "$LOG_FILE" 2>&1 &
    DAEMON_PID=$!

    # Wait for cold-start discovery completion.
    # These two banners are the harness's only synchronisation points, and this run has no
    # terminal (stdout is a file), so they must stay gated on `--json` alone and never on
    # isatty(1) the way the event table is — otherwise every run times out right here.
    WAIT_COUNT=0
    while ! grep -q "Indexed" "$LOG_FILE" 2>/dev/null; do
        sleep 0.005
        WAIT_COUNT=$((WAIT_COUNT + 1))
        if [ "$WAIT_COUNT" -gt 600 ]; then
            echo "[ERROR] Timeout waiting for cold-start discovery on run $i" >&2
            kill -9 "$DAEMON_PID" 2>/dev/null || true
            exit 1
        fi
    done
    T_INDEX=$(date +%s%N)

    # Wait for event loop to become active
    WAIT_COUNT=0
    while ! grep -q "Monitoring FDs" "$LOG_FILE" 2>/dev/null; do
        sleep 0.005
        WAIT_COUNT=$((WAIT_COUNT + 1))
        if [ "$WAIT_COUNT" -gt 400 ]; then
            break
        fi
    done

    # Measure wall-clock cold-start discovery duration (ms)
    DISCOVERY_MS=$(awk "BEGIN { printf \"%.2f\", ($T_INDEX - $T_START) / 1000000 }")
    echo "$DISCOVERY_MS" >> "$FILE_DISCOVERY"

    # If active workload requested, drive real foreground transitions
    if [ "$ACTIVE_WORKLOAD" -eq 1 ]; then
        am start -n com.android.settings/.Settings > /dev/null 2>&1 || true
        sleep 0.4
        input keyevent KEYCODE_HOME > /dev/null 2>&1 || true
        sleep 0.4
        am start -a android.intent.action.VIEW -d "https://www.google.com" > /dev/null 2>&1 || true
        sleep 0.4
        input keyevent KEYCODE_HOME > /dev/null 2>&1 || true
        sleep 0.4
    fi

    # Sample memory footprint (after any active app switches)
    PSS_KB=0
    RSS_KB=0
    if [ -f "/proc/$DAEMON_PID/smaps_rollup" ]; then
        PSS_KB=$(grep -m1 '^Pss:' "/proc/$DAEMON_PID/smaps_rollup" 2>/dev/null | awk '{print $2}')
        RSS_KB=$(grep -m1 '^Rss:' "/proc/$DAEMON_PID/smaps_rollup" 2>/dev/null | awk '{print $2}')
    fi
    if [ -z "$PSS_KB" ] || [ "$PSS_KB" -eq 0 ] 2>/dev/null; then
        RSS_KB=$(grep -m1 '^VmRSS:' "/proc/$DAEMON_PID/status" 2>/dev/null | awk '{print $2}')
        PSS_KB=$RSS_KB
    fi
    echo "${PSS_KB:-0}" >> "$FILE_PSS"
    echo "${RSS_KB:-0}" >> "$FILE_RSS"

    # Sample CPU context switches for idle wakeup verification (inter-event window)
    CS1=$(grep 'ctxt_switches' "/proc/$DAEMON_PID/status" 2>/dev/null | awk '{s += $2} END {print s}')
    sleep "$IDLE_SLEEP"
    CS2=$(grep 'ctxt_switches' "/proc/$DAEMON_PID/status" 2>/dev/null | awk '{s += $2} END {print s}')
    WAKEUPS=$(( (CS2 - CS1) ))
    if [ "$WAKEUPS" -lt 0 ]; then
        WAKEUPS=0
    fi
    echo "$WAKEUPS" >> "$FILE_WAKEUPS"

    # Descriptor and thread census (see the FILE_FDS comment: drift is the failure, and what
    # an --observe daemon can and cannot prove about it)
    FD_COUNT=0
    if [ -d "/proc/$DAEMON_PID/fd" ]; then
        FD_COUNT=$(ls -1 "/proc/$DAEMON_PID/fd" 2>/dev/null | wc -l | tr -d ' ')
    fi
    THREADS=$(awk '/^Threads:/ { print $2; exit }' "/proc/$DAEMON_PID/status" 2>/dev/null)
    echo "${FD_COUNT:-0}" >> "$FILE_FDS"
    echo "${THREADS:-0}" >> "$FILE_THREADS"

    # Terminate daemon cleanly
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""

    if [ "$CSV_OUT" -eq 0 ] && [ "$JSON_OUT" -eq 0 ] && [ "$MD_ONLY" -eq 0 ]; then
        printf "  [%3d/%3d] Discovery: %6.1f ms | PSS: %5d kB | RSS: %5d kB | Idle Wakeups: %d | FDs: %2d | Threads: %d\n" \
            "$i" "$ITERS" "$DISCOVERY_MS" "${PSS_KB:-0}" "${RSS_KB:-0}" "$WAKEUPS" "${FD_COUNT:-0}" "${THREADS:-0}"
    fi

    i=$((i + 1))
done

# Function to compute statistics from sorted numbers
calc_stats() {
    input_file="$1"
    sort -n "$input_file" | awk '
    BEGIN { n = 0; sum = 0; }
    {
        a[n] = $1;
        sum += $1;
        n++;
    }
    END {
        if (n == 0) {
            print "0 0 0 0 0 0";
            exit;
        }
        min = a[0];
        max = a[n - 1];
        mean = sum / n;
        p50 = a[int(n * 0.50)];
        p95 = a[int(n * 0.95)];
        p99 = a[int(n * 0.99)];
        printf("%.2f %.2f %.2f %.2f %.2f %.2f\n", min, p50, p95, p99, max, mean);
    }'
}

set -- $(calc_stats "$FILE_DISCOVERY")
DISC_MIN="${1:-0}"; DISC_P50="${2:-0}"; DISC_P95="${3:-0}"; DISC_P99="${4:-0}"; DISC_MAX="${5:-0}"; DISC_MEAN="${6:-0}"

set -- $(calc_stats "$FILE_PSS")
PSS_MIN="${1:-0}"; PSS_P50="${2:-0}"; PSS_P95="${3:-0}"; PSS_P99="${4:-0}"; PSS_MAX="${5:-0}"; PSS_MEAN="${6:-0}"

set -- $(calc_stats "$FILE_RSS")
RSS_MIN="${1:-0}"; RSS_P50="${2:-0}"; RSS_P95="${3:-0}"; RSS_P99="${4:-0}"; RSS_MAX="${5:-0}"; RSS_MEAN="${6:-0}"

set -- $(calc_stats "$FILE_WAKEUPS")
WAKE_MIN="${1:-0}"; WAKE_P50="${2:-0}"; WAKE_P95="${3:-0}"; WAKE_P99="${4:-0}"; WAKE_MAX="${5:-0}"; WAKE_MEAN="${6:-0}"

set -- $(calc_stats "$FILE_FDS")
FD_MIN="${1:-0}"; FD_P50="${2:-0}"; FD_P95="${3:-0}"; FD_P99="${4:-0}"; FD_MAX="${5:-0}"; FD_MEAN="${6:-0}"

set -- $(calc_stats "$FILE_THREADS")
TH_MIN="${1:-0}"; TH_P50="${2:-0}"; TH_P95="${3:-0}"; TH_P99="${4:-0}"; TH_MAX="${5:-0}"; TH_MEAN="${6:-0}"

# Flatness verdicts, as integers so the comparison cannot be a string accident.
if [ "$DAEMON_MODE" = "act" ]; then
    CENSUS_KIND="dispatch"
    CENSUS_SCOPE="the dispatch path (act mode dispatched real kills)"
else
    CENSUS_KIND="steady-state"
    CENSUS_SCOPE="the startup descriptor and the thread count only -- --observe never calls spawn_kill(); run --daemon-mode act to test a dispatch leak"
fi
FD_DRIFT=$(( ${FD_MAX%.*} - ${FD_MIN%.*} ))
TH_DRIFT=$(( ${TH_MAX%.*} - ${TH_MIN%.*} ))

WORKLOAD_LABEL="Steady-state Idle"
if [ "$ACTIVE_WORKLOAD" -eq 1 ]; then
    WORKLOAD_LABEL="Active App-Switch Pipeline"
fi

# Census labels in the shapes the two summaries want. Capitalising through awk rather
# than ${var^} is the same ash constraint as above; tr for the whole word is POSIX.
CENSUS_KIND_CAP=$(printf '%s' "$CENSUS_KIND" | awk '{ print toupper(substr($0, 1, 1)) substr($0, 2) }')
CENSUS_KIND_UP=$(printf '%s' "$CENSUS_KIND" | tr 'a-z' 'A-Z')
if [ "$FD_DRIFT" -eq 0 ] && [ "$TH_DRIFT" -eq 0 ]; then
    MD_VERDICT="**${CENSUS_KIND_CAP} census verdict (${CENSUS_KIND} mode, ${ITERS} samples):** stable -- one extra descriptor, one thread, no drift. Scope: ${CENSUS_SCOPE} (docs/ROADMAP.md 8)."
    PLAIN_VERDICT=$(printf '%s CENSUS (%s mode): stable at %.0f descriptors / %.0f threads over %s samples -- the one extra fd and the single-thread promise both hold. Scope: %s\n' "$CENSUS_KIND_UP" "$DAEMON_MODE" "$FD_P50" "$TH_P50" "$ITERS" "$CENSUS_SCOPE")
else
    MD_VERDICT="**${CENSUS_KIND_CAP} census verdict (${CENSUS_KIND} mode, ${ITERS} samples):** **DRIFT** -- fds ${FD_MIN}..${FD_MAX}, threads ${TH_MIN}..${TH_MAX}; the dispatcher promises exactly one extra descriptor and one thread. Scope: ${CENSUS_SCOPE}"
    PLAIN_VERDICT=$(printf '%s CENSUS (%s mode): DRIFT -- fds %.0f..%.0f, threads %.0f..%.0f. The dispatcher promises exactly one extra descriptor and one thread; something moved, so treat every latency number above as suspect. Scope: %s\n' "$CENSUS_KIND_UP" "$DAEMON_MODE" "$FD_MIN" "$FD_MAX" "$TH_MIN" "$TH_MAX" "$CENSUS_SCOPE")
fi

# The markdown table is printed both on its own (-f markdown) and at the foot of the plain
# summary. One function, so the two can never drift apart.
print_md_table() {
    cat << EOF
| Metric | Workload | Iterations | Min | P50 (Median) | P95 | P99 | Max | Mean |
|---|---|---|---|---|---|---|---|---|
| Cold-start Discovery | $WORKLOAD_LABEL | $ITERS | ${DISC_MIN} ms | ${DISC_P50} ms | ${DISC_P95} ms | ${DISC_P99} ms | ${DISC_MAX} ms | ${DISC_MEAN} ms |
| Memory Footprint (PSS) | $WORKLOAD_LABEL | $ITERS | ${PSS_MIN} kB | ${PSS_P50} kB | ${PSS_P95} kB | ${PSS_P99} kB | ${PSS_MAX} kB | ${PSS_MEAN} kB |
| Memory Footprint (RSS) | $WORKLOAD_LABEL | $ITERS | ${RSS_MIN} kB | ${RSS_P50} kB | ${RSS_P95} kB | ${RSS_P99} kB | ${RSS_MAX} kB | ${RSS_MEAN} kB |
| Inter-Event Idle Wakeups | $WORKLOAD_LABEL | $ITERS | ${WAKE_MIN} | ${WAKE_P50} | ${WAKE_P95} | ${WAKE_P99} | ${WAKE_MAX} | ${WAKE_MEAN} |
| Daemon FD Census | $WORKLOAD_LABEL | $ITERS | ${FD_MIN} | ${FD_P50} | ${FD_P95} | ${FD_P99} | ${FD_MAX} | ${FD_MEAN} |
| Daemon Thread Census | $WORKLOAD_LABEL | $ITERS | ${TH_MIN} | ${TH_P50} | ${TH_P95} | ${TH_P99} | ${TH_MAX} | ${TH_MEAN} |

$MD_VERDICT
EOF
}

# Output formats
if [ "$CSV_OUT" -eq 1 ]; then
    echo "workload,metric,iterations,unit,min,p50,p95,p99,max,mean"
    echo "$WORKLOAD_LABEL,cold_start_discovery,$ITERS,ms,$DISC_MIN,$DISC_P50,$DISC_P95,$DISC_P99,$DISC_MAX,$DISC_MEAN"
    echo "$WORKLOAD_LABEL,memory_pss,$ITERS,kB,$PSS_MIN,$PSS_P50,$PSS_P95,$PSS_P99,$PSS_MAX,$PSS_MEAN"
    echo "$WORKLOAD_LABEL,memory_rss,$ITERS,kB,$RSS_MIN,$RSS_P50,$RSS_P95,$RSS_P99,$RSS_MAX,$RSS_MEAN"
    echo "$WORKLOAD_LABEL,idle_cpu_wakeups,$ITERS,count,$WAKE_MIN,$WAKE_P50,$WAKE_P95,$WAKE_P99,$WAKE_MAX,$WAKE_MEAN"
    echo "$WORKLOAD_LABEL,daemon_fd_count,$ITERS,count,$FD_MIN,$FD_P50,$FD_P95,$FD_P99,$FD_MAX,$FD_MEAN"
    echo "$WORKLOAD_LABEL,daemon_thread_count,$ITERS,count,$TH_MIN,$TH_P50,$TH_P95,$TH_P99,$TH_MAX,$TH_MEAN"
elif [ "$JSON_OUT" -eq 1 ]; then
    cat << EOF
[
  {"workload":"$WORKLOAD_LABEL","metric":"cold_start_discovery","iterations":$ITERS,"unit":"ms","min":$DISC_MIN,"p50":$DISC_P50,"p95":$DISC_P95,"p99":$DISC_P99,"max":$DISC_MAX,"mean":$DISC_MEAN},
  {"workload":"$WORKLOAD_LABEL","metric":"memory_pss","iterations":$ITERS,"unit":"kB","min":$PSS_MIN,"p50":$PSS_P50,"p95":$PSS_P95,"p99":$PSS_P99,"max":$PSS_MAX,"mean":$PSS_MEAN},
  {"workload":"$WORKLOAD_LABEL","metric":"memory_rss","iterations":$ITERS,"unit":"kB","min":$RSS_MIN,"p50":$RSS_P50,"p95":$RSS_P95,"p99":$RSS_P99,"max":$RSS_MAX,"mean":$RSS_MEAN},
  {"workload":"$WORKLOAD_LABEL","metric":"idle_cpu_wakeups","iterations":$ITERS,"unit":"count","min":$WAKE_MIN,"p50":$WAKE_P50,"p95":$WAKE_P95,"p99":$WAKE_P99,"max":$WAKE_MAX,"mean":$WAKE_MEAN},
  {"workload":"$WORKLOAD_LABEL","metric":"daemon_fd_count","iterations":$ITERS,"unit":"count","min":$FD_MIN,"p50":$FD_P50,"p95":$FD_P95,"p99":$FD_P99,"max":$FD_MAX,"mean":$FD_MEAN,"drift":$FD_DRIFT},
  {"workload":"$WORKLOAD_LABEL","metric":"daemon_thread_count","iterations":$ITERS,"unit":"count","min":$TH_MIN,"p50":$TH_P50,"p95":$TH_P95,"p99":$TH_P99,"max":$TH_MAX,"mean":$TH_MEAN,"drift":$TH_DRIFT}
]
EOF
elif [ "$MD_ONLY" -eq 1 ]; then
    echo "### End-to-End Daemon Empirical Benchmark ($WORKLOAD_LABEL, $ITERS sample runs)"
    print_md_table
else
    cat << EOF

====================================================================================================
 mini-lmk Empirical Benchmark Summary ($WORKLOAD_LABEL, $ITERS runs)
====================================================================================================
Metric                         | Iterations |      Min |   Median |      P95 |      P99 |      Max |     Mean
-------------------------------+------------+----------+----------+----------+----------+----------+---------
Cold-start Discovery (ms)      | $(printf "%10d" "$ITERS") | $(printf "%8.1f" "$DISC_MIN") | $(printf "%8.1f" "$DISC_P50") | $(printf "%8.1f" "$DISC_P95") | $(printf "%8.1f" "$DISC_P99") | $(printf "%8.1f" "$DISC_MAX") | $(printf "%8.1f" "$DISC_MEAN")
Memory Footprint PSS (kB)      | $(printf "%10d" "$ITERS") | $(printf "%8.0f" "$PSS_MIN") | $(printf "%8.0f" "$PSS_P50") | $(printf "%8.0f" "$PSS_P95") | $(printf "%8.0f" "$PSS_P99") | $(printf "%8.0f" "$PSS_MAX") | $(printf "%8.0f" "$PSS_MEAN")
Memory Footprint RSS (kB)      | $(printf "%10d" "$ITERS") | $(printf "%8.0f" "$RSS_MIN") | $(printf "%8.0f" "$RSS_P50") | $(printf "%8.0f" "$RSS_P95") | $(printf "%8.0f" "$RSS_P99") | $(printf "%8.0f" "$RSS_MAX") | $(printf "%8.0f" "$RSS_MEAN")
Inter-Event Idle CPU Wakeups   | $(printf "%10d" "$ITERS") | $(printf "%8.0f" "$WAKE_MIN") | $(printf "%8.0f" "$WAKE_P50") | $(printf "%8.0f" "$WAKE_P95") | $(printf "%8.0f" "$WAKE_P99") | $(printf "%8.0f" "$WAKE_MAX") | $(printf "%8.1f" "$WAKE_MEAN")
Daemon FD Census (count)       | $(printf "%10d" "$ITERS") | $(printf "%8.0f" "$FD_MIN") | $(printf "%8.0f" "$FD_P50") | $(printf "%8.0f" "$FD_P95") | $(printf "%8.0f" "$FD_P99") | $(printf "%8.0f" "$FD_MAX") | $(printf "%8.1f" "$FD_MEAN")
Daemon Thread Census (count)   | $(printf "%10d" "$ITERS") | $(printf "%8.0f" "$TH_MIN") | $(printf "%8.0f" "$TH_P50") | $(printf "%8.0f" "$TH_P95") | $(printf "%8.0f" "$TH_P99") | $(printf "%8.0f" "$TH_MAX") | $(printf "%8.1f" "$TH_MEAN")
$PLAIN_VERDICT
====================================================================================================

### Markdown Table
EOF
    print_md_table
fi

if [ "$RUN_MICRO" -eq 1 ]; then
    if [ -x "$DEFAULT_MICROBENCH" ]; then
        echo ""
        "$DEFAULT_MICROBENCH" -n 10000
    elif [ -x "./microbench" ]; then
        echo ""
        ./microbench -n 10000
    fi
fi
