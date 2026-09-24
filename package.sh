#!/usr/bin/env bash
set -euo pipefail

VERSION=$(grep '^version = ' Cargo.toml | head -n1 | cut -d'"' -f2)
OUT_DIR="dist"
STAGE_DIR="dist/stage"
ZIP_NAME="mini-lmk-axmanager-v${VERSION}.zip"

echo "Packaging mini-lmk AxManager Plugin v${VERSION}..."

rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/bin/arm64-v8a" \
         "$STAGE_DIR/bin/armeabi-v7a" \
         "$STAGE_DIR/bin/x86_64" \
         "$STAGE_DIR/bin/x86" \
         "$STAGE_DIR/system/bin" \
         "$OUT_DIR"

# Verify all binaries exist
for target in aarch64-linux-android:arm64-v8a armv7-linux-androideabi:armeabi-v7a x86_64-linux-android:x86_64 i686-linux-android:x86; do
    rust_target="${target%%:*}"
    abi="${target##*:}"
    bin_path="target/${rust_target}/release/mini-lmk"
    if [ ! -f "$bin_path" ]; then
        echo "[ERROR] Missing binary for ${rust_target}: ${bin_path}" >&2
        echo "Run: cargo build --release --target ${rust_target}" >&2
        exit 1
    fi
    cp "$bin_path" "$STAGE_DIR/bin/${abi}/mini-lmk"
    echo "  [+] Included ${abi} (${rust_target})"
done

# Copy module metadata and scripts
cp package/axmanager/module.prop "$STAGE_DIR/"
cp package/axmanager/customize.sh "$STAGE_DIR/"
cp package/axmanager/service.sh "$STAGE_DIR/"
cp run-daemon.sh "$STAGE_DIR/"
touch "$STAGE_DIR/system/bin/.gitkeep"

# Package zip
rm -f "$OUT_DIR/$ZIP_NAME"
(cd "$STAGE_DIR" && zip -r -9 "../../$OUT_DIR/$ZIP_NAME" ./*)

echo " Successfully packaged: $OUT_DIR/$ZIP_NAME"
ls -lh "$OUT_DIR/$ZIP_NAME"
