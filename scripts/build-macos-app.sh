#!/usr/bin/env bash
set -euo pipefail

RS_BOARD_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS_BOARD_ROOT="$(cd "$RS_BOARD_SCRIPT_DIR/.." && pwd)"
RS_BOARD_TARGET_DIR="$RS_BOARD_ROOT/target"
RS_BOARD_EXPECTED_BUNDLE_VERSION="cargo-bundle v0.11.0"
RS_BOARD_TARGET="aarch64-apple-darwin"
RS_BOARD_MINIMUM_MACOS="13.0"

fail() {
  echo "error: $*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

read_plist() {
  /usr/libexec/PlistBuddy -c "Print :$1" "$2"
}

verify_dynamic_libraries() {
  local executable="$1"
  local dependency
  local rpath

  while IFS= read -r dependency; do
    dependency="${dependency#"${dependency%%[![:space:]]*}"}"
    dependency="${dependency%% (*}"
    case "$dependency" in
      /System/Library/* | /usr/lib/* | @executable_path/../Frameworks/* | @loader_path/../Frameworks/*) ;;
      *) fail "unexpected dynamic library path: $dependency" ;;
    esac
  done < <(otool -L "$executable" | tail -n +2)

  while IFS= read -r rpath; do
    [[ "$rpath" == "@executable_path/../Frameworks" ]] \
      || fail "unexpected runtime search path: $rpath"
  done < <(
    otool -l "$executable" \
      | awk '$1 == "cmd" && $2 == "LC_RPATH" { found = 1; next }
             found && $1 == "path" { print $2; found = 0 }'
  )
}

read_macho_minimum_macos() {
  otool -l "$1" \
    | awk '$1 == "cmd" && $2 == "LC_BUILD_VERSION" { found = 1; next }
           found && $1 == "minos" { print $2; exit }'
}

[[ $# -eq 1 ]] || fail "usage: $0 <version>"
RS_BOARD_VERSION="$1"
[[ "$RS_BOARD_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || fail "version must use three numeric components"

[[ "$(uname -s)" == "Darwin" ]] || fail "macOS is required"
[[ "$(uname -m)" == "arm64" ]] || fail "an Apple Silicon Mac is required"

unset CARGO_BUNDLE_SKIP_BUILD
export CARGO_TARGET_DIR="$RS_BOARD_TARGET_DIR"

for command_name in cargo cargo-bundle rustup jq codesign plutil file otool lipo ditto sips; do
  require_command "$command_name"
done
[[ -x /usr/libexec/PlistBuddy ]] || fail "required command not found: /usr/libexec/PlistBuddy"

rustup target list --installed | grep -Fxq "$RS_BOARD_TARGET" \
  || fail "Rust target is not installed: $RS_BOARD_TARGET"

RS_BOARD_BUNDLE_VERSION="$(cargo-bundle --version 2>/dev/null || true)"
[[ "$RS_BOARD_BUNDLE_VERSION" == "$RS_BOARD_EXPECTED_BUNDLE_VERSION" ]] \
  || fail "expected $RS_BOARD_EXPECTED_BUNDLE_VERSION; found ${RS_BOARD_BUNDLE_VERSION:-nothing}"

cd "$RS_BOARD_ROOT"

RS_BOARD_WORKSPACE_VERSION="$(
  cargo metadata --locked --no-deps --format-version 1 \
    | jq -r '.packages[] | select(.name == "app") | .version'
)"
[[ "$RS_BOARD_VERSION" == "$RS_BOARD_WORKSPACE_VERSION" ]] \
  || fail "requested version $RS_BOARD_VERSION does not match workspace version $RS_BOARD_WORKSPACE_VERSION"

for required_file in \
  Cargo.lock \
  crates/app/assets/AppIcon-1024.png \
  crates/app/assets/AppIcon.icns \
  crates/app/assets/NotoSansSC-Regular.otf \
  crates/app/assets/macos-info-plist-ext.xml \
  crates/app/assets/THIRD_PARTY_NOTICES.txt; do
  [[ -s "$required_file" ]] || fail "required file is missing or empty: $required_file"
done

RS_BOARD_ICON_WIDTH="$(
  sips -g pixelWidth crates/app/assets/AppIcon-1024.png \
    | awk '/pixelWidth:/ { print $2 }'
)"
RS_BOARD_ICON_HEIGHT="$(
  sips -g pixelHeight crates/app/assets/AppIcon-1024.png \
    | awk '/pixelHeight:/ { print $2 }'
)"
[[ "$RS_BOARD_ICON_WIDTH" == "1024" && "$RS_BOARD_ICON_HEIGHT" == "1024" ]] \
  || fail "AppIcon-1024.png must be 1024x1024"
plutil -lint crates/app/assets/macos-info-plist-ext.xml >/dev/null

RS_BOARD_STAGE_DIR="$RS_BOARD_ROOT/.release-tmp/$RS_BOARD_VERSION"
RS_BOARD_STAGE_APP="$RS_BOARD_STAGE_DIR/RS Board.app"
[[ ! -e "$RS_BOARD_STAGE_DIR" ]] \
  || fail "staging directory already exists: $RS_BOARD_STAGE_DIR"
RS_BOARD_BUILD_COMPLETE=0

cleanup() {
  if [[ "$RS_BOARD_BUILD_COMPLETE" != "1" ]]; then
    rm -rf "$RS_BOARD_STAGE_DIR"
  fi
}
trap cleanup EXIT

echo "checking Rust sources"
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace

echo "building RS Board $RS_BOARD_VERSION for $RS_BOARD_TARGET"
MACOSX_DEPLOYMENT_TARGET="$RS_BOARD_MINIMUM_MACOS" \
  cargo build --locked --release --target "$RS_BOARD_TARGET"
(
  cd "$RS_BOARD_ROOT/crates/app"
  CARGO_BUNDLE_SKIP_BUILD=1 \
  MACOSX_DEPLOYMENT_TARGET="$RS_BOARD_MINIMUM_MACOS" \
    cargo bundle --release --target "$RS_BOARD_TARGET" --format osx
)

RS_BOARD_SOURCE_APP="$RS_BOARD_TARGET_DIR/$RS_BOARD_TARGET/release/bundle/osx/RS Board.app"
[[ -d "$RS_BOARD_SOURCE_APP" ]] || fail "cargo-bundle output not found: $RS_BOARD_SOURCE_APP"

mkdir -p "$RS_BOARD_STAGE_DIR"
ditto "$RS_BOARD_SOURCE_APP" "$RS_BOARD_STAGE_APP"

RS_BOARD_PLIST="$RS_BOARD_STAGE_APP/Contents/Info.plist"
RS_BOARD_EXECUTABLE="$RS_BOARD_STAGE_APP/Contents/MacOS/app"
RS_BOARD_ICON="$RS_BOARD_STAGE_APP/Contents/Resources/AppIcon.icns"

[[ -x "$RS_BOARD_EXECUTABLE" ]] || fail "bundle executable is missing"
[[ -f "$RS_BOARD_ICON" && ! -L "$RS_BOARD_ICON" && -s "$RS_BOARD_ICON" ]] \
  || fail "bundle icon is missing, empty, or a symbolic link"
plutil -replace CFBundleVersion -string "$RS_BOARD_VERSION" "$RS_BOARD_PLIST"
plutil -lint "$RS_BOARD_PLIST" >/dev/null

[[ "$(read_plist CFBundleIdentifier "$RS_BOARD_PLIST")" == "com.linjiajian.rs-board" ]] \
  || fail "unexpected CFBundleIdentifier"
[[ "$(read_plist CFBundleShortVersionString "$RS_BOARD_PLIST")" == "$RS_BOARD_VERSION" ]] \
  || fail "unexpected CFBundleShortVersionString"
[[ "$(read_plist CFBundleVersion "$RS_BOARD_PLIST")" == "$RS_BOARD_VERSION" ]] \
  || fail "unexpected CFBundleVersion"
[[ "$(read_plist LSMinimumSystemVersion "$RS_BOARD_PLIST")" == "$RS_BOARD_MINIMUM_MACOS" ]] \
  || fail "unexpected LSMinimumSystemVersion"
[[ "$(read_plist LSUIElement "$RS_BOARD_PLIST")" == "true" ]] \
  || fail "LSUIElement must be true"
[[ "$(read_plist CFBundleIconFile "$RS_BOARD_PLIST")" == "AppIcon.icns" ]] \
  || fail "unexpected CFBundleIconFile"
[[ "$(read_plist CFBundleDocumentTypes:0:CFBundleTypeExtensions:0 "$RS_BOARD_PLIST")" == "rsboard" ]] \
  || fail "rsboard document extension is missing"
[[ "$(read_plist UTExportedTypeDeclarations:0:UTTypeIdentifier "$RS_BOARD_PLIST")" == "com.linjiajian.rs-board.document" ]] \
  || fail "rsboard UTI is missing"

[[ "$(lipo -archs "$RS_BOARD_EXECUTABLE")" == "arm64" ]] \
  || fail "bundle executable must contain only arm64"
[[ "$(read_macho_minimum_macos "$RS_BOARD_EXECUTABLE")" == "$RS_BOARD_MINIMUM_MACOS" ]] \
  || fail "bundle executable must require macOS $RS_BOARD_MINIMUM_MACOS"

verify_dynamic_libraries "$RS_BOARD_EXECUTABLE"

echo "applying ad-hoc signature"
codesign --force --sign - "$RS_BOARD_STAGE_APP"
codesign --verify --deep --strict --verbose=2 "$RS_BOARD_STAGE_APP"

RS_BOARD_SIGNATURE_INFO="$(codesign --display --verbose=4 "$RS_BOARD_STAGE_APP" 2>&1)"
grep -q '^Signature=adhoc$' <<<"$RS_BOARD_SIGNATURE_INFO" \
  || fail "bundle does not have an ad-hoc signature"

RS_BOARD_ENTITLEMENTS="$(
  codesign --display --entitlements :- "$RS_BOARD_STAGE_APP" 2>/dev/null
)" || fail "could not inspect bundle entitlements"
if grep -q '<key>' <<<"$RS_BOARD_ENTITLEMENTS"; then
  fail "release bundle contains unexpected entitlements"
fi

RS_BOARD_BUILD_COMPLETE=1
echo "built: $RS_BOARD_STAGE_APP"
