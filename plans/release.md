# RS Board macOS ad-hoc 发布方案

## 1. 目标与边界

RS Board 首发只面向 Apple Silicon，最低支持 macOS 13。构建结果是一个经过 ad-hoc 签名的 `RS Board.app`，再封装为 DMG，供自己和少量可信朋友安装。

这条方案：

- 不需要 Apple Developer Program。
- 不需要 Developer ID 证书。
- 不启用 Apple 公证。
- 不保证 Gatekeeper 直接放行。
- 朋友首次打开时，需要在 macOS“隐私与安全性”中手动选择“仍要打开”。
- 首发不做 Intel/Universal Binary、Mac App Store、PKG 和自动更新。

ad-hoc 签名只保证 app bundle 内的代码和资源在签名后没有变化，不证明开发者身份。DMG 应通过可信的私聊、网盘或 GitHub Release 发送，并同时提供 SHA-256；它适合熟人间的小规模分发，不适合公开推广。

## 2. 当前基础与待补内容

仓库已经具备：

- Rust workspace 和可运行的 MVP。
- `RS Board` bundle 名称。
- `com.linjiajian.rs-board` Bundle ID。
- macOS 13 最低系统声明。
- `.rsboard` 文档类型与 UTI 声明。
- Apple Silicon Rust target。
- `codesign`、`hdiutil`、`iconutil`、`plutil` 等 macOS 系统工具。

本轮 A0 已补齐：

- 正式应用图标。
- 菜单栏应用的 plist 配置。
- 固定版本的 `cargo-bundle`。
- 构建、DMG 打包和验证脚本。
- 给朋友看的安装与手动放行说明。
- 内置字体和第三方依赖的许可说明。

## 3. 需要创建或修改的文件

目标目录结构：

```text
crates/app/assets/
  AppIcon.svg                             # 可重现的矢量图标源文件
  AppIcon-1024.png                        # 生成出的 1024px 检查图
  AppIcon.icns
  NotoSansSC-Regular.otf                 # 已存在
  macos-info-plist-ext.xml               # 已存在，需要修改
  THIRD_PARTY_NOTICES.txt                # 新增

distribution/
  about.toml                             # cargo-about 许可策略
  about.hbs                              # Rust 依赖 notice 模板
  NotoSansSC-OFL.txt                     # Noto 字体原始许可文本
  README.txt                             # 新增，朋友安装说明

scripts/
  generate-macos-icon.sh                 # 新增，生成 PNG 和 icns
  generate-third-party-notices.sh        # 新增，生成并检查依赖许可
  build-macos-app.sh                     # 新增，构建并签名 .app
  package-macos-dmg.sh                   # 新增，一键生成最终 DMG
  verify-macos-package.sh                # 新增，独立验证最终产物

dist/                                    # 构建产物，不提交 Git
.release-tmp/                            # 临时 staging，不提交 Git
```

### 3.1 `crates/app/assets/AppIcon.icns`

`generate-macos-icon.sh` 以提交到仓库的 `AppIcon.svg` 为唯一图形源，调用固定版本 `cargo-bundle` 内置的 `resvg` 和 `icns` 实现生成标准 1x/2x 表示。脚本再用 `iconutil` 展开 ICNS、取出 1024x1024 的 `AppIcon-1024.png`，并用 `sips` 校验尺寸。中间 bundle 和 `.iconset` 都放在临时目录，不提交 Git；连续两次生成的 PNG 和 ICNS 哈希必须一致。

至少检查以下尺寸清晰且没有意外透明边缘：

```text
16x16
32x32
128x128
256x256
512x512
1024x1024
```

菜单栏单色 template icon 继续使用 `tray.rs` 中的现有实现；它与 Finder、Launchpad 显示的应用图标不是同一个资源。

### 3.2 `crates/app/Cargo.toml`

在现有 `[package.metadata.bundle]` 中增加：

```toml
icon = ["assets/AppIcon.icns"]
```

保留以下现有配置：

```toml
name = "RS Board"
identifier = "com.linjiajian.rs-board"
category = "public.app-category.productivity"
osx_minimum_system_version = "13.0"
osx_info_plist_exts = ["assets/macos-info-plist-ext.xml"]
```

Cargo workspace version 是产品版本的唯一来源：

```toml
[workspace.package]
version = "0.1.0"
```

版本使用三段纯数字，如 `0.1.0`、`0.1.1`，产物名与之保持一致。`package-macos-dmg.sh` 默认直接使用 workspace 当前版本，并且不修改 `Cargo.toml` 或 `Cargo.lock`。传入 `--update` 时才把最后一段加一、同步 `Cargo.lock` 并把新版本传给内部构建脚本；成功时保留版本更新，失败时恢复原文件。

### 3.3 `crates/app/assets/macos-info-plist-ext.xml`

在现有 `.rsboard` 文档类型声明之外增加：

```xml
<key>LSUIElement</key>
<true/>
```

这样应用从启动开始就按菜单栏应用运行，减少 Dock 图标短暂出现的可能。保留现有 `CFBundleDocumentTypes` 和 `UTExportedTypeDeclarations` 内容。

本方案不增加 App Sandbox、Developer ID entitlement 或公证相关配置。屏幕录制继续由 macOS TCC 权限控制。

### 3.4 `crates/app/assets/THIRD_PARTY_NOTICES.txt`

至少记录：

- `NotoSansSC-Regular.otf` 的准确来源、版本和文件哈希。
- 字体的原始许可文本；如果确认是 SIL Open Font License，应附带对应 OFL 文本。
- 分发二进制所需的第三方 Rust 依赖许可声明。

该文件由固定版本的 `cargo-about` 和仓库内模板生成：

```bash
./scripts/generate-third-party-notices.sh
```

生成器只分析 `Cargo.lock` 中 `aarch64-apple-darwin` 的 normal dependency graph，并附加
`epaint_default_fonts` 实际随 crate 分发的 Hack、OFL、Ubuntu Font Licence
和 emoji-icon-font MIT notice。生成器会验证 Noto 字体与这些原始 notice 的
SHA-256；打包脚本使用 `--check` 拒绝过期的许可文件。

仓库 `LICENSE` 中的版权人和 `Cargo.toml` bundle copyright 也应在首发前统一。许可来源没有核对完成时，不向朋友分发字体随二进制打包的版本。

### 3.5 `distribution/README.txt`

该文件会放进 DMG，内容保持简短：

```text
RS Board 安装说明

要求：Apple Silicon Mac，macOS 13 或更高版本。

1. 把 RS Board.app 拖到 Applications。
2. 从 Applications 双击 RS Board。
3. 如果系统阻止打开，请前往：
   系统设置 -> 隐私与安全性 -> 安全性 -> 仍要打开。
4. 再次确认“打开”。RS Board 会出现在菜单栏，不常驻 Dock。
5. 首次截图时允许“屏幕录制”；较新系统可能显示为
   “屏幕与系统音频录制”。授权后退出并重新打开 RS Board。
6. 默认使用 F1 截图，部分键盘需要 Fn+F1。

此版本使用 ad-hoc 签名，没有经过 Apple 公证，只应安装来自作者本人
或双方确认过的可信链接的文件。
```

不指导朋友关闭 Gatekeeper，也不把 `xattr -d` 作为安装步骤。系统提供的“仍要打开”是本方案唯一支持的放行方式。

### 3.6 `.gitignore`

增加：

```gitignore
/dist/
/.release-tmp/
```

最终 DMG、临时 app 和挂载目录都不提交到 Git。版本通过 Git tag 和发布附件管理。

## 4. 一次性准备

### 4.1 安装并固定 `cargo-bundle`

当前机器已安装并验证兼容现有 workspace metadata 的固定版本。如需在新机器准备环境，执行：

```bash
cargo install cargo-bundle --version 0.11.0 --locked
```

`0.11.0` 已在当前 workspace metadata 和 Rust 工具链上验证通过。安装时不能省略版本号，也不能使用未固定版本的 `cargo install cargo-bundle`。

确认环境：

```bash
rustc -V
cargo -V
rustup target list --installed
cargo bundle --version
command -v codesign
command -v hdiutil
command -v iconutil
command -v sips
```

`rustup target list --installed` 必须包含：

```text
aarch64-apple-darwin
```

### 4.2 准备应用图标和许可

在开始编写打包脚本前完成：

1. 执行 `generate-macos-icon.sh`，生成并肉眼检查 `AppIcon.icns`。
2. 把图标路径加入 bundle metadata。
3. 核对字体来源和许可。
4. 统一 MIT 版权署名。
5. 准备 `distribution/README.txt`。

依赖许可使用启用了 CLI 的固定 `cargo-about` 版本生成：

```bash
cargo install cargo-about --version 0.9.1 --locked --features cli
./scripts/generate-third-party-notices.sh
./scripts/generate-third-party-notices.sh --check
```

生成和检查均使用 `--locked --offline --target aarch64-apple-darwin`，不会把
development 或 build edge 写入分发 notice；Cargo 标为 normal edge 的 proc-macro
仍会保留。

## 5. `build-macos-app.sh` 的职责

脚本输出一个已完成 ad-hoc 签名的 app，不生成 DMG。它接受版本参数：

```bash
./scripts/build-macos-app.sh 0.1.0
```

执行顺序如下。

### 5.1 前置检查

1. 必须运行在 Apple Silicon macOS。
2. 检查 `cargo`、`cargo-bundle`、`codesign`、`plutil`、`file` 和 `otool`。
3. 参数必须是三段数字版本。
4. 参数版本必须等于 workspace version。
5. 检查 `Cargo.lock`、图标源文件、`AppIcon.icns`、plist 扩展和许可文件存在。
6. 目标 `.release-tmp/<version>/RS Board.app` 不得已经存在，避免混入旧文件。

正式交付前还应保证 Git worktree 干净。调试脚本期间可以使用未提交修改，但最终给朋友的构建必须对应一个明确 commit 和 `v<version>` tag。

### 5.2 质量检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

任一命令失败立即停止，不生成可分发产物。

### 5.3 构建 app bundle

`Cargo.toml` 中声明最低 macOS 版本还不够，编译时也要显式设置 deployment target：

```bash
(
  cd crates/app
  MACOSX_DEPLOYMENT_TARGET=13.0 \
    cargo bundle --release --target aarch64-apple-darwin --format osx
)
```

必须从 `crates/app` 运行，使 bundle metadata 中的 `assets/...` 路径相对 app crate 正确解析。必须显式指定 `--format osx`；否则当前 `cargo-bundle` 在 macOS 上还会尝试生成它自己的 DMG，绕过本方案的 staging、签名和独立验证步骤。

预期 cargo-bundle 输出：

```text
target/aarch64-apple-darwin/release/bundle/osx/RS Board.app
```

脚本把它完整复制到：

```text
.release-tmp/<version>/RS Board.app
```

后续签名和打包只操作 staging 副本，不修改 `target/` 中的原始构建结果。

`cargo-bundle 0.11.0` 会把 `CFBundleVersion` 生成为构建时间戳。脚本在 staging 副本中将它规范化为产品版本，再执行 plist 校验和签名；`target/` 中的原始 bundle 保持不变。

### 5.4 签名前验证

脚本验证：

```text
Contents/MacOS/app                       存在且可执行
Contents/Resources/AppIcon.icns          是非空普通文件且哈希正确
Contents/Info.plist                      可被 plutil 解析
CFBundleIdentifier                       com.linjiajian.rs-board
CFBundleShortVersionString               等于参数版本
CFBundleVersion                          等于参数版本
LSMinimumSystemVersion                   13.0
LSUIElement                              true
CFBundleDocumentTypes / UTI              包含 rsboard 声明
```

使用 `file` 或 `lipo -info` 确认主程序是 `arm64`。使用 `otool -L` 确认没有引用 `.release-tmp`、Cargo target 或本机构建工具目录中的动态库。同时从 `otool -l` 的 `LC_BUILD_VERSION` 读取 `minos`，确认编译产物而不只是 plist 声明的最低系统版本为 `13.0`。

### 5.5 ad-hoc 签名

当前 bundle 预计只有一个主 Mach-O，因此直接签名整个 app：

```bash
codesign --force --sign - ".release-tmp/<version>/RS Board.app"
```

本方案不使用：

```text
Developer ID identity
--timestamp
notarytool
stapler
公证凭据
```

如果以后加入 framework、dylib、helper app 或 XPC service，必须先对内层可执行代码逐个 ad-hoc 签名，再签最外层 app，不能把 `--deep` 当成签名流程。

签名后验证：

```bash
codesign --verify --deep --strict --verbose=2 ".release-tmp/<version>/RS Board.app"
codesign --display --verbose=4 ".release-tmp/<version>/RS Board.app"
```

输出应显示 ad-hoc 签名，不能出现意外 entitlement。由于没有 Developer ID 和公证，`spctl --assess` 拒绝这个 app 是预期结果，不能把 Gatekeeper 通过作为脚本成功条件。

## 6. `package-macos-dmg.sh` 的职责

这是日常发包使用的唯一入口：

```bash
./scripts/package-macos-dmg.sh
```

它读取当前 workspace version，检查 staging 不存在且 `THIRD_PARTY_NOTICES.txt` 与当前依赖一致，然后清空 `dist/` 并调用 `build-macos-app.sh`。默认不会修改版本文件；使用 `--update` 时会先备份并升级 `Cargo.toml` 和 `Cargo.lock`，任一步失败都会恢复原版本文件。成功后创建：

```text
.release-tmp/0.1.1/dmg-root/
  RS Board.app
  Applications -> /Applications
  README.txt
  THIRD_PARTY_NOTICES.txt
```

然后生成：

```bash
hdiutil create \
  -volname "RS Board" \
  -srcfolder ".release-tmp/0.1.1/dmg-root" \
  -format UDZO \
  -ov \
  "dist/RS-Board-0.1.1-macos-arm64.dmg"
```

每次构建前，脚本会删除 `dist/` 下包括隐藏文件和子目录在内的全部旧内容。生成后计算摘要：

```bash
shasum -a 256 "dist/RS-Board-0.1.1-macos-arm64.dmg"
```

写入：

```text
dist/RS-Board-0.1.1-macos-arm64.dmg.sha256
```

需要升级 patch 版本并更新版本文件时使用：

```bash
./scripts/package-macos-dmg.sh --update
```

无参数模式下，app、DMG 文件名及 bundle plist 都使用 workspace 当前版本。

摘要文件只记录 DMG 文件名，不记录构建机绝对路径。

## 7. `verify-macos-package.sh` 的职责

独立验证脚本只接受最终 DMG，不能依赖 Cargo target 或构建 staging：

```bash
./scripts/verify-macos-package.sh \
  dist/RS-Board-0.1.0-macos-arm64.dmg
```

它执行：

1. 重新计算 SHA-256，并与 `.sha256` 文件比对。
2. 以只读方式挂载 DMG。
3. 确认根目录只有 app、Applications 链接、README 和许可说明。
4. 验证 Applications 链接确实指向 `/Applications`。
5. 对挂载后的 app 运行 `codesign --verify --deep --strict`。
6. 确认签名类型为 ad-hoc，不存在未知签名身份或 entitlement。
7. 验证 Bundle ID、版本、最低系统、`LSUIElement` 和 `.rsboard` UTI。
8. 确认主程序只有 `arm64`、Mach-O `minos` 为 `13.0`，且没有异常动态库路径。
9. 卸载 DMG。

挂载点使用系统临时目录创建，并通过退出 trap 清理。验证脚本会按镜像绝对路径清理同一路径的残留设备；如果 `hdiutil` 已附加设备但未完成挂载，会卸载残留设备并最多重试三次。任何断言失败都返回非零状态，失败的 DMG 不发送给朋友。

## 8. 自己安装与冒烟测试

### 8.1 本机构建测试

本机生成的文件通常没有互联网下载产生的 quarantine 标记。完成打包后：

1. 打开 DMG。
2. 把 `RS Board.app` 拖入 `/Applications`。
3. 从 Applications 启动。
4. 确认菜单栏出现图标，Dock 不常驻。
5. 允许屏幕录制权限并重启应用。
6. 完成一次截图、标注、暂存、恢复和保存。
7. 双击 `.rsboard`，确认能打开或转交给已有实例。
8. 开启登录时启动，注销并重新登录，确认只启动一个实例；测试后再关闭。

不要直接从 DMG 内长期运行。登录项和稳定权限测试都应在 `/Applications` 中进行。

### 8.2 模拟朋友收到文件

只在发布机本地打开不能验证朋友的首次放行体验。候选 DMG 应通过实际准备采用的渠道上传，再由一个干净 macOS 用户或另一台 Mac 下载。

下载后验证：

- DMG 可以打开并拖动安装。
- 首次双击 app 时 Gatekeeper 会阻止运行，这是预期行为。
- “系统设置 -> 隐私与安全性 -> 仍要打开”可以成功放行。
- 放行后应用能够启动，屏幕录制权限可以授予。
- 授权并重启后，完整截图工作流可用。
- 不需要关闭 Gatekeeper，也不需要执行 `xattr -d`。

如果系统没有提供“仍要打开”，或者提示文件损坏且无法通过系统界面放行，应停止分发并检查 DMG、签名和传输过程，不能让朋友绕过更多系统安全设置。

## 9. 给朋友的实际安装步骤

朋友收到两个文件：

```text
RS-Board-<version>-macos-arm64.dmg
RS-Board-<version>-macos-arm64.dmg.sha256
```

安装流程：

1. 确认 Mac 是 Apple Silicon，系统为 macOS 13 或更高版本。
2. 打开 DMG，把 `RS Board.app` 拖到 Applications。
3. 从 Applications 双击 RS Board。
4. macOS 阻止打开后，打开“系统设置 -> 隐私与安全性”。
5. 在安全性区域找到 RS Board，点击“仍要打开”，按系统要求认证。
6. 再次确认“打开”。应用启动后位于菜单栏。
7. 按 `F1` 触发截图，并允许屏幕录制权限。
8. 退出 RS Board 后重新启动，再次按 `F1` 验证截图。

不同 macOS 版本的提示文字可能略有差异。较新系统可能把权限显示为“屏幕与系统音频录制”。

SHA-256 只能验证下载文件是否与发送者提供的文件一致，不能替代 Developer ID 身份认证。摘要应通过与 DMG 相同或另一个双方可信的聊天渠道发送。

## 10. 更新版本的步骤

每次给朋友发新版本：

1. 完成代码、变更说明和已知问题。
2. 依赖或图标变化时更新生成资源。
3. 运行 `package-macos-dmg.sh`，按当前版本完成全部自动检查；需要发布新 patch 版本时使用 `--update`。
4. 检查版本变更，提交代码并创建对应的 `v<version>` tag。
5. 运行 `verify-macos-package.sh`。
6. 从实际分发渠道下载一次并完成手动放行冒烟测试。
7. 发送 DMG、SHA-256、安装说明和已知问题。

同一版本的 DMG 一经发送不得替换。修复后递增 patch 版本，例如从 `0.1.0` 更新为 `0.1.1`。

朋友升级时：

1. 退出正在运行的 RS Board。
2. 打开新 DMG。
3. 用新的 `RS Board.app` 替换 `/Applications` 中的旧版本。
4. 再次启动并按系统提示放行。
5. 如果屏幕录制失效，在系统设置中重新授权并重启应用。

ad-hoc 签名会随二进制内容变化。即使 Bundle ID 不变，新版本仍可能再次触发 Gatekeeper 或屏幕录制授权，这是这条零付费分发路线需要接受的成本。

升级只替换 app，不删除 Application Support 中的讲义、草稿和设置。若未来提高 `.rsboard` schema，发布前必须增加迁移测试，并明确新版数据是否还能被旧版打开。

## 11. 完成定义

### A0：构建能力

- [x] 增加图标源文件、生成脚本、`AppIcon.icns` 和 bundle icon 配置。
- [x] 增加 `LSUIElement=true`。
- [x] 核对字体和依赖许可。
- [x] 固定并安装 `cargo-bundle`。
- [x] 实现图标生成、构建、打包和验证脚本。
- [x] 将 `dist/` 和 `.release-tmp/` 加入 `.gitignore`。

完成标准：一个命令可以生成版本化 DMG 和 SHA-256 文件，app 是 arm64、macOS 13、有效 ad-hoc 签名。

### A1：本机安装

- [ ] 从 DMG 安装到 `/Applications`。
- [ ] 菜单栏、屏幕录制、全局快捷键可用。
- [ ] 截图、标注、暂存、恢复、保存通过。
- [ ] `.rsboard` 文件关联和单实例通过。
- [ ] 登录时启动通过。

完成标准：本机从安装后的 app 完成完整工作流，没有依赖 Cargo 或终端启动。

### A2：朋友分发

- [ ] 从真实分发渠道下载候选 DMG。
- [ ] 在干净用户或另一台 Mac 上通过“仍要打开”放行。
- [ ] 完成屏幕录制授权和应用重启。
- [ ] 在非开发机上完成一次完整工作流。
- [ ] 安装过程不要求关闭 Gatekeeper 或运行 `xattr -d`。

完成标准：至少一位朋友可以仅根据 `README.txt` 完成安装、手动放行、权限授权和截图保存流程。

## 12. 明确不做

- Developer ID 签名。
- Apple 公证和 stapled ticket。
- Gatekeeper 无提示安装。
- Mac App Store 和 App Sandbox。
- Intel 或 Universal Binary。
- PKG 安装器。
- 自动更新。
- 签名证书、公证凭据或发布 CI。
- 引导用户关闭 Gatekeeper 或删除 quarantine 属性。

当朋友数量扩大、更新频率提高，或者手动放行成为明显负担时，再单独评估 Apple Developer Program。当前阶段不为这个小工具产生开发者账号费用。
