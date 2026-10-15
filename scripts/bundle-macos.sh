#!/usr/bin/env bash
set -euo pipefail

if [[ $(uname -s) != Darwin ]]; then
  echo "Build this bundle on macOS (Xcode command-line tools and Rust 1.90+ required)." >&2
  exit 1
fi

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bundle=${1:-"$root/dist/Intercom TUI.app"}

cargo build --manifest-path "$root/Cargo.toml" --release --locked
install -d "$bundle/Contents/MacOS"
install -m 755 "$root/target/release/intercom-tui" "$bundle/Contents/MacOS/intercom-tui"
install -m 644 "$root/packaging/macos/Info.plist" "$bundle/Contents/Info.plist"
codesign --force --sign - "$bundle"

echo "Created $bundle"
echo "Run from your terminal: \"$bundle/Contents/MacOS/intercom-tui\""
