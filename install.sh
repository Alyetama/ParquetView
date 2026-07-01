#!/usr/bin/env bash
#
# Build ParquetView, ad-hoc sign it, and install it to /Applications.
#
# The Tauri bundler's final step calls `xattr` by name; if a non-Apple `xattr`
# (e.g. the conda shim) is first on PATH, that step errors *after* the .app is
# already built. This script tolerates that, then does the signing/xattr/copy
# with the system tools explicitly.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_NAME="ParquetView.app"
BUNDLE="$ROOT/src-tauri/target/release/bundle/macos/$APP_NAME"
DEST="/Applications/$APP_NAME"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

echo "==> Building release bundle…"
# The bundler may exit non-zero on the cosmetic xattr step; don't abort on it.
( cd "$ROOT" && npm run tauri build ) || echo "   (bundler returned non-zero — continuing; the .app is built before the xattr step)"

if [ ! -d "$BUNDLE" ]; then
  echo "!! Build did not produce $BUNDLE" >&2
  exit 1
fi

echo "==> Stripping extended attributes (system xattr)…"
/usr/bin/xattr -cr "$BUNDLE"

echo "==> Ad-hoc signing…"
/usr/bin/codesign --force --deep --sign - --timestamp=none "$BUNDLE"
/usr/bin/codesign --verify --strict "$BUNDLE" && echo "   signature OK"

echo "==> Installing to /Applications…"
[ -d "$DEST" ] && rm -rf "$DEST"
cp -R "$BUNDLE" "$DEST"
/usr/bin/xattr -cr "$DEST"

echo "==> Registering .parquet association with Launch Services…"
"$LSREGISTER" -f "$DEST"

echo "✅ Installed $DEST"
echo "   First launch: right-click the app in /Applications → Open (Gatekeeper)."
