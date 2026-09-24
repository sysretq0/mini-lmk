#!/system/bin/sh
# Copyright (C) 2026 sysretq0
# SPDX-License-Identifier: GPL-3.0-only
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.

# mini-lmk supervisor wrapper script
MODE="${1:---act}"
SCRIPT_DIR="${0%/*}"

# Dynamically locate mini-lmk binary:
# 1. Prefer command on PATH (e.g., AxManager exports $MODPATH/system/bin into PATH)
# 2. Check $MODPATH/system/bin/mini-lmk
# 3. Check adjacent / relative directories
# 4. Check standalone /data/local/tmp paths
if command -v mini-lmk >/dev/null 2>&1; then
    BIN="$(command -v mini-lmk)"
elif [ -n "$MODPATH" ] && [ -x "$MODPATH/system/bin/mini-lmk" ]; then
    BIN="$MODPATH/system/bin/mini-lmk"
elif [ -x "$SCRIPT_DIR/system/bin/mini-lmk" ]; then
    BIN="$SCRIPT_DIR/system/bin/mini-lmk"
elif [ -x "$SCRIPT_DIR/mini-lmk" ]; then
    BIN="$SCRIPT_DIR/mini-lmk"
elif [ -x "/data/local/tmp/mini-lmk" ]; then
    BIN="/data/local/tmp/mini-lmk"
elif [ -x "/data/local/tmp/mlmk/mini-lmk" ]; then
    BIN="/data/local/tmp/mlmk/mini-lmk"
else
    echo "[ERROR] Cannot find executable mini-lmk binary!" >&2
    exit 1
fi

# Ensure runtime directories exist
mkdir -p /data/local/tmp/mlmk/config /data/local/tmp/mlmk/logs
[ ! -f /data/local/tmp/mlmk/config/exclude.list ] && touch /data/local/tmp/mlmk/config/exclude.list
[ ! -f /data/local/tmp/mlmk/config/games.list ] && touch /data/local/tmp/mlmk/config/games.list

echo "[$(date)] mini-lmk supervisor started. Binary: $BIN ($MODE)"

while true; do
    echo "[$(date)] Starting mini-lmk ($MODE)..."
    "$BIN" "$MODE"
    EXIT_CODE=$?
    echo "[$(date)] mini-lmk exited with code $EXIT_CODE. Restarting in 1s..."
    sleep 1
done
