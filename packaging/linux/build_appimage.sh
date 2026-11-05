#!/usr/bin/env bash
# Build COLMAP-Rust-Studio-x86_64.AppImage. COLMAP itself is not bundled.
set -euo pipefail
cd "$(dirname "$0")/../.."
TOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
[ -n "$TOOL" ] || { echo "Set APPIMAGETOOL to an appimagetool binary"; exit 1; }
cargo build --release --features gui
APPDIR=target/AppDir
rm -rf "$APPDIR" && mkdir -p "$APPDIR/usr/bin"
install -m755 target/release/colmap-studio target/release/colmap-launcher "$APPDIR/usr/bin/"
install -m755 packaging/linux/AppRun "$APPDIR/AppRun"
cp packaging/linux/colmap-studio.desktop packaging/linux/colmap-studio.png "$APPDIR/"
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$TOOL" --no-appstream "$APPDIR" COLMAP-Rust-Studio-x86_64.AppImage
