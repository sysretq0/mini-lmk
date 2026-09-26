#!/system/bin/sh
# Verify the reporting half of benchmark.sh under a shell that is not bash.
#
# Why this exists: benchmark.sh's shebang is `#!/system/bin/sh`, which on the device is
# toybox ash, and the README runs it there (`adb shell /data/local/tmp/benchmark.sh`).
# `bash -n` -- the obvious syntax gate -- is useless against this, because the two shells
# disagree about what is legal *at run time*, not at parse time. A real instance:
# `${CENSUS_KIND^}` passed `bash -n`, ran fine in bash, and aborted the whole script on
# the device with "Bad substitution" (exit 2) after every measurement had already been
# taken. That bug reached the census verdict lines in both output branches.
#
# So instead of parsing, this executes: it slices the summary section out of benchmark.sh
# (from the flatness verdicts to the microbench hook), feeds it synthetic statistics, and
# requires that a POSIX shell and bash produce *byte-identical* reports for every output
# format, and that the census verdict actually appears in the output. Synthetic stats keep
# it off the device: no daemon, no root, no adb.
#
# Usage: scripts/check-benchmark-sh.sh [path/to/benchmark.sh]
set -eu

TARGET=${1:-scripts/benchmark.sh}
POSIX_SHELL=${POSIX_SHELL:-dash}
BODY=$(mktemp)
trap 'rm -f "$BODY" "$BODY.vars" "$BODY.out" "$BODY.out".*' EXIT

if [ ! -f "$TARGET" ]; then
    echo "[FAIL] no such script: $TARGET" >&2
    exit 1
fi
if ! command -v "$POSIX_SHELL" >/dev/null 2>&1; then
    echo "[SKIP] need a non-bash POSIX shell ($POSIX_SHELL); set POSIX_SHELL=..." >&2
    exit 0
fi

# The section is bounded by markers. If they ever move, fail loudly rather than checking
# nothing -- an empty slice makes every comparison below trivially pass.
awk '/^# Flatness verdicts/,/^if \[ "\$RUN_MICRO" -eq 1 \]/' "$TARGET" | sed '$d' > "$BODY"
if [ ! -s "$BODY" ]; then
    echo "[FAIL] could not slice the summary section out of $TARGET;" >&2
    echo "       the '# Flatness verdicts' / 'RUN_MICRO' markers moved." >&2
    exit 1
fi
LINES=$(awk 'END { print NR }' "$BODY")
if [ "$LINES" -lt 20 ]; then
    echo "[FAIL] sliced only $LINES lines from $TARGET: too small to be the report" >&2
    exit 1
fi

# Synthetic samples. Values are in the shape calc_stats prints them (%.2f), because the
# whole point is to exercise the arithmetic and formatting the real run feeds in.
cat > "$BODY.vars" << 'EOF'
DAEMON_MODE=observe
FD_MIN=8.00; FD_MAX=8.00; FD_P50=8.00; FD_P95=8.00; FD_P99=8.00; FD_MEAN=8.00
TH_MIN=1.00; TH_MAX=1.00; TH_P50=1.00; TH_P95=1.00; TH_P99=1.00; TH_MEAN=1.00
DISC_MIN=12.0; DISC_P50=14.5; DISC_P95=19.9; DISC_P99=21.0; DISC_MAX=22.0; DISC_MEAN=15.2
PSS_MIN=900.00; PSS_P50=1099.00; PSS_P95=1300.00; PSS_P99=1310.00; PSS_MAX=1320.00; PSS_MEAN=1100.5
RSS_MIN=3876.00; RSS_P50=3950.00; RSS_P95=4020.00; RSS_P99=4021.00; RSS_MAX=4022.00; RSS_MEAN=3951.25
WAKE_MIN=0.00; WAKE_P50=1.00; WAKE_P95=3.00; WAKE_P99=4.00; WAKE_MAX=5.00; WAKE_MEAN=1.25
ITERS=20; ACTIVE_WORKLOAD=0; RUN_MICRO=0
CSV_OUT=0; JSON_OUT=0; MD_ONLY=0
EOF

FAILS=0
# Both verdict arms matter: a drift in the descriptor or thread census is the failure this
# script's own output is supposed to announce, and it is built by a conditional.
for drift in stable drifting; do
    for fmt in plain markdown csv json; do
        for shx in "$POSIX_SHELL" bash; do
            {
                cat "$BODY.vars"
                case "$fmt" in
                    markdown) echo 'MD_ONLY=1' ;;
                    csv) echo 'CSV_OUT=1' ;;
                    json) echo 'JSON_OUT=1' ;;
                esac
                if [ "$drift" = drifting ]; then
                    echo 'FD_MIN=5.00; FD_MAX=7.00; FD_P50=6.00; FD_P95=7.00; FD_P99=7.00'
                    echo 'TH_MIN=1.00; TH_MAX=2.00; TH_P50=1.00; TH_P95=2.00; TH_P99=2.00'
                fi
                cat "$BODY"
            } > "$BODY.out"
            if ! "$shx" "$BODY.out" > "$BODY.out.$drift.$fmt.$(basename "$shx")" 2>&1; then
                echo "[FAIL] $shx aborted: drift=$drift format=$fmt" >&2
                FAILS=$((FAILS + 1))
            fi
        done
        ref="$BODY.out.$drift.$fmt.dash"
        alt="$BODY.out.$drift.$fmt.bash"
        if ! cmp -s "$ref" "$alt"; then
            echo "[FAIL] $POSIX_SHELL and bash disagree: drift=$drift format=$fmt" >&2
            diff "$ref" "$alt" | sed 's/^/       /' >&2
            FAILS=$((FAILS + 1))
        fi
    done
done

# A report that prints no verdict is not a report: the census sentence is the whole point
# of the descriptor sampling, and it is the line that used to die on the device.
if ! grep -qi 'census' "$BODY.out.stable.plain.dash"; then
    echo "[FAIL] the POSIX shell's plain report contains no census verdict line" >&2
    FAILS=$((FAILS + 1))
fi
for f in "$BODY.out".*; do
    if grep -q 'Bad substitution' "$f"; then
        echo "[FAIL] 'Bad substitution' in $(basename "$f"): a bashism survived" >&2
        FAILS=$((FAILS + 1))
    fi
done
rm -f "$BODY.out".*

if [ "$FAILS" -ne 0 ]; then
    echo "[FAIL] benchmark.sh is not POSIX-shell-clean: $FAILS check(s) failed" >&2
    exit 1
fi
echo "[OK] benchmark.sh reports identically under $POSIX_SHELL and bash (8 format/drift pairs)"
