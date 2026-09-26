#!/system/bin/sh
# Copyright (C) 2026 sysretq0
# SPDX-License-Identifier: GPL-3.0-only

MODDIR="${0%/*}"

nohup "$MODDIR/run-daemon.sh" >/dev/null 2>&1 &
