#!/usr/bin/env bash
#
# Builds "LAN Audio Share.app" — a double-clickable macOS bundle.
#
#   bash scripts/make-macos-app.sh           # universal: Apple Silicon + Intel
#   bash scripts/make-macos-app.sh --native  # this machine only, builds quicker
#
# The bundle lands in target/release/ and can be dragged straight into
# /Applications. Nothing but Xcode command line tools and Rust is required.
#
# Universal is the default so the result runs on any Mac. It compiles everything
# twice, so it takes roughly twice as long; --native is there for quick local
# iteration. Either way the finished architectures are printed at the end.
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

UNIVERSAL=1
for arg in "$@"; do
  case "$arg" in
    --native) UNIVERSAL=0 ;;
    # Accepted so older invocations and docs keep working; it is now the default.
    --universal) UNIVERSAL=1 ;;
    -h|--help)
      # Print the comment header, however long it happens to be.
      awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"
      exit 0
      ;;
    *) echo "error: unknown option '$arg'" >&2; exit 1 ;;
  esac
done

# Fail on an old toolchain here, with the fix spelled out, rather than letting
# it surface as a wall of unmet dependency requirements partway through a build.
REQUIRED_RUSTC="$(sed -n 's/^rust-version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
if [[ -n "$REQUIRED_RUSTC" ]] && command -v rustc >/dev/null; then
  HAVE_RUSTC="$(rustc --version | awk '{print $2}')"
  if ! awk -v have="$HAVE_RUSTC" -v need="$REQUIRED_RUSTC" 'BEGIN {
        split(have, h, /[.+-]/); split(need, n, ".");
        exit (h[1] > n[1] || (h[1] == n[1] && h[2] >= n[2])) ? 0 : 1
      }'; then
    echo "error: this project needs Rust ${REQUIRED_RUSTC} or newer, but rustc is ${HAVE_RUSTC}." >&2
    echo "       run:  rustup update" >&2
    exit 1
  fi
fi

# Version comes from Cargo.toml so the bundle never disagrees with the binary.
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
VERSION="${VERSION:-0.1.0}"

APP="target/release/${APP_NAME}.app"
CONTENTS="${APP}/Contents"
MACOS_DIR="${CONTENTS}/MacOS"
RESOURCES="${CONTENTS}/Resources"

echo "==> Building ${APP_NAME} ${VERSION}"

build_both_arches() {
  # Never swallow the reason this fails: the message is the whole point when
  # something goes wrong, and guessing at a cause here actively misleads.
  if ! rustup target add x86_64-apple-darwin aarch64-apple-darwin 2>&1 |
    sed 's/^/    /'; then
    return 1
  fi
  cargo build --release --target x86_64-apple-darwin || return 1
  cargo build --release --target aarch64-apple-darwin || return 1
}

if [[ "$UNIVERSAL" == "1" ]]; then
  echo "--> Building for Apple Silicon and Intel (this takes about twice as long)"
  # Cross-compiling should cost the second architecture, not the whole build,
  # so fall back to native rather than failing outright.
  if ! build_both_arches; then
    echo
    echo "    warning: could not build for both architectures (see the error above)."
    echo "             Falling back to a build for this machine only; the app will"
    echo "             not run on other Macs."
    echo
    UNIVERSAL=0
    cargo build --release
  fi
else
  echo "--> Building for this machine only"
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
  echo "             (regenerate it with: python3 tools/make_icons.py)"
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
# Always stated, so there is never any doubt about what actually got produced.
echo "Architectures: $(lipo -archs "${MACOS_DIR}/${BIN_NAME}" 2>/dev/null || echo unknown)"
echo
echo "Double-click it, or drag it into /Applications."
echo "On first launch macOS will ask for Local Network access — and for"
echo "Microphone access when you pick an input device in SEND mode. Both must be"
echo "allowed or the app cannot find peers or capture audio."
