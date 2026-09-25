#!/system/bin/sh
# Copyright (C) 2026 sysretq0
# SPDX-License-Identifier: GPL-3.0-only

MODDIR="${0%/*}"

if [ -f "$MODDIR/run-daemon.sh" ]; then
    nohup "$MODDIR/run-daemon.sh" "$@" >/dev/null 2>&1 &
elif [ -x "$MODDIR/system/bin/mini-lmk" ]; then
    # Same forwarding rule as run-daemon.sh: every argument reaches the binary, and the
    # historical `--act` default only applies when nothing was passed.
    if [ "$#" -eq 0 ]; then
        set -- --act
    fi
    nohup "$MODDIR/system/bin/mini-lmk" "$@" >/dev/null 2>&1 &
fi
