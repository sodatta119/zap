#!/usr/bin/env bash
# Build the installables for THIS machine's OS into dist/<product>/.
#
# Layout (per product, so each app's downloads sit together):
#   dist/zap/   zap-macos.dmg  zap-macos-cli  zap-linux.deb  ...
#   dist/zulu/  zulu-macos.dmg  zulu-android.apk  zulu-linux.deb  ...
#
# One machine only builds its own desktop platform (Mac -> .dmg, Linux -> .deb);
# the Android .apk builds anywhere an Android SDK is present. To get every
# platform's installer (incl. Windows .zip) in one place, push a `v*` git tag and
# let CI (.github/workflows/release.yml) build them into the same dist/<product>/
# layout and attach them to the GitHub Release.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"                 # repo root - dist/ lives here
ZAP="$ROOT/dist/zap"
ZULU="$ROOT/dist/zulu"
mkdir -p "$ZAP" "$ZULU"

if ! command -v cargo-bundle >/dev/null 2>&1; then
  echo "cargo-bundle not found. Install it with:  cargo install cargo-bundle"
  exit 1
fi

# Build the Zulu Android APK into dist/zulu/ if an Android SDK is available.
# Pure Kotlin, so no NDK/Rust cross-compile - just the SDK + the gradle wrapper.
build_apk() {
  local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
  if [ -z "$sdk" ]; then
    case "$(uname -s)" in
      Darwin) [ -d "$HOME/Library/Android/sdk" ] && sdk="$HOME/Library/Android/sdk" ;;
      Linux)  [ -d "$HOME/Android/Sdk" ] && sdk="$HOME/Android/Sdk" ;;
    esac
  fi
  if [ -z "$sdk" ] || [ ! -d "$sdk" ]; then
    echo "ℹ️  Android SDK not found - skipping the Zulu APK (set ANDROID_HOME to build it)."
    return 0
  fi
  echo "Building Zulu APK..."
  ( cd "$ROOT/networking/android/zulu" \
      && ANDROID_HOME="$sdk" ANDROID_SDK_ROOT="$sdk" ./gradlew :app:assembleDebug --console=plain -q )
  cp "$ROOT/networking/android/zulu/app/build/outputs/apk/debug/app-debug.apk" "$ZULU/zulu-android.apk"
  echo "✅ dist/zulu/zulu-android.apk"
}

# The Cargo workspace lives under networking/ (zOrigin category layout). Run all
# cargo commands from there; target/ is networking/target, dist/ stays at repo root.
cd "$ROOT/networking"

case "$(uname -s)" in
  Darwin)
    # Universal (Intel + Apple Silicon): build both arches, then `lipo` them into
    # one fat binary so a single .dmg runs natively on any Mac. cargo-bundle only
    # builds one arch, so we bundle for the .app skeleton (icon + Info.plist) and
    # then swap in the universal binary and repackage the .dmg ourselves.
    ARM=aarch64-apple-darwin
    X86=x86_64-apple-darwin
    for t in "$ARM" "$X86"; do rustup target add "$t" >/dev/null 2>&1 || true; done

    # --- Zap (file transfer): universal CLI + GUI ---
    cargo build --release --package zap-cli --package zap-desktop \
      --target "$ARM" --target "$X86"
    lipo -create -output "$ZAP/zap-macos-cli" \
      "target/$X86/release/zap" "target/$ARM/release/zap"
    ( cd crates/zap-desktop && cargo bundle --release )
    APP=target/release/bundle/osx/zap.app
    lipo -create -output "$APP/Contents/MacOS/zap-desktop" \
      "target/$X86/release/zap-desktop" "target/$ARM/release/zap-desktop"
    STAGE=target/zap-dmg
    rm -rf "$STAGE"; mkdir -p "$STAGE"
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    rm -f "$ZAP/zap-macos.dmg"
    hdiutil create -volname zap -srcfolder "$STAGE" -ov -format UDZO "$ZAP/zap-macos.dmg" >/dev/null
    echo "✅ dist/zap/zap-macos.dmg (universal)  +  dist/zap/zap-macos-cli (universal)"
    lipo -info "$ZAP/zap-macos-cli"

    # --- Zulu (clipboard sync): universal GUI, no CLI ---
    cargo build --release --package zulu-desktop --target "$ARM" --target "$X86"
    ( cd crates/zulu-desktop && cargo bundle --release )
    ZAPP=target/release/bundle/osx/Zulu.app
    lipo -create -output "$ZAPP/Contents/MacOS/zulu-desktop" \
      "target/$X86/release/zulu-desktop" "target/$ARM/release/zulu-desktop"
    ZSTAGE=target/zulu-dmg
    rm -rf "$ZSTAGE"; mkdir -p "$ZSTAGE"
    cp -R "$ZAPP" "$ZSTAGE/"
    ln -s /Applications "$ZSTAGE/Applications"
    rm -f "$ZULU/zulu-macos.dmg"
    hdiutil create -volname Zulu -srcfolder "$ZSTAGE" -ov -format UDZO "$ZULU/zulu-macos.dmg" >/dev/null
    echo "✅ dist/zulu/zulu-macos.dmg (universal)"

    build_apk
    ;;
  Linux)
    # --- Zap ---
    cargo build --release --package zap-cli
    ( cd crates/zap-desktop && cargo bundle --release --format deb )
    cp target/release/bundle/deb/zap*.deb "$ZAP/zap-linux.deb"
    cp target/release/zap-desktop "$ZAP/zap-linux"
    cp target/release/zap "$ZAP/zap-linux-cli"
    echo "✅ dist/zap/zap-linux.deb  +  dist/zap/zap-linux  +  dist/zap/zap-linux-cli"

    # --- Zulu (desktop app only) ---
    ( cd crates/zulu-desktop && cargo bundle --release --format deb )
    cp target/release/bundle/deb/[Zz]ulu*.deb "$ZULU/zulu-linux.deb"
    cp target/release/zulu-desktop "$ZULU/zulu-linux"
    echo "✅ dist/zulu/zulu-linux.deb  +  dist/zulu/zulu-linux"

    build_apk
    ;;
  *)
    echo "This script builds macOS/Linux desktop installers. For Windows, run on Windows:"
    echo "  cargo build --release --package zap-desktop --package zulu-desktop --package zap-cli"
    exit 1
    ;;
esac

echo
echo "dist/ tree:"
( cd "$ROOT" && find dist -type f | sort | sed 's/^/  /' )
echo
echo "Note: this built $(uname -s) desktop (+ APK if an SDK was found). For ALL"
echo "platforms at once (incl. Windows .zip), tag a release:"
echo "  git tag v0.1.0 && git push --tags   (CI -> dist/<product>/ release assets)"
