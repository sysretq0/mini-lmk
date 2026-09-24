#!/system/bin/sh
# mini-lmk supervisor wrapper script
MODE="${1:---observe}"
while true; do
    echo "[$(date)] Starting mini-lmk ($MODE)..."
    /data/local/tmp/mlmk/mini-lmk "$MODE"
    EXIT_CODE=$?
    echo "[$(date)] mini-lmk exited with code $EXIT_CODE. Restarting in 1s..."
    sleep 1
done
