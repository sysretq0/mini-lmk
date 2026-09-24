#!/system/bin/sh
# Copyright (C) 2026 sysretq0
# SPDX-License-Identifier: GPL-3.0-only

MODDIR="${0%/*}"

if [ -f "$MODDIR/run-daemon.sh" ]; then
    nohup "$MODDIR/run-daemon.sh" "$@" >/dev/null 2>&1 &
elif [ -x "$MODDIR/system/bin/mini-lmk" ]; then
    nohup "$MODDIR/system/bin/mini-lmk" "${1:---act}" >/dev/null 2>&1 &
fi
