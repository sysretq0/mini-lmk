ui_print "- Installing mini-lmk ($ARCH)..."

case "$ARCH" in
    arm64) ABI="arm64-v8a" ;;
    arm)   ABI="armeabi-v7a" ;;
    x64)   ABI="x86_64" ;;
    x86)   ABI="x86" ;;
    *)     abort "! Unsupported CPU architecture: $ARCH" ;;
esac

mkdir -p "$MODPATH/system/bin"

if [ -f "$MODPATH/bin/$ABI/mini-lmk" ]; then
    cp -f "$MODPATH/bin/$ABI/mini-lmk" "$MODPATH/system/bin/mini-lmk"
elif [ -f "$MODPATH/bin/$ARCH/mini-lmk" ]; then
    cp -f "$MODPATH/bin/$ARCH/mini-lmk" "$MODPATH/system/bin/mini-lmk"
else
    abort "! Binary for $ARCH ($ABI) not found in package!"
fi

rm -rf "$MODPATH/bin"

chmod 755 "$MODPATH/system/bin/mini-lmk"
[ -f "$MODPATH/run-daemon.sh" ] && chmod 755 "$MODPATH/run-daemon.sh"
[ -f "$MODPATH/service.sh" ] && chmod 755 "$MODPATH/service.sh"

mkdir -p /data/local/tmp/mlmk/config /data/local/tmp/mlmk/logs
[ ! -f /data/local/tmp/mlmk/config/exclude.list ] && touch /data/local/tmp/mlmk/config/exclude.list
[ ! -f /data/local/tmp/mlmk/config/games.list ] && touch /data/local/tmp/mlmk/config/games.list

ui_print "- mini-lmk installed successfully."
