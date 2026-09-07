#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release --locked
bundle="dist/Gopher.app"
mkdir -p "$bundle/Contents/MacOS"
cp packaging/Info.plist "$bundle/Contents/Info.plist"
cp target/release/gopher "$bundle/Contents/MacOS/gopher"
# A real Apple signing identity is needed for reliable macOS notification authorization.
# Prefer an installed Apple Development identity for this personal app.
identity="${GOPHER_SIGNING_IDENTITY:-}"
if [ -z "$identity" ]; then
  identity=$(security find-identity -v -p codesigning | sed -n '/"Apple Development:/s/.*) \([A-F0-9]*\) .*/\1/p' | head -n 1)
fi
if [ -z "$identity" ]; then
  identity="-"
  printf '%s\n' 'Warning: no Apple Development signing identity found. The ad-hoc build may be unable to request notifications. Set GOPHER_SIGNING_IDENTITY to an Apple signing identity.' >&2
fi
codesign --force --sign "$identity" "$bundle"
codesign --verify --strict "$bundle"
printf '%s\n' "Built $bundle. Move it to ~/Applications, open it, then enable Launch at login in its menu."
