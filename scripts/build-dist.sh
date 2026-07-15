#!/usr/bin/env bash
# Build the installable for THIS machine's OS into ./dist.
#
# One machine can only build its own platform's GUI installer — Mac makes a
# .dmg, Linux makes a .deb + tarball. To get every platform's installer in one
# place, push a `v*` git tag and let CI (.github/workflows/release.yml) build
# macOS + Windows + Linux and attach them to the GitHub Release.
set -euo pipefail
cd "$(dirname "$0")/.."

mkdir -p dist

if ! command -v cargo-bundle >/dev/null 2>&1; then
  echo "cargo-bundle not found. Install it with:  cargo install cargo-bundle"
  exit 1
fi

cargo build --release --package zap-cli

case "$(uname -s)" in
  Darwin)
    # Run from the crate dir so cargo-bundle finds the icon paths.
    ( cd crates/zap-desktop && cargo bundle --release )
    cp target/release/bundle/dmg/zap.dmg dist/zap-macos.dmg
    cp target/release/zap dist/zap-macos-cli
    echo "✅ dist/zap-macos.dmg  +  dist/zap-macos-cli"
    ;;
  Linux)
    ( cd crates/zap-desktop && cargo bundle --release --format deb )
    cp target/release/bundle/deb/*.deb dist/
    cp target/release/zap-desktop dist/zap-linux
    cp target/release/zap dist/zap-linux-cli
    echo "✅ dist/*.deb  +  dist/zap-linux  +  dist/zap-linux-cli"
    ;;
  *)
    echo "This script builds macOS/Linux. For Windows, run on Windows:"
    echo "  cargo build --release --package zap-desktop --package zap-cli"
    exit 1
    ;;
esac

echo
echo "Note: this only built $(uname -s). For ALL platforms at once, tag a release:"
echo "  git tag v0.1.0 && git push --tags   (CI builds macOS + Windows + Linux)"
