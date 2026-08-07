#!/usr/bin/env bash
# 使用 bash 执行（macOS 自带 bash 3.2，脚本用到的语法均兼容）

# 开启严格模式：
#   -e    任一命令出错立即退出
#   -u    使用未定义变量立即退出
#   -o pipefail  管道中任一命令出错则整个管道视为失败
set -euo pipefail

# 当前脚本所在目录（resolve 符号链接后取绝对路径），即 scripts/
RS_BOARD_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 项目根目录，即 scripts/ 的上一级
RS_BOARD_ROOT="$(cd "$RS_BOARD_SCRIPT_DIR/.." && pwd)"

# 打印错误信息到 stderr 并以退出码 1 结束脚本
fail() {
  echo "error: $*" >&2
  exit 1
}

# 检查命令是否存在，不存在则调用 fail 报错退出
require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

# 默认使用 workspace 当前版本打包；--update 才自动升级 patch 版本并更新版本文件。
RS_BOARD_UPDATE_VERSION=0
if [[ $# -eq 1 && "$1" == "--update" ]]; then
  RS_BOARD_UPDATE_VERSION=1
elif [[ $# -ne 0 ]]; then
  fail "usage: $0 [--update]"
fi

# 仅支持 macOS（hdiutil/ditto 是 macOS 专属工具）
[[ "$(uname -s)" == "Darwin" ]] || fail "macOS is required"
# 逐个检查打包所需的命令：
#   cargo   编译 Rust 工程
#   ditto   复制 .app 并保留权限/结构
#   hdiutil 创建 DMG 镜像
#   jq      解析 cargo metadata 的 JSON 输出
#   shasum  生成 sha256 校验和
for command_name in cargo ditto hdiutil jq shasum; do
  require_command "$command_name"
done

# 切换到项目根目录，后续所有相对路径都以根目录为基准
cd "$RS_BOARD_ROOT"

# 从 Cargo.toml 的 [workspace.package] 中读取当前版本号：
#   cargo metadata --locked  按 Cargo.lock 锁定版本解析
#   --offline                不访问网络
#   --no-deps                不解析依赖（更快）
#   jq -er 选取名为 "app" 的包，要求恰好一个，取其 version 字段
# ——这就是"当前版本"的来源，改版本只需修改 Cargo.toml 里的 version
RS_BOARD_CURRENT_VERSION="$(
  cargo metadata --locked --offline --no-deps --format-version 1 \
    | jq -er \
      '[.packages[] | select(.name == "app")]
       | if length == 1 then .[0].version else error("expected one app package") end'
)"
# 校验当前版本必须形如 x.y.z（三段纯数字），否则拒绝打包
[[ "$RS_BOARD_CURRENT_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || fail "workspace version must use three numeric components"

if [[ "$RS_BOARD_UPDATE_VERSION" == "1" ]]; then
  # 用 IFS=. 把 "x.y.z" 按点拆成三个数字分别存入主/次/补丁号
  IFS=. read -r RS_BOARD_MAJOR RS_BOARD_MINOR RS_BOARD_PATCH \
    <<<"$RS_BOARD_CURRENT_VERSION"
  # --update 发布版本 = 补丁号 + 1（如 0.1.0 -> 0.1.1）。
  RS_BOARD_VERSION="$RS_BOARD_MAJOR.$RS_BOARD_MINOR.$((10#$RS_BOARD_PATCH + 1))"
else
  # 保持版本模式直接使用 workspace 当前版本，构建与产物仍保持版本一致。
  RS_BOARD_VERSION="$RS_BOARD_CURRENT_VERSION"
fi

# 打包的临时工作目录（以发布版本号命名）
RS_BOARD_STAGE_DIR="$RS_BOARD_ROOT/.release-tmp/$RS_BOARD_VERSION"
# build-macos-app.sh 产出的 .app 位置
RS_BOARD_STAGE_APP="$RS_BOARD_STAGE_DIR/RS Board.app"
# DMG 内容暂存根目录（最终会被 hdiutil 打成镜像）
RS_BOARD_DMG_ROOT="$RS_BOARD_STAGE_DIR/dmg-root"
# 成品输出目录 dist/
RS_BOARD_DIST_DIR="$RS_BOARD_ROOT/dist"
# DMG 文件名（版本号体现在文件名里）
RS_BOARD_DMG_NAME="RS-Board-$RS_BOARD_VERSION-macos-arm64.dmg"
RS_BOARD_DMG="$RS_BOARD_DIST_DIR/$RS_BOARD_DMG_NAME"
# 校验和文件路径
RS_BOARD_CHECKSUM="$RS_BOARD_DMG.sha256"

# 前置检查：同版本的临时目录已存在 -> 拒绝（说明上次打包没清理干净）
[[ ! -e "$RS_BOARD_STAGE_DIR" ]] \
  || fail "staging directory already exists: $RS_BOARD_STAGE_DIR"
# distribution/README.txt 必须存在且非空（会打进 DMG）
[[ -s "$RS_BOARD_ROOT/distribution/README.txt" ]] \
  || fail "distribution README is missing or empty"
# 第三方许可声明必须与生成脚本一致（--check 只比对不生成）
"$RS_BOARD_SCRIPT_DIR/generate-third-party-notices.sh" --check
# THIRD_PARTY_NOTICES.txt 必须存在且非空
[[ -s "$RS_BOARD_ROOT/crates/app/assets/THIRD_PARTY_NOTICES.txt" ]] \
  || fail "third-party notices are missing or empty"

# 打包是否完整完成（成功置 1，失败保持 0，供 cleanup 判断是否要清理产物）
RS_BOARD_PACKAGE_COMPLETE=0
# Cargo.toml 中的版本是否已被改动（置 1 后 cleanup 会负责回滚）
RS_BOARD_VERSION_UPDATED=0
# 版本文件的备份目录；仅自动升级版本时创建。
RS_BOARD_VERSION_BACKUP_DIR=""
# 临时新 Cargo.toml 的路径（写入完成后清空，供 cleanup 判断）
RS_BOARD_MANIFEST_TEMP=""

# ===== 清理/回滚逻辑（版本回滚就靠这里）=====
# 脚本无论成功还是失败，退出前都会触发本函数（trap EXIT）
cleanup() {
  # 1. 删除打包临时目录
  rm -rf "$RS_BOARD_STAGE_DIR"
  # 2. 删除未成功 mv 的临时 Cargo.toml
  if [[ -n "$RS_BOARD_MANIFEST_TEMP" ]]; then
    rm -f "$RS_BOARD_MANIFEST_TEMP"
  fi
  # 3. 打包未完整完成时（中途失败）：
  if [[ "$RS_BOARD_PACKAGE_COMPLETE" != "1" ]]; then
    # 删除可能已生成的 DMG 和校验和，避免留下残缺产物
    rm -f "$RS_BOARD_DMG" "$RS_BOARD_CHECKSUM"
    # 若版本号已被修改过，则用备份把 Cargo.toml / Cargo.lock 还原：
    # 即"回滚版本"——失败时版本号回到打包前的 x.y.z
    if [[ "$RS_BOARD_VERSION_UPDATED" == "1" ]]; then
      cp "$RS_BOARD_VERSION_BACKUP_DIR/Cargo.toml" "$RS_BOARD_ROOT/Cargo.toml"
      cp "$RS_BOARD_VERSION_BACKUP_DIR/Cargo.lock" "$RS_BOARD_ROOT/Cargo.lock"
    fi
  fi
  # 4. 删除备份目录本身
  if [[ -n "$RS_BOARD_VERSION_BACKUP_DIR" ]]; then
    rm -rf "$RS_BOARD_VERSION_BACKUP_DIR"
  fi
}
# 注册：脚本退出（包括出错退出）时自动执行 cleanup
trap cleanup EXIT

if [[ "$RS_BOARD_UPDATE_VERSION" == "1" ]]; then
  RS_BOARD_VERSION_BACKUP_DIR="$(mktemp -d /tmp/rs-board-version.XXXXXX)"
  # 先把当前的 Cargo.toml / Cargo.lock 备份到备份目录，供失败回滚使用
  cp "$RS_BOARD_ROOT/Cargo.toml" "$RS_BOARD_VERSION_BACKUP_DIR/Cargo.toml"
  cp "$RS_BOARD_ROOT/Cargo.lock" "$RS_BOARD_VERSION_BACKUP_DIR/Cargo.lock"

  # 创建临时文件用于写入改版后的 Cargo.toml
  RS_BOARD_MANIFEST_TEMP="$(mktemp "$RS_BOARD_ROOT/.Cargo.toml.XXXXXX")"

  # ===== 更新版本号（核心步骤，awk 只改一处，不破坏其它内容）=====
  # 仅替换 [workspace.package] 中与当前版本相符的 version。
  if ! awk \
    -v current="$RS_BOARD_CURRENT_VERSION" \
    -v next_version="$RS_BOARD_VERSION" '
      /^\[workspace\.package\][[:space:]]*$/ {
        in_workspace_package = 1
        print
        next
      }
      /^\[/ {
        in_workspace_package = 0
      }
      in_workspace_package && /^[[:space:]]*version[[:space:]]*=/ {
        value = $0
        gsub(/[[:space:]]/, "", value)
        if (value != "version=\"" current "\"") {
          exit 1
        }
        print "version = \"" next_version "\""
        updated++
        next
      }
      { print }
      END {
        if (updated != 1) {
          exit 1
        }
      }
    ' "$RS_BOARD_ROOT/Cargo.toml" >"$RS_BOARD_MANIFEST_TEMP"; then
    fail "could not update workspace version in Cargo.toml"
  fi
  chmod 0644 "$RS_BOARD_MANIFEST_TEMP"
  RS_BOARD_VERSION_UPDATED=1
  mv "$RS_BOARD_MANIFEST_TEMP" "$RS_BOARD_ROOT/Cargo.toml"
  RS_BOARD_MANIFEST_TEMP=""

  # Cargo.toml 版本变更后同步 Cargo.lock，并确认新版本已生效。
  cargo metadata --offline --format-version 1 >/dev/null
  RS_BOARD_UPDATED_VERSION="$(
    cargo metadata --locked --offline --no-deps --format-version 1 \
      | jq -er \
        '[.packages[] | select(.name == "app")]
         | if length == 1 then .[0].version else error("expected one app package") end'
  )"
  [[ "$RS_BOARD_UPDATED_VERSION" == "$RS_BOARD_VERSION" ]] \
    || fail "workspace version did not update to $RS_BOARD_VERSION"

  echo "workspace version: $RS_BOARD_CURRENT_VERSION -> $RS_BOARD_VERSION"
else
  echo "workspace version unchanged: $RS_BOARD_VERSION"
fi

# 版本处理成功后、调用构建脚本前，删除 dist 下的全部旧产物。
if [[ -e "$RS_BOARD_DIST_DIR" && ! -d "$RS_BOARD_DIST_DIR" ]]; then
  fail "distribution path is not a directory: $RS_BOARD_DIST_DIR"
fi
mkdir -p "$RS_BOARD_DIST_DIR"
echo "clearing distribution directory: $RS_BOARD_DIST_DIR"
find "$RS_BOARD_DIST_DIR" -mindepth 1 -maxdepth 1 -exec rm -rf {} +

# 调用构建脚本，按发布版本号编译 .app
"$RS_BOARD_SCRIPT_DIR/build-macos-app.sh" "$RS_BOARD_VERSION"

# 准备 DMG 内容目录与成品目录
mkdir -p "$RS_BOARD_DMG_ROOT" "$RS_BOARD_DIST_DIR"
# ditto 复制 .app 进 DMG 根目录（保留结构/权限）
ditto "$RS_BOARD_STAGE_APP" "$RS_BOARD_DMG_ROOT/RS Board.app"
# 创建指向 /Applications 的符号链接，方便用户拖拽安装
ln -s /Applications "$RS_BOARD_DMG_ROOT/Applications"
# 把说明文档和第三方许可拷进 DMG
cp "$RS_BOARD_ROOT/distribution/README.txt" "$RS_BOARD_DMG_ROOT/README.txt"
cp "$RS_BOARD_ROOT/crates/app/assets/THIRD_PARTY_NOTICES.txt" \
  "$RS_BOARD_DMG_ROOT/THIRD_PARTY_NOTICES.txt"

# 用 hdiutil 把 dmg-root 打成压缩 UDZO 格式的 DMG：
#   -volname 挂载后显示的卷名
#   -srcfolder 要打包的目录
#   -format UDZO 压缩镜像
hdiutil create \
  -volname "RS Board" \
  -srcfolder "$RS_BOARD_DMG_ROOT" \
  -format UDZO \
  "$RS_BOARD_DMG"

# 在 dist/ 目录内为 DMG 生成 sha256 校验和文件（文件名 = DMG名.sha256）
(
  cd "$RS_BOARD_DIST_DIR"
  shasum -a 256 "$RS_BOARD_DMG_NAME" >"$RS_BOARD_DMG_NAME.sha256"
)

# 调用验证脚本校验 DMG 内容/签名
"$RS_BOARD_SCRIPT_DIR/verify-macos-package.sh" "$RS_BOARD_DMG"
# 标记打包完整完成：此后 cleanup 不再删产物、不再回滚版本
RS_BOARD_PACKAGE_COMPLETE=1

# 输出成品路径与校验和路径
echo "packaged: $RS_BOARD_DMG"
echo "checksum: $RS_BOARD_CHECKSUM"
