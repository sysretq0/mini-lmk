#!/system/bin/sh
MODDIR="${0%/*}"

sleep 5
pkill -f "mini-lmk" 2>/dev/null

if [ -x "$MODDIR/run-daemon.sh" ]; then
    nohup "$MODDIR/run-daemon.sh" --act > /data/local/tmp/mlmk/logs/stdout.log 2>&1 &
elif [ -x "$MODDIR/system/bin/mini-lmk" ]; then
    nohup "$MODDIR/system/bin/mini-lmk" --act > /data/local/tmp/mlmk/logs/stdout.log 2>&1 &
fi
