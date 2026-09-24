#!/system/bin/sh
# Copyright (C) 2026 sysretq0
# SPDX-License-Identifier: GPL-3.0-only

MODDIR="${0%/*}"
[ -n "$MODDIR" ] && [ -d "$MODDIR" ] && export MODPATH="${MODPATH:-$MODDIR}"

until [ "$(getprop sys.boot_completed 2>/dev/null)" = "1" ] && pidof com.android.systemui >/dev/null 2>&1; do
    sleep 2
done

if command -v mini-lmk >/dev/null 2>&1; then
    BIN="mini-lmk"
elif [ -n "$MODPATH" ] && [ -x "$MODPATH/system/bin/mini-lmk" ]; then
    BIN="$MODPATH/system/bin/mini-lmk"
else
    case "$(getprop ro.product.cpu.abi 2>/dev/null || uname -m)" in
        arm64*|aarch64*) ARCH="arm64-v8a" ;;
        arm*|armeabi*)   ARCH="armeabi-v7a" ;;
        x86_64*|x64*)    ARCH="x86_64" ;;
        x86*|i*86*)      ARCH="x86" ;;
        *)               ARCH="" ;;
    esac
    BIN="$MODPATH/bin/$ARCH/mini-lmk"
fi

[ -f "$BIN" ] && chmod 755 "$BIN" 2>/dev/null

if [ ! -x "$BIN" ] && ! command -v "$BIN" >/dev/null 2>&1; then
    echo "mini-lmk: binary not found ($BIN)" >&2
    exit 1
fi

MODE="${1:---act}"

while true; do
    "$BIN" "$MODE"
    sleep 2
done
