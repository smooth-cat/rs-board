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

## 性能基线

性能埋点默认关闭。使用 release 构建采样时，通过环境变量把非阻塞 JSONL 日志写入指定文件，并为本轮固定语料和路径分类：

```bash
mkdir -p perf-logs
RS_BOARD_PERF_LOG=perf-logs/ui-hot.jsonl \
RS_BOARD_PERF_CORPUS=ui \
RS_BOARD_PERF_RUN_KIND=hot \
cargo run -p app --release -- --show
```

日志文件使用 append 模式；每轮热路径实验应使用一个新的文件，并在同一进程连续执行至少 105 次，等待后台任务结束后正常退出应用再汇总。`RS_BOARD_PERF_CORPUS` 必须是 `solid`、`ui`、`photo`，`RS_BOARD_PERF_RUN_KIND` 必须是 `hot` 或 `cold`。汇总器会按 `run_id` 分别剔除每个热路径 run 的前 5 次，并要求剩余至少 100 次，多个不完整 run 不会被拼成完整样本。

冷路径还必须设置 `RS_BOARD_PERF_COLD_SOURCE=startup|wake|display_change`，三种来源和三类语料分别至少采样 10 次。每个进程/run 只执行一次对应的启动、唤醒或显示器变化，再采集一次首次 `F1`；完成后正常退出并用新进程进行下一次，多个 run 可追加到同一个、只用于该实验的日志文件。汇总器要求至少 10 个独立 run 且每个 run 只有一个同组冷样本。4K 冷路径以 `max_us` 和 `within_limit` 检查 500ms 单次上限，不以 p95 代替。汇总单个阶段或全部阶段：

```bash
scripts/summarize-capture-performance.sh perf-logs/ui-hot.jsonl \
  capture.editor_frame_submitted
scripts/summarize-capture-performance.sh perf-logs/ui-hot.jsonl
scripts/verify-capture-performance.sh perf-logs/ui-hot.jsonl
```

汇总器拒绝 debug、非法或缺失标签、任何非成功测量、缺少唯一且位于末尾的 clean `run_complete`，以及包含 dropped event 的数据；只有 `complete=yes` 的行可作为基线。`within_limit` 对热路径比较 p95，对冷截图比较 max；验证器会在样本不完整、没有计划内指标或任一阈值失败时返回非零。确认验收行的 `resolution` 是原生 `3840x2160` 或 `7680x4320`；没有尺寸字段的内部阶段显示 `-`，不能单独用于跨分辨率验收。截图呈现看 `capture.editor_frame_submitted`，暂存/保存的当前 UI 完成边界看 `persistence.request_to_ui_complete` 并区分 `workflow`，8K 后台暂存完成看 `stash.request.total`，可靠存储内部耗时看 `persistence.store.total`。

`capture.editor_frame_submitted` 表示首个编辑器 UI pass 和窗口命令已提交，是当前 eframe 链路的代理终点，不等同于系统 compositor 已显示；原生双层窗口接入后再补充精确的合成完成事件。日志只包含关联 ID、尺寸、字节数、阶段、耗时和脱敏错误码，不记录错误文本、图像、文字、标题或完整路径。

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
