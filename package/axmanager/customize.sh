# Fallback function definitions if not provided by installer environment
command -v ui_print >/dev/null 2>&1 || ui_print() { echo "$@"; }
command -v abort >/dev/null 2>&1 || abort() { echo "$@" >&2; exit 1; }
command -v set_perm >/dev/null 2>&1 || set_perm() {
    chown "$2:$3" "$1" 2>/dev/null || true
    chmod "$4" "$1" 2>/dev/null || true
}
command -v set_perm_recursive >/dev/null 2>&1 || set_perm_recursive() {
    find "$1" -type d -exec chmod "$4" {} + 2>/dev/null || true
    find "$1" -type f -exec chmod "$5" {} + 2>/dev/null || true
    chown -R "$2:$3" "$1" 2>/dev/null || true
}

# Fallback for MODPATH if executed standalone
[ -z "$MODPATH" ] && MODPATH="${MODDIR:-$PWD}"

# Fallback for ARCH if not provided by installer environment
[ -z "$ARCH" ] && ARCH="$(getprop ro.product.cpu.abi 2>/dev/null | tr -d '\r')"
[ -z "$ARCH" ] && ARCH="$(uname -m 2>/dev/null)"

case "$ARCH" in
    arm64*|aarch64*) ABI="arm64-v8a" ;;
    arm*|armeabi*)   ABI="armeabi-v7a" ;;
    x86_64*|x64*)    ABI="x86_64" ;;
    x86*|i*86*)      ABI="x86" ;;
    *)               abort "! Unsupported CPU architecture: $ARCH" ;;
esac

ui_print "- Installing mini-lmk ($ARCH -> $ABI)..."

mkdir -p "$MODPATH/system/bin"

if [ -f "$MODPATH/bin/$ABI/mini-lmk" ]; then
    cp -f "$MODPATH/bin/$ABI/mini-lmk" "$MODPATH/system/bin/mini-lmk"
elif [ -f "$MODPATH/bin/$ARCH/mini-lmk" ]; then
    cp -f "$MODPATH/bin/$ARCH/mini-lmk" "$MODPATH/system/bin/mini-lmk"
else
    abort "! Binary for $ARCH ($ABI) not found in package!"
fi

# Clean up other ABIs to save space, keeping matching ABI as fallback
for abi_dir in "$MODPATH/bin"/*; do
    [ -d "$abi_dir" ] && [ "${abi_dir##*/}" != "$ABI" ] && rm -rf "$abi_dir"
done

# Initialize module configuration directory
mkdir -p "$MODPATH/mlmk/config" "$MODPATH/mlmk/logs"
[ ! -f "$MODPATH/mlmk/config/exclude.list" ] && touch "$MODPATH/mlmk/config/exclude.list"
[ ! -f "$MODPATH/mlmk/config/games.list" ] && touch "$MODPATH/mlmk/config/games.list"

# Set permissions
set_perm_recursive "$MODPATH/system/bin" 0 0 0755 0755
set_perm_recursive "$MODPATH/mlmk" 0 0 0755 0644
chmod 777 "$MODPATH/mlmk/logs" 2>/dev/null || chmod 755 "$MODPATH/mlmk/logs"
[ -f "$MODPATH/run-daemon.sh" ] && chmod 755 "$MODPATH/run-daemon.sh"
[ -f "$MODPATH/service.sh" ] && chmod 755 "$MODPATH/service.sh"
[ -f "$MODPATH/bin/$ABI/mini-lmk" ] && chmod 755 "$MODPATH/bin/$ABI/mini-lmk"

ui_print "- mini-lmk installed successfully."
