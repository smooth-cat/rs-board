# RS Board

RS Board 是一个面向 Apple Silicon macOS 13+ 的菜单栏截图标注工具。

## 开发

安装 Rust，并确认目标可用：

```bash
rustup target add aarch64-apple-darwin
```

从仓库根目录启动：

```bash
cargo dev
```

等价于 `cargo run -p app -- --show`，会直接显示标注主窗口。

首次截图时需要授予 macOS 屏幕录制权限，授权后重新启动应用。

## 检查

提交前运行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 打包

### 第 1 步: 安装发布工具（每台开发机仅需一次）

安装固定版本的打包与许可生成工具：

```bash
cargo install cargo-bundle --version 0.11.0 --locked
cargo install cargo-about --version 0.9.1 --locked --features cli
```

确认版本：

```bash
cargo-bundle --version
cargo-about --version
```

### 第 2 步: 更新生成资源（按需执行）

新增、删除或升级项目依赖后，更新第三方许可文件：

```bash
./scripts/generate-third-party-notices.sh
```

修改 `crates/app/assets/AppIcon.svg` 后，重新生成应用图标：

```bash
./scripts/generate-macos-icon.sh
```

仅修改 Rust 业务代码时，可以跳过本步骤。

### 第 3 步: 生成 DMG

默认使用 workspace 当前版本打包，不修改 `Cargo.toml` 和 `Cargo.lock`：

```bash
./scripts/package-macos-dmg.sh
```

脚本通过前置检查后会先删除 `dist/` 下的全部内容，随后执行全部检查、构建 arm64 app、进行 ad-hoc 签名，并生成 DMG 和 SHA-256。

需要把 workspace version 的最后一段加一（例如 `0.1.0 -> 0.1.1`）并同步 `Cargo.lock` 时，执行：

```bash
./scripts/package-macos-dmg.sh --update
```

更新模式打包失败时会恢复原来的 `Cargo.toml` 和 `Cargo.lock`；打包成功时保留新版本。

### 第 4 步: 独立验证产物

使用第 3 步输出的产物版本号验证，例如：

```bash
./scripts/verify-macos-package.sh \
  dist/RS-Board-0.1.1-macos-arm64.dmg
```

详细实现和发布约束见 `plans/mvp.md` 与 `plans/release.md`。
