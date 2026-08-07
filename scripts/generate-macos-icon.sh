#!/usr/bin/env bash
set -euo pipefail

RS_BOARD_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS_BOARD_ROOT="$(cd "$RS_BOARD_SCRIPT_DIR/.." && pwd)"
RS_BOARD_ASSET_DIR="$RS_BOARD_ROOT/crates/app/assets"
RS_BOARD_SOURCE_SVG="$RS_BOARD_ASSET_DIR/AppIcon.svg"
RS_BOARD_SOURCE_PNG="$RS_BOARD_ASSET_DIR/AppIcon-1024.png"
RS_BOARD_ICNS="$RS_BOARD_ASSET_DIR/AppIcon.icns"
RS_BOARD_EXPECTED_BUNDLE_VERSION="cargo-bundle v0.11.0"

fail() {
  echo "error: $*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

[[ $# -eq 0 ]] || fail "usage: $0"
[[ "$(uname -s)" == "Darwin" ]] || fail "macOS is required"

for command_name in cargo cargo-bundle iconutil sips; do
  require_command "$command_name"
done
[[ "$(cargo-bundle --version)" == "$RS_BOARD_EXPECTED_BUNDLE_VERSION" ]] \
  || fail "expected $RS_BOARD_EXPECTED_BUNDLE_VERSION"
[[ -s "$RS_BOARD_SOURCE_SVG" ]] || fail "icon source is missing: $RS_BOARD_SOURCE_SVG"

RS_BOARD_TEMP_DIR="$(mktemp -d /tmp/rs-board-icon.XXXXXX)"
RS_BOARD_BUNDLE_DIR="$RS_BOARD_TEMP_DIR/icon-bundle"
RS_BOARD_TARGET_DIR="$RS_BOARD_TEMP_DIR/target"
RS_BOARD_ICONSET="$RS_BOARD_TEMP_DIR/AppIcon.iconset"
RS_BOARD_GENERATED_ICNS="$RS_BOARD_TARGET_DIR/release/bundle/osx/RS Board Icon.app/Contents/Resources/RS Board Icon.icns"

cleanup() {
  rm -rf "$RS_BOARD_TEMP_DIR"
}
trap cleanup EXIT

mkdir -p "$RS_BOARD_BUNDLE_DIR/src" "$RS_BOARD_TARGET_DIR/release"
cp "$RS_BOARD_SOURCE_SVG" "$RS_BOARD_BUNDLE_DIR/AppIcon.svg"
cp /usr/bin/true "$RS_BOARD_TARGET_DIR/release/rs-board-icon"
: >"$RS_BOARD_BUNDLE_DIR/src/main.rs"

cat >"$RS_BOARD_BUNDLE_DIR/Cargo.toml" <<'TOML'
[package]
name = "rs-board-icon"
version = "0.0.0"
edition = "2024"
description = "Temporary RS Board icon bundle"
license = "MIT"

[package.metadata.bundle]
name = "RS Board Icon"
identifier = "com.linjiajian.rs-board.icon-generator"
icon = ["AppIcon.svg"]
TOML

(
  cd "$RS_BOARD_BUNDLE_DIR"
  CARGO_BUNDLE_SKIP_BUILD=1 CARGO_TARGET_DIR="$RS_BOARD_TARGET_DIR" \
    cargo bundle --release --format osx >/dev/null
)

[[ -s "$RS_BOARD_GENERATED_ICNS" ]] || fail "cargo-bundle did not generate an icon"
iconutil -c iconset "$RS_BOARD_GENERATED_ICNS" -o "$RS_BOARD_ICONSET"

RS_BOARD_GENERATED_PNG="$RS_BOARD_ICONSET/icon_512x512@2x.png"
[[ -s "$RS_BOARD_GENERATED_PNG" ]] || fail "generated ICNS is missing its 1024px representation"

RS_BOARD_ICON_WIDTH="$(
  sips -g pixelWidth "$RS_BOARD_GENERATED_PNG" | awk '/pixelWidth:/ { print $2 }'
)"
RS_BOARD_ICON_HEIGHT="$(
  sips -g pixelHeight "$RS_BOARD_GENERATED_PNG" | awk '/pixelHeight:/ { print $2 }'
)"
[[ "$RS_BOARD_ICON_WIDTH" == "1024" && "$RS_BOARD_ICON_HEIGHT" == "1024" ]] \
  || fail "generated icon source must be 1024x1024"

cp "$RS_BOARD_GENERATED_PNG" "$RS_BOARD_SOURCE_PNG"
cp "$RS_BOARD_GENERATED_ICNS" "$RS_BOARD_ICNS"

echo "generated: $RS_BOARD_SOURCE_PNG"
echo "generated: $RS_BOARD_ICNS"
