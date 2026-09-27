#!/usr/bin/env bash
#
# Builds "LAN Audio Share.app" — a double-clickable macOS bundle.
#
#   bash scripts/make-macos-app.sh              # native (Apple Silicon or Intel)
#   bash scripts/make-macos-app.sh --universal  # one binary for both
#
# The bundle lands in target/release/ and can be dragged straight into
# /Applications. Nothing but Xcode command line tools and Rust is required.
#
# A bundle is not just cosmetic here. macOS grants Microphone and Local Network
# access per app identity, and a bare binary has none — it inherits whatever the
# terminal that launched it was granted, which is why a loose executable tends to
# fail at capturing audio or finding peers in ways that are hard to diagnose.

set -euo pipefail

APP_NAME="LAN Audio Share"
BUNDLE_ID="${BUNDLE_ID:-com.lanaudioshare.app}"
BIN_NAME="lan-audio-share"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this builds a macOS bundle and must be run on macOS." >&2
  echo "       on Windows just use target/release/${BIN_NAME}.exe" >&2
  exit 1
fi

UNIVERSAL=0
for arg in "$@"; do
  case "$arg" in
    --universal) UNIVERSAL=1 ;;
    -h|--help) sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "error: unknown option '$arg'" >&2; exit 1 ;;
  esac
done

# Version comes from Cargo.toml so the bundle never disagrees with the binary.
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
VERSION="${VERSION:-0.1.0}"

APP="target/release/${APP_NAME}.app"
CONTENTS="${APP}/Contents"
MACOS_DIR="${CONTENTS}/MacOS"
RESOURCES="${CONTENTS}/Resources"

echo "==> Building ${APP_NAME} ${VERSION}"

if [[ "$UNIVERSAL" == "1" ]]; then
  echo "--> Building for both architectures"
  rustup target add x86_64-apple-darwin aarch64-apple-darwin >/dev/null
  cargo build --release --target x86_64-apple-darwin
  cargo build --release --target aarch64-apple-darwin
else
  cargo build --release
fi

echo "--> Assembling bundle"
rm -rf "$APP"
mkdir -p "$MACOS_DIR" "$RESOURCES"

if [[ "$UNIVERSAL" == "1" ]]; then
  lipo -create -output "${MACOS_DIR}/${BIN_NAME}" \
    "target/x86_64-apple-darwin/release/${BIN_NAME}" \
    "target/aarch64-apple-darwin/release/${BIN_NAME}"
else
  cp "target/release/${BIN_NAME}" "${MACOS_DIR}/${BIN_NAME}"
fi
chmod +x "${MACOS_DIR}/${BIN_NAME}"

if [[ -f assets/AppIcon.icns ]]; then
  cp assets/AppIcon.icns "${RESOURCES}/AppIcon.icns"
else
  echo "    warning: assets/AppIcon.icns missing, the app will show a blank icon"
  echo "             (regenerate it with: python3 tools/make_icon.py)"
fi

# The two usage descriptions are load-bearing, not boilerplate. macOS refuses
# audio input outright without NSMicrophoneUsageDescription — and that applies to
# virtual devices like Loopback Audio too, not just real microphones.
cat > "${CONTENTS}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>${BIN_NAME}</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleIdentifier</key>
	<string>${BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>${APP_NAME}</string>
	<key>CFBundleDisplayName</key>
	<string>${APP_NAME}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
	<key>CFBundleVersion</key>
	<string>${VERSION}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.music</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSPrincipalClass</key>
	<string>NSApplication</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>LAN Audio Share needs access to audio input devices to capture the sound you want to stream. Choose a loopback device to share desktop audio.</string>
	<key>NSLocalNetworkUsageDescription</key>
	<string>LAN Audio Share finds other computers on your local network so it can stream audio to and from them.</string>
</dict>
</plist>
PLIST

# Ad-hoc signature. This is not about trust — it gives the app a stable identity
# so macOS remembers the permissions you grant it instead of re-prompting (or
# silently denying) after every rebuild.
echo "--> Signing (ad-hoc)"
codesign --force --sign - --identifier "$BUNDLE_ID" "$APP"

# Only matters if the bundle was downloaded rather than built here, but harmless.
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
touch "$APP"  # nudge Finder into picking up the new icon

echo
echo "Built: ${APP}"
if [[ "$UNIVERSAL" == "1" ]]; then
  echo "Architectures: $(lipo -archs "${MACOS_DIR}/${BIN_NAME}")"
fi
echo
echo "Double-click it, or drag it into /Applications."
echo "On first launch macOS will ask for Local Network access — and for"
echo "Microphone access when you pick an input device in SEND mode. Both must be"
echo "allowed or the app cannot find peers or capture audio."
