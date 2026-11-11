#!/bin/sh
# Builds ConvKit.app from this package. With --bundle-conv, copies a conv
# binary into the app so it doesn't need one on the PATH.
set -eu
cd "$(dirname "$0")"

conv=
if [ "${1:-}" = "--bundle-conv" ]; then
  conv="${CONV_BIN:-${CARGO_TARGET_DIR:-../../target}/release/conv}"
  if [ ! -f "$conv" ] || [ ! -x "$conv" ]; then
    echo "No conv at $conv. Build it first (cargo build --release), or set CONV_BIN." >&2
    exit 1
  fi
fi

swift build -c release
bin="$(swift build -c release --show-bin-path)/ConvKit"

app=build/ConvKit.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/ConvKit"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>ConvKit</string>
  <key>CFBundleDisplayName</key><string>ConvKit</string>
  <key>CFBundleIdentifier</key><string>dev.convkit.app</string>
  <key>CFBundleExecutable</key><string>ConvKit</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key><array><string>public.item</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

if [ -n "$conv" ]; then
  cp "$conv" "$app/Contents/Resources/conv"
fi

codesign --force --sign - "$app"
echo "$PWD/$app"
