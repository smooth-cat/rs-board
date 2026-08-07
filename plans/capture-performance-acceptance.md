# 截图性能优化验收记录

日期：2026-08-08

## 自动化正确性门槛

阶段 6.7 将以下计划要求固化为自动测试，提交前统一执行：

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build --workspace --release
git diff --check
```

本次结果：137 个 workspace 测试全部通过，严格 clippy、格式检查、差异检查和 release 构建均通过。

覆盖关系：

| 计划要求 | 自动化证据 |
| --- | --- |
| 编码失败后正式保存可重试 | `background_encode::tests::failed_current_capture_can_be_retried_with_the_same_sequence` |
| 两个活动编码槽且只保留一个最新等待项 | `background_encode::tests::limits_active_encodes_and_keeps_only_latest_pending_request` |
| 原生/编码缓冲最终释放 | `background_encode::tests::prepared_background_releases_pixels_after_the_last_consumer` |
| 旧提交成功、当前请求失败仍保留健康草稿 | `draft_coordinator::tests::stale_success_followed_by_current_failure_preserves_the_healthy_draft` |
| latest-wins 与 generation 条件删除 | `draft_coordinator::tests::latest_job_wins_and_old_generation_delete_cannot_remove_it` |
| 退出等待有 2 秒上限 | `draft_coordinator::tests::shutdown_wait_is_bounded_and_abandons_a_job_still_waiting_for_encoding` |
| 背景、manifest、slot、marker、文件/目录同步和原子替换故障 | `storage::store::tests::draft_commit_faults_never_destroy_the_last_recoverable_generation` |
| 保存后预览合并、最近讲义合并、剪贴板顺序和队列上界 | `post_save::tests::*` |
| 被淘汰保存后任务释放快照 | `post_save::tests::superseded_render_releases_its_snapshot_reference` |
| 多屏负坐标、显示器变化和活动显示器移除 | `capture_surface::tests::*` |
| 睡眠或长时间停顿后的冷刷新 | `capture_surface::tests::long_wall_clock_gap_forces_a_cold_refresh_without_replacing_active_snapshot` |
| 真实 `3840 x 2160` 和 `7680 x 4320` 压平路径 | `renderer::tests::raster_renderer_accepts_4k_and_8k_canvases` |
| `.rsboard`、导入导出、草稿和中断提交恢复兼容 | `storage::store::tests::*` 与 `common::format::tests::*` |
| 每个计划指标的阈值和 p95/max 口径 | `performance::tests::summary_applies_every_planned_hot_path_limit` |
| 不完整或超限样本使验收失败 | `performance::tests::verifier_fails_incomplete_or_over_limit_measurements` |

测试故障点只在 `cfg(test)` 构建存在，release 二进制没有运行时故障注入入口。

## 实机指标门槛

性能数据必须来自 release 构建、原生 4K/8K 像素和 `solid`、`ui`、`photo` 三类固定语料。每个热路径 run 先预热 5 次再采样 100 次；冷路径的 `startup`、`wake`、`display_change` 每类使用至少 10 个独立进程，每个进程只采一个冷样本。

每个日志文件使用以下命令验收；样本缺失、日志不完整、存在 dropped event 或任一指标超限都会返回非零：

```bash
scripts/verify-capture-performance.sh perf-logs/<corpus>-<run-kind>.jsonl
```

阈值由脚本按阶段、workflow 和原生分辨率选择：

| 指标 | 统计量 | 上限 |
| --- | --- | --- |
| 4K 热截图首帧 | p95 | 50ms |
| 4K 冷截图首帧 | max | 500ms |
| 4K/8K `Esc` 隐藏 | p95 | 50ms |
| 4K 正式保存并隐藏 | p95 | 1s |
| 8K 后台暂存完成 | p95 | 6s |
| 8K 正式保存并隐藏 | p95 | 6s |

## 仍需参考机执行

当前自动化会话运行在 16GB M1 Pro、macOS 15.7.3，但没有可访问的显示器或屏幕录制会话，因此下列结论不能由本次运行替代：

- macOS 13 CoreGraphics 回退和 macOS 14+ `SCScreenshotManager` 的真实权限、超时与画面正确性。
- 多显示器不同缩放、负坐标、拔插、分辨率变化、睡眠唤醒和焦点恢复。
- 三类语料的 4K/8K p95 与冷路径 max。
- 8GB Apple Silicon 上连续 30 轮 8K 后的峰值 RSS、空闲 10 秒回落，以及 Instruments 的持续增长检查。

这些项目必须在目标硬件上执行并保留 JSONL、RSS 记录和 Instruments 截图。没有这些工件时，只能认定实现与自动化回归完成，不能声称实机指标已经通过。
