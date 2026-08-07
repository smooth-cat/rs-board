#!/usr/bin/env bash
set -euo pipefail

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

attached_devices_for_dmg() {
  hdiutil info -plist \
    | plutil -convert json -o - - \
    | jq -r --arg image_path "$RS_BOARD_DMG" '
        .images[]?
        | select(."image-path" == $image_path)
        | ."system-entities"[0]."dev-entry" // empty
      '
}

detach_dmg() {
  local device
  local devices

  devices="$(attached_devices_for_dmg)" || return 1
  [[ -n "$devices" ]] || return 0

  while IFS= read -r device; do
    [[ -n "$device" ]] || continue
    if ! hdiutil detach "$device" -quiet >/dev/null 2>&1; then
      hdiutil detach "$device" -force -quiet >/dev/null 2>&1
    fi
  done <<<"$devices"
}

[[ $# -eq 1 ]] || fail "usage: $0 <path-to-dmg>"

for command_name in shasum hdiutil jq codesign plutil lipo otool spctl; do
  require_command "$command_name"
done
[[ -x /usr/libexec/PlistBuddy ]] || fail "required command not found: /usr/libexec/PlistBuddy"

[[ -f "$1" ]] || fail "DMG not found: $1"
RS_BOARD_DMG_DIR="$(cd "$(dirname "$1")" && pwd)"
RS_BOARD_DMG_NAME="$(basename "$1")"
RS_BOARD_DMG="$RS_BOARD_DMG_DIR/$RS_BOARD_DMG_NAME"
RS_BOARD_CHECKSUM="$RS_BOARD_DMG.sha256"

[[ "$RS_BOARD_DMG_NAME" =~ ^RS-Board-([0-9]+\.[0-9]+\.[0-9]+)-macos-arm64\.dmg$ ]] \
  || fail "unexpected DMG filename: $RS_BOARD_DMG_NAME"
RS_BOARD_EXPECTED_VERSION="${BASH_REMATCH[1]}"
[[ -f "$RS_BOARD_CHECKSUM" ]] || fail "checksum not found: $RS_BOARD_CHECKSUM"

RS_BOARD_EXPECTED_CHECKSUM_LINE="$(
  cd "$RS_BOARD_DMG_DIR"
  shasum -a 256 "$RS_BOARD_DMG_NAME"
)"
RS_BOARD_ACTUAL_CHECKSUM_LINE="$(<"$RS_BOARD_CHECKSUM")"
RS_BOARD_CHECKSUM_LINE_COUNT="$(wc -l <"$RS_BOARD_CHECKSUM" | tr -d ' ')"
[[ "$RS_BOARD_CHECKSUM_LINE_COUNT" == "1" ]] \
  || fail "checksum file must contain exactly one line"
[[ "$RS_BOARD_ACTUAL_CHECKSUM_LINE" == "$RS_BOARD_EXPECTED_CHECKSUM_LINE" ]] \
  || fail "checksum does not match $RS_BOARD_DMG_NAME"

RS_BOARD_MOUNT_DIR="$(mktemp -d /tmp/rs-board-dmg.XXXXXX)"
RS_BOARD_ATTACH_LOG="$(mktemp /tmp/rs-board-dmg-attach.XXXXXX)"

cleanup() {
  detach_dmg >/dev/null 2>&1 || true
  rm -rf "$RS_BOARD_MOUNT_DIR"
  rm -f "$RS_BOARD_ATTACH_LOG"
}
trap cleanup EXIT

# 同一路径的上次失败镜像可能仍处于“已附加但未挂载”状态。
detach_dmg || fail "could not detach a stale DMG device"

RS_BOARD_ATTACH_ATTEMPT=1
while [[ "$RS_BOARD_ATTACH_ATTEMPT" -le 3 ]]; do
  if hdiutil attach "$RS_BOARD_DMG" \
    -readonly \
    -nobrowse \
    -mountpoint "$RS_BOARD_MOUNT_DIR" \
    >/dev/null 2>"$RS_BOARD_ATTACH_LOG"; then
    break
  fi

  detach_dmg || fail "could not clean up a partially attached DMG"
  if [[ "$RS_BOARD_ATTACH_ATTEMPT" -eq 3 ]]; then
    cat "$RS_BOARD_ATTACH_LOG" >&2
    fail "could not attach DMG after 3 attempts"
  fi

  echo "DMG attach attempt $RS_BOARD_ATTACH_ATTEMPT failed; retrying" >&2
  sleep 1
  RS_BOARD_ATTACH_ATTEMPT=$((RS_BOARD_ATTACH_ATTEMPT + 1))
done

RS_BOARD_APP="$RS_BOARD_MOUNT_DIR/RS Board.app"
RS_BOARD_PLIST="$RS_BOARD_APP/Contents/Info.plist"
RS_BOARD_EXECUTABLE="$RS_BOARD_APP/Contents/MacOS/app"
RS_BOARD_ICON="$RS_BOARD_APP/Contents/Resources/AppIcon.icns"

[[ -d "$RS_BOARD_APP" && ! -L "$RS_BOARD_APP" ]] \
  || fail "RS Board.app is missing or is a symbolic link"
[[ -L "$RS_BOARD_MOUNT_DIR/Applications" ]] || fail "Applications link is missing"
[[ "$(readlink "$RS_BOARD_MOUNT_DIR/Applications")" == "/Applications" ]] \
  || fail "Applications link has an unexpected target"
[[ -f "$RS_BOARD_MOUNT_DIR/README.txt" && ! -L "$RS_BOARD_MOUNT_DIR/README.txt" \
  && -s "$RS_BOARD_MOUNT_DIR/README.txt" ]] \
  || fail "README.txt is missing, empty, or a symbolic link"
[[ -f "$RS_BOARD_MOUNT_DIR/THIRD_PARTY_NOTICES.txt" \
  && ! -L "$RS_BOARD_MOUNT_DIR/THIRD_PARTY_NOTICES.txt" \
  && -s "$RS_BOARD_MOUNT_DIR/THIRD_PARTY_NOTICES.txt" ]] \
  || fail "THIRD_PARTY_NOTICES.txt is missing, empty, or a symbolic link"
[[ -f "$RS_BOARD_ICON" && ! -L "$RS_BOARD_ICON" && -s "$RS_BOARD_ICON" ]] \
  || fail "AppIcon.icns is missing, empty, or a symbolic link"

RS_BOARD_ROOT_ITEM_COUNT="$(find "$RS_BOARD_MOUNT_DIR" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')"
[[ "$RS_BOARD_ROOT_ITEM_COUNT" == "4" ]] \
  || fail "DMG root contains unexpected files"

plutil -lint "$RS_BOARD_PLIST" >/dev/null
codesign --verify --deep --strict --verbose=2 "$RS_BOARD_APP"

RS_BOARD_SIGNATURE_INFO="$(codesign --display --verbose=4 "$RS_BOARD_APP" 2>&1)"
grep -q '^Signature=adhoc$' <<<"$RS_BOARD_SIGNATURE_INFO" \
  || fail "app does not have an ad-hoc signature"

RS_BOARD_ENTITLEMENTS="$(
  codesign --display --entitlements :- "$RS_BOARD_APP" 2>/dev/null
)" || fail "could not inspect app entitlements"
if grep -q '<key>' <<<"$RS_BOARD_ENTITLEMENTS"; then
  fail "app contains unexpected entitlements"
fi

[[ "$(read_plist CFBundleIdentifier "$RS_BOARD_PLIST")" == "com.linjiajian.rs-board" ]] \
  || fail "unexpected CFBundleIdentifier"
[[ "$(read_plist CFBundleShortVersionString "$RS_BOARD_PLIST")" == "$RS_BOARD_EXPECTED_VERSION" ]] \
  || fail "unexpected CFBundleShortVersionString"
[[ "$(read_plist CFBundleVersion "$RS_BOARD_PLIST")" == "$RS_BOARD_EXPECTED_VERSION" ]] \
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
[[ -x "$RS_BOARD_EXECUTABLE" ]] || fail "bundle executable is missing"
[[ "$(lipo -archs "$RS_BOARD_EXECUTABLE")" == "arm64" ]] \
  || fail "bundle executable must contain only arm64"
[[ "$(read_macho_minimum_macos "$RS_BOARD_EXECUTABLE")" == "$RS_BOARD_MINIMUM_MACOS" ]] \
  || fail "bundle executable must require macOS $RS_BOARD_MINIMUM_MACOS"

verify_dynamic_libraries "$RS_BOARD_EXECUTABLE"

if spctl --assess --type execute --verbose=4 "$RS_BOARD_APP" >/dev/null 2>&1; then
  echo "Gatekeeper accepted the app under the current system policy"
else
  echo "Gatekeeper rejected the non-notarized app as expected; friends must use Open Anyway"
fi

detach_dmg || fail "could not detach verified DMG"

echo "verified: $RS_BOARD_DMG"
