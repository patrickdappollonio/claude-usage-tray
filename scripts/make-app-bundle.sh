#!/bin/sh
#
# Assembles "Claude Usage Tray.app" around an already-built binary.
#
#   scripts/make-app-bundle.sh <binary> <version> [output-dir]
#
# The bundle is what buys modern Notification Center notifications:
# UNUserNotificationCenter refuses to talk to a process that has no bundle
# identity at all, so a bare binary can only fall back to `osascript` (which
# current macOS shows nothing for). Inside a bundle the tray gets a real name,
# a real icon slot, a permission prompt on first use, and a proper entry in
# System Settings > Notifications.
#
# The bundle is signed ad hoc (`codesign -s -`), not with an Apple certificate.
# Ad-hoc signing is enough to give the bundle the stable code identity the
# notification framework wants; it is not enough for Gatekeeper, so a directly
# downloaded copy still needs its quarantine attribute cleared (see the README).
#
# Runs on macOS only: `codesign` and (for the zip step in CI) `ditto` are macOS
# tools. POSIX sh otherwise, so `sh -n` on Linux can check it in CI.
#
# The app icon (Contents/Resources/AppIcon.icns) is built from
# assets/appicon-1024.png (the gauge at 30%, white on a dark rounded square,
# rendered by the app's own icon code) via `sips` + `iconutil`. If the master
# PNG is missing the bundle still assembles, just with the generic icon.

set -eu

BUNDLE_ID="com.patrickdappollonio.claude-usage-tray"
BUNDLE_NAME="Claude Usage Tray"
EXECUTABLE="claude-usage-tray"
# 12.0, and specifically because of notifications: the bundled path sets a
# notification's interruption level, and `UNNotificationContent`'s
# `setInterruptionLevel:` only exists from macOS 12. Sending that selector to a
# macOS 11 object would raise rather than degrade, so the bundle declares the
# floor instead of finding out at runtime. The bare binary has no such limit,
# because it never reaches that code (see `platform/macos/mod.rs::notify`).
MIN_MACOS="12.0"

usage() {
	echo "usage: $0 <binary> <version> [output-dir]" >&2
	exit 2
}

[ "$#" -ge 2 ] || usage

BINARY="$1"
VERSION="$2"
OUT_DIR="${3:-dist}"

[ -f "$BINARY" ] || {
	echo "$0: no such binary: $BINARY" >&2
	exit 1
}

APP="$OUT_DIR/$BUNDLE_NAME.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>$EXECUTABLE</string>
	<key>CFBundleIdentifier</key>
	<string>$BUNDLE_ID</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>$BUNDLE_NAME</string>
	<key>CFBundleDisplayName</key>
	<string>$BUNDLE_NAME</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>LSMinimumSystemVersion</key>
	<string>$MIN_MACOS</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSSupportsAutomaticTermination</key>
	<false/>
	<key>NSSupportsSuddenTermination</key>
	<false/>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
</dict>
</plist>
PLIST

# App icon: resize the committed master into an iconset and compile it. Both
# tools ship with macOS. Skipped quietly when the master is not present.
ICON_MASTER="$(dirname "$0")/../assets/appicon-1024.png"
if [ -f "$ICON_MASTER" ]; then
	ICONSET="$OUT_DIR/AppIcon.iconset"
	rm -rf "$ICONSET"
	mkdir -p "$ICONSET"
	for size in 16 32 128 256 512; do
		sips -z "$size" "$size" "$ICON_MASTER" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
		double=$((size * 2))
		sips -z "$double" "$double" "$ICON_MASTER" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
	done
	iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
	rm -rf "$ICONSET"
fi

cp "$BINARY" "$APP/Contents/MacOS/$EXECUTABLE"
chmod 0755 "$APP/Contents/MacOS/$EXECUTABLE"

# Ad-hoc signature. `--deep` so the copied executable is signed along with the
# bundle, `--force` so re-running over an existing bundle replaces the old
# signature instead of failing.
codesign --force --deep -s - "$APP"
codesign --verify --verbose=2 "$APP"

echo "$APP"
