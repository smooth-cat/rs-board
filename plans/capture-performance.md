# 截图捕获与持久化性能优化计划

## 1. 背景与范围

当前截图编辑流程功能已经贯通，但两段关键交互仍有明显等待：

- 按下 `F1` 后，冻结画面和标注工具不能立即出现，窗口还会从素材库的 `1120 x 760` 尺寸扩张到全屏。
- 新截图或恢复的草稿按 `Esc` 时，以及讲义按 `Cmd+S` 保存时，4K 背景的编码和落盘会造成数秒等待。

本计划只优化 macOS 截图呈现、截图背景的数据通路和持久化调度，不改变画板文档格式、标注坐标、撤销重做语义或正式讲义的可靠保存边界。最低支持 macOS 13，不为 macOS 12 及以下增加第三条截图后端，也不引入持续录屏。

## 2. 性能目标与测量口径

参考环境为 8GB Apple Silicon 和 release 构建。4K、8K 分别按截图结果的 `3840 x 2160`、`7680 x 4320` 原生像素定义，不按显示器营销型号或逻辑分辨率定义。基准画面固定覆盖纯色、UI/文字和高熵照片三类语料，每类单独达标，避免 PNG 内容复杂度掩盖退化。

所有交互指标使用单调时钟埋点。热路径和持久化路径预热 5 次后，每类语料连续采样至少 100 次，以 p95 判定。冷路径分别在应用启动、系统唤醒和显示器配置变化后采样至少 10 次，任一次不得超过上限。

| 场景 | 起点 | 终点 | 目标 |
| --- | --- | --- | --- |
| 4K 热路径截图 | 全局快捷键回调收到 `F1` | 正确显示器的冻结画面已合成，工具栏完成首帧且可命中 | p95 `<= 50ms` |
| 4K 冷路径截图 | 启动、唤醒或显示器变化后的首次 `F1` | 冻结画面和工具栏可交互 | 单次 `<= 500ms` |
| 4K/8K 暂存退出 | 编辑器接受到可执行退出的 `Esc` | 所有截图编辑窗口均已隐藏且不再接收输入 | p95 `<= 50ms` |
| 4K 正式保存 | 编辑器接受到 `Cmd+S` | 正式讲义完成可靠提交并隐藏编辑器 | p95 `<= 1s` |
| 8K 后台暂存 | 编辑器接受到可执行退出的 `Esc` | 最新草稿完成可靠提交 | p95 `<= 6s` |
| 8K 正式保存 | 编辑器接受到 `Cmd+S` | 正式讲义完成可靠提交并隐藏编辑器 | p95 `<= 6s` |

4K 正式保存指标包含等待仍为 `Pending` 的预编码，但只适用于编码调度器未饱和的正常负载。两个不可中断编码槽都被旧任务占用的压力场景只验收正确性、资源有界和最终完成，不单独承诺 `1s`。8K 后台暂存样本在没有更新截图淘汰该任务的条件下测量；被 latest-wins 规则淘汰的任务不计为耗时样本。文字编辑、拖拽等未提交操作仍由编辑器先消费 `Esc`；只有编辑器判定为退出请求时才开始计算暂存退出耗时。正式讲义的可执行 `Esc` 不进入后台草稿队列，其关闭和未保存提示沿用现有语义。

## 3. 当前实现与瓶颈

### 3.1 `F1` 捕获链路

当前实现的主要路径是：

```text
全局快捷键回调
  -> channel
  -> egui 更新周期轮询 GlobalF1Hotkey
  -> 准备鼠标所在显示器
  -> 临时创建 SCShareableContent / SCStream
  -> 启动 stream
  -> 等待第一张 complete frame
  -> 停止并清理 stream
  -> 逐像素 BGRA -> RGBA
  -> egui 线程创建 ColorImage 并上传完整背景纹理
  -> 把同一个 1120 x 760 主窗口切成无边框全屏窗口
  -> 显示工具栏
```

这条路径把以下工作都放在首个可见帧之前：

- 每次重新枚举 `SCShareableContent`、创建并启停 `SCStream`，且停止成功后才返回图像。
- 复制完整 Retina 帧并逐像素交换颜色通道。
- 在 egui 线程构造 `ColorImage`、上传 GPU 纹理和创建工作会话。
- 复用素材库窗口，通过多条 viewport command 调整大小、装饰、层级和全屏状态。

因此截图 API 的启动成本、CPU 像素转换、GPU 上传和窗口重配串行叠加。窗口先以素材库尺寸参与合成，也会产生肉眼可见的小窗放大和闪烁。

### 3.2 `Esc` 与 `Cmd+S` 持久化链路

当前 `Esc` 会进入 `Stashing`，等待 `replace_latest_draft` 完成后才隐藏窗口并释放会话；持久化失败则回到编辑器并提供重试。`Cmd+S` 同样等待正式保存，这是正确的可靠性边界，但完成处理仍包含不必要的同步工作。

主要成本包括：

- 新截图首次持久化时，完整 RGBA 背景才开始复制和 PNG 编码。
- `BackgroundData::EncodedPng` 在 `normalized_png` 中仍会先解码再重新编码，稳定的 PNG 不能直接复用。
- staging 目录内的背景、manifest、slot 和 commit marker 分别写入并 `sync_all`，随后还要同步目录和执行原子替换；可靠性步骤本身不能简单删除，但编码和重复转换放大了总耗时。
- 正式保存成功后，UI 完成处理会取得并复制完整背景、启动压平图和预览任务、同步刷新最近讲义，然后才隐藏窗口。
- 每次任务单独创建线程，应用阶段枚举把 `Stashing` 视为全局忙碌，导致暂存期间新的 `F1` 被拒绝。

## 4. 目标架构

### 4.1 单次截图后端

统一截图接口返回原生图像，不在截图关键路径生成 RGBA：

```text
NativeCaptureFrame
  request_id
  capture_sequence
  image: retained CGImage
  pixel_size
  display_id
  display_bounds_global
  scale_factor
  captured_at
```

- macOS 14 及以上使用 `SCScreenshotManager` 和目标显示器的 `SCContentFilter` 完成一次性截图。
- macOS 13 使用 `CGWindowListCreateImage` 兼容路径。
- 两条路径都保持当前的录屏权限检查、鼠标所在显示器选择、是否包含光标、RS Board 自身窗口排除和最大 8K 校验。
- 捕获 worker 只等待一张图片，不创建持续运行的 `SCStream`，也不等待额外的 stop 回调。
- 全局快捷键回调继续通过线程安全事件通道进入应用协调器，但必须立即唤醒事件循环；不依赖普通 egui 重绘频率才能处理 `CaptureRequested`。
- 接受 `F1` 时记录目标显示器快照、进程内单调递增的 `capture_sequence` 和原活动应用/窗口。`Capturing` 期间不接受重复 `F1`，因此不会用新的快捷键请求替换正在进行的捕获。
- 单次捕获使用 500ms 总期限且不自动重试。结果返回后用 `request_id`、`capture_sequence` 和 `display_id` 校验；显示器已经失效或前台不再等待该结果时丢弃结果，不显示旧帧。
- 权限拒绝、超时或 API 失败后清理 `Capturing`/`Presenting` 状态并保持普通窗口原状。已有 RS Board 窗口可见时通过现有非阻塞错误渠道报告；所有窗口均隐藏时只写结构化日志，不激活应用或延迟显示错误。

### 4.2 每显示器预热的双层窗口

素材库与截图编辑器不再复用同一个窗口。窗口协调器为每个可用显示器维护：

```text
DisplayCaptureSurface
  display_snapshot
  frozen_image_panel   # 原生冻结画面层
  editor_overlay       # 透明 egui 标注层
  lifecycle: Hidden | Presenting(request_id) | Editing(session_id)
```

- `frozen_image_panel` 是无边框、无动画、覆盖目标显示器的原生 panel。其 layer 直接以 `CGImage` 作为 `contents`，使用正确的 `contentsScale`，避免 BGRA -> RGBA 和 egui 背景纹理上传进入显示关键路径。
- `editor_overlay` 是尺寸相同、位于冻结层上方的透明 egui 窗口，只绘制工具栏、已有标注和交互中的临时标注。画布坐标仍使用原始像素尺寸，窗口缩放只影响显示变换。
- 两个预热窗口在空闲时都隐藏、关闭隐式动画并设为点击穿透；不得出现在 Dock、窗口切换器或截图结果中，也不得抢占 key window。
- 开始编辑时先设置原生 layer 内容，再按固定层级无动画显示冻结层和覆盖层；冻结层始终点击穿透，只有覆盖层取消点击穿透并取得输入焦点。
- 退出编辑时先让覆盖层停止接收输入，再同时隐藏两层；清除 layer 内容和释放 GPU/原生图像可以在隐藏后执行。
- 应用启动、系统唤醒、屏幕增删、分辨率/缩放或全局坐标变化时重建受影响的空闲 `DisplayCaptureSurface`。目标显示器仍存在且正在编辑时，保留捕获时的显示快照和坐标变换到会话退出后再重建；目标显示器被移除时按有效 `Esc` 的语义立即隐藏并后台暂存。重建期间的第一次截图按冷路径指标验收。
- 素材库、设置和错误界面保留独立普通窗口，其尺寸和显示状态不会再影响截图覆盖层。普通窗口在截图编辑期间保留在目标覆盖层后方，其他显示器上的普通窗口继续可见，且它们仍不得进入截图结果。
- `Esc`、`Cmd+S` 成功或捕获失败后尽力恢复 `F1` 前的活动应用和窗口焦点；原对象已经失效时不强制激活其他窗口。

热路径顺序固定为：

```text
F1
  -> 从缓存选择鼠标所在显示器
  -> 单次捕获得到 CGImage
  -> 发布新的 capture_sequence 并淘汰更旧的后台准备工作
  -> 主线程设置 frozen_image_panel.layer.contents
  -> 显示冻结层和透明 editor_overlay
  -> 工具栏可交互
  -> 后台准备持久化数据
```

### 4.3 后台预编码

每张成功截图获得 `CGImage` 后都提交后台 PNG 预编码请求，但编码调度必须有界。窗口层和编码任务分别 retain 同一张原生图像；待编码请求被淘汰时立即释放其引用，已经开始的编码则在原生调用返回后释放，即使结果已经过期。系统只共享仍被当前会话或持久化任务引用的不可变结果：

```text
PreparedBackground
  capture_sequence
  pixel_size
  encoded_png: Pending | Ready(shared bytes) | Failed(error)

BackgroundEncodeScheduler
  active: 0..2 non-cancellable encodes
  pending_latest: optional request
```

- 使用原生 `CGImage`/`CGImageDestination` 或等价的无额外 RGBA 往返路径生成 PNG；编码、尺寸校验和内存分配不得阻塞 UI 线程。
- `CGImageDestinationFinalize` 等已经开始的原生编码调用不强制中断。调度器最多允许两个此类调用同时收尾；有空闲槽时立即编码最新截图，两个槽都占用时只保留一个 `capture_sequence` 最大的待编码请求，更旧的待编码请求及引用立即释放。
- 新帧成功捕获且通过可呈现校验后，才发布新的 `capture_sequence` 并淘汰旧草稿仍在等待的编码或提交请求；捕获失败不淘汰旧任务。已开始的旧编码允许返回，但结果直接丢弃；已经进入原子提交段的旧草稿允许完成，其结果不得改变新会话的前台状态。
- `Esc` 或 `Cmd+S` 生成的持久化快照引用同一个预编码结果。编码尚未结束时由后台持久化任务等待；`Esc` 的 UI 隐藏不等待，`Cmd+S` 则继续显示“保存中”直到可靠提交完成。
- `PreparedBackground::Failed` 用于区分编码失败和存储失败。草稿编码失败按最新暂存失败规则丢弃；正式保存编码失败时保留编辑器和持久错误提示，用户重试会创建新的编码请求。
- 从磁盘恢复的健康 PNG 通过 `CGImageSource` 创建冻结层需要的原生图像；原 PNG 在尺寸和限制校验通过后直接作为规范化背景复用，不再执行“解码 -> RGBA -> 重新编码”。源背景丢失时，仅当编辑会话仍持有有效 `CGImage` 或 RGBA 内存副本才允许重建；没有内存来源时判定文档损坏并阻止保存。格式不满足存储约束或后续压平渲染确实需要像素时才解码为 RGBA。
- 原生冻结图、预编码字节、标注文档和编辑资源分开持有。编辑窗口隐藏后立即释放窗口侧资源；持久化任务只保留完成当前提交所需的不可变快照。

### 4.4 `Esc`：立即隐藏与 latest-wins 暂存

新截图和恢复草稿在收到有效 `Esc` 后执行：

```text
冻结当前 revision，取得 capture_sequence，分配 sequence 和 generation_id
  -> 编辑覆盖层停止接收输入
  -> 隐藏冻结层与覆盖层
  -> 释放 WorkingSession 的编辑资源
  -> 把不可变 StashJob 交给后台队列
  -> 应用回到可接受 F1 的空闲状态
```

`Esc` 不等待 PNG 编码、文件写入、`sync_all` 或目录替换。暂存改为独立于前台编辑阶段的单 worker 串行队列：

```text
StashJob
  capture_sequence: 前台截图会话的进程内单调序号
  sequence: 进程内单调递增序号
  generation_id: 唯一持久化标识
  persistence_context
  snapshot
  prepared_background
```

队列遵循以下规则：

1. 应用协调器在接受新截图请求时预分配 `capture_sequence`，恢复截图会话准备完成时也分配同类序号；只有新会话已经具备可呈现背景时才把它发布为 `latest_presented_capture_sequence`，捕获失败不推进已发布序号。
2. 发布新的 `capture_sequence` 时，淘汰尚未进入原子提交段的旧截图暂存 job、等待中的旧背景编码和旧待编码请求。已开始的不可中断编码只允许收尾并丢弃结果，不产生失败提示。
3. 协调器同时记录 `latest_requested_sequence`。当前会话的 job 入队时，删除尚未开始且 sequence 更小的 job，只保留最新请求。UUID `generation_id` 继续写入草稿槽位，用于恢复草稿正式保存后的匹配删除，不参与先后排序。
4. 已经进入原子提交段的旧 job 不取消，允许它完成原子替换；worker 完成后必须继续处理当前仍有效的最新 job。旧提交可以短暂成为磁盘草稿，但不得改变更新会话的前台状态。
5. worker 严格串行调用存储层，禁止两个 job 并发替换 `draft/latest`。每个 job 仍写入独立 staging 目录，必需文件和 commit marker 全部完成并同步后才原子替换；失败只清理本次 staging，不覆盖或删除最后一份健康草稿。
6. 结果事件同时携带 `capture_sequence`、sequence 和 `generation_id`。任意成功提交都按实际磁盘状态更新最后健康草稿元数据和草稿可用状态；结果不属于最新前台会话或最新暂存请求时，除此之外不改变前台状态，也不显示提示。
7. 已被更新 `capture_sequence` 或 sequence 淘汰的失败只写诊断日志，不改变草稿可用状态，也不显示提示。当前最新请求失败时不重试、不恢复已经关闭的会话、不打开素材库或任何其他窗口。
8. 当前最新请求失败瞬间若至少一个 RS Board 窗口已经可见，则发送内容为“最新草稿暂存失败”的普通 Toast；若所有窗口均隐藏，则只写结构化错误日志，不缓存一个等待下次开窗显示的 Toast。
9. 当前最新请求成功后更新最后提交元数据；若之后又有更新会话或 sequence 入队，以新的结果为准。成功、失败或任务被淘汰后都释放 job、编码结果和会话残留资源；允许未保存截图和标注丢失，磁盘上最后一份健康草稿及其可恢复状态保持不变。
10. 草稿协调器还接受 `DeleteIfGeneration(generation_id)`。恢复草稿正式保存成功后，把条件删除作为命令交给同一 worker；worker 在与 `draft/latest` 替换相同的串行临界区内比较当前 generation，只有匹配时才删除。正式讲义提交成功后无需等待该清理命令即可隐藏编辑器。

暂存 worker 忙碌不再把应用整体标记为 `Stashing`。前台回到 `Idle` 后新的 `F1` 必须可用。应用退出时停止接受新的前台请求，最多等待当前最新草稿任务 2 秒；超时后放弃等待或排队中的工作，保留磁盘上最后一份健康草稿并记录结构化日志。退出过程不得重新打开已隐藏的编辑器，遗留 staging 由下次启动时的现有恢复清理流程处理。

### 4.5 `Cmd+S`：保留可靠保存屏障

`Cmd+S` 继续冻结当前 revision、禁止新的编辑命令并显示现有“保存中”状态。正式数据的背景、manifest 和必要提交标记完成同步及原子提交后，才算保存成功并隐藏编辑器。

- 新截图优先复用已经开始的 PNG 预编码；恢复草稿或正式讲义优先复用已经校验的 PNG 字节。正常负载下，即使预编码仍为 `Pending`，从接受 `Cmd+S` 起仍按 4K p95 `<= 1s` 验收。
- 正式提交失败时恢复原编辑会话和输入，保留现有持久错误提示及重试能力；编码失败后的重试重新启动编码，存储失败后的重试可以复用健康的已编码字节。这一语义不采用后台暂存的丢弃策略。
- 正式提交前创建只包含共享引用和提交后工作所需数据的不可变快照，不复制完整背景。提交成功后先隐藏窗口并释放编辑资源，再把最近讲义刷新、预览、压平图和按设置复制图片交给有界的保存后 worker。
- 同一讲义只保留最新 revision 的预览和压平任务；最近讲义刷新请求合并；复制图片按正式提交顺序执行。剪贴板更新不属于正式保存屏障，编辑器隐藏后的短暂时间内可能仍是旧内容。
- 预览、压平图、剪贴板或最近讲义刷新失败不回滚已提交讲义。结果到达时有 RS Board 窗口可见则通过现有非阻塞渠道显示普通 Toast；所有窗口均隐藏时只写结构化日志，不缓存延迟 Toast。
- 从恢复草稿进入的会话保存成功后，把 `DeleteIfGeneration` 交给草稿协调器异步处理；只能删除匹配的草稿 generation，不能删除期间由后台队列写入的更新草稿。
- `DeleteIfGeneration` 失败不回滚已提交讲义，也不删除最后健康草稿；错误使用与其他保存后任务相同的“可见窗口 Toast，否则仅日志”规则。

## 5. 状态与接口调整

前台会话状态和后台持久化状态必须拆开：

```text
ForegroundPhase = Idle | Capturing(request_id) | Editing(session_id) | Saving(request_id)
BackgroundEncodeState = active[0..2] + optional pending_latest(capture_sequence)
StashWorkerState = Idle | Processing(DraftCommand)
DraftCommand = Commit(StashJob) | DeleteIfGeneration(generation_id)
PostSaveWorkerState = Idle | Processing(PostSaveJob)
```

关键接口边界如下：

- 截图后端：输入 request、`capture_sequence`、目标显示器快照和捕获选项，在 500ms 总期限内异步返回 `NativeCaptureFrame` 或失败；同一时刻只接受一个捕获请求。
- 窗口协调器：按显示器管理 `DisplayCaptureSurface`，提供 `prepare`、`present`、`set_input_enabled`、`hide`、焦点恢复和显示器失效处理；不持有业务文档。
- 工作会话：背景由 `PreparedBackground` 表示，原生图像负责可见冻结层，编码结果负责存储；标注层不要求完整背景成为 egui texture。
- 编码调度器：接收带 `capture_sequence` 的背景请求，最多运行两个不可中断编码并合并为一个最新待处理请求；淘汰结果不得进入持久化层。
- 草稿调度器：接收 `DraftCommand`，维护最新 `capture_sequence` 和 sequence，在同一串行临界区内执行 `draft/latest` 替换及 generation 条件删除；提交结果同时返回 `capture_sequence`、sequence 和 `generation_id`。
- 保存后调度器：以固定 worker 和按输出键合并的有界队列处理预览、压平图、最近讲义刷新和复制图片；任务不持有编辑窗口资源。
- 草稿结果处理：先判断结果的 `capture_sequence` 和 sequence 是否仍为最新，再根据结果到达时是否已有可见 RS Board 窗口决定是否 Toast。日志始终记录 request、capture sequence、stash sequence、generation、document、revision、阶段、耗时和错误链，但不记录图像数据。

存储层继续负责 staging、校验、同步和原子替换，不负责窗口显示、Toast 或重试决策。应先通过耗时埋点确认各次 `sync_all` 的必要性；只有在不削弱崩溃恢复和提交完整性的前提下才合并同步点。

## 6. 实施顺序

1. **埋点与基线**：为快捷键、显示器准备、截图 API、首帧合成、PNG 编码、各文件写入/同步、原子替换和保存后任务增加统一 request、capture sequence、stash sequence 和 generation 关联的耗时日志，使用固定语料建立 4K/8K release 基线。
2. **窗口拆分和预热**：把素材库与编辑器窗口分开，实现每显示器冻结 panel 和透明覆盖层、无动画显隐、点击穿透、普通窗口层级、焦点恢复及显示器生命周期处理，先消除小窗放大。
3. **单次原生截图**：接入 macOS 14 `SCScreenshotManager` 和 macOS 13 回退，增加 500ms 总期限和单请求门控，将 `CGImage` 直接交给冻结层；移除热路径中的临时 stream、BGRA -> RGBA 和背景纹理上传。
4. **有界预编码与数据复用**：引入 `PreparedBackground` 和最多双活动编码、单最新等待项的调度器；让已编码 PNG 通过校验后直接写入，移除非必要的解码重编码，并实现内存背景重建和编码失败重试语义。
5. **后台草稿协调**：拆分前台/后台状态，落地 capture sequence 与 stash sequence 双重过滤、串行 latest-wins 提交、generation 条件删除、立即隐藏和“可见窗口 Toast，否则仅日志”的失败语义。
6. **保存收尾优化**：保持 `Cmd+S` 可靠提交屏障，使用不可变共享快照和有界合并队列，把压平、预览、剪贴板和最近讲义刷新移到隐藏窗口之后。
7. **回归与指标验收**：完成故障注入、编码槽饱和、多显示器、睡眠唤醒、4K/8K、退出期限、内存回落和性能采样；只有所有正确性测试通过后才用新链路替换旧实现。

每一阶段都保留旧链路作为开发期回退点，但正式交付不长期维护两套可配置实现。

## 7. 验证方案

### 7.1 截图与窗口

- 使用 `3840 x 2160` 和 `7680 x 4320` 捕获像素及固定的纯色、UI/文字、高熵照片语料，对热启动、首次冷启动、睡眠唤醒、显示器增删、分辨率和 Retina 缩放变化分别测量 `F1` 延迟。
- 多显示器不同缩放和负坐标布局下，冻结帧、窗口边界、鼠标目标显示器和标注坐标必须一致。
- 截图出现时不得看到素材库小窗放大、系统窗口动画、透明空白帧或上一张截图；素材库和设置窗口留在覆盖层后方，其他显示器上的普通窗口保持原有可见状态。
- RS Board 的素材库、设置、Toast 和预热窗口不得进入截图；隐藏状态不得截获鼠标或键盘。编辑器退出或捕获失败后尽力恢复原活动应用焦点，原窗口失效时不得意外激活其他窗口。
- 捕获期间连续按 `F1` 只产生一个请求。macOS 14+ 和 macOS 13 回退路径分别覆盖权限允许、权限拒绝、500ms 超时、显示器在捕获中移除和系统 API 失败，且都不自动重试。
- 捕获失败时，有可见 RS Board 窗口则使用现有错误渠道；所有窗口隐藏时不得激活应用、打开窗口或留下延迟提示，只记录日志。
- 编辑期间改变目标显示器的缩放、分辨率或全局坐标，确认当前捕获坐标保持到会话退出；移除目标显示器时确认窗口在 50ms 指标内隐藏并按 Esc 语义暂存。

### 7.2 暂存队列

- 连续执行 `F1 -> Esc -> F1`，新帧成功前旧草稿任务仍有效；新帧成功后淘汰旧编码和未进入原子提交段的旧 job，第二次截图不等待第一次 PNG 编码、写盘或同步完成。
- 让一个不可中断旧编码收尾时启动新编码，验证最多两个活动槽；占满两个槽后快速产生更多截图，验证 UI 仍立即呈现、只保留最新待编码项且更旧引用被释放。槽位饱和场景只验收正确性和有界资源，不验收 `Cmd+S <= 1s`。
- 快速产生多个 `capture_sequence` 和 stash sequence，验证尚未开始的旧 job 被淘汰，进入原子提交段的旧 job 可完成，但最新成功 job 最终是 `draft/latest`。
- 人为调整旧、新 job 完成时序，确认旧结果只可反映实际磁盘健康草稿，不能覆盖更新结果、改变新会话前台状态或显示过期提示。
- 在没有既有草稿时让运行中的旧 job 成功、最新 job 失败，确认旧 job 形成的健康草稿仍显示为可恢复。
- 在背景、manifest、marker、同步和原子替换各阶段注入错误，确认 staging 可清理且最后一份健康草稿始终可恢复。
- 最新 capture/stash sequence 失败且素材库或编辑器已经可见时，只显示“最新草稿暂存失败”普通 Toast，不显示模态框或持久错误条，也不提供重试。
- 最新 capture/stash sequence 失败且所有窗口隐藏时，不激活应用、不打开窗口、不留下延迟 Toast，仅记录日志；已淘汰结果同样不得提示或影响最新 job。
- 交错执行 `draft/latest` 替换和 `DeleteIfGeneration`，确认匹配 generation 才删除，期间写入的新 generation 永远不会被旧正式保存清理。
- 后台任务进行时退出应用，确认最多等待 2 秒，超时后保留最后健康草稿、记录日志且不重新打开编辑器；下次启动可清理不完整 staging。
- 最新失败或任务被淘汰后，确认待处理引用立即释放；不可中断编码调用返回后，原生图像、编码缓冲、标注快照和会话资源最终全部释放。

### 7.3 正式保存与兼容

- 正常编码负载下，在 PNG 为 `Ready` 和 `Pending` 时分别立即执行 `Cmd+S`，均从按键起计 4K p95 `<= 1s`；可靠提交前持续显示保存中状态，成功后隐藏，失败后保留可重试的编辑会话。
- PNG 已预编码、来自健康草稿、源背景丢失但存在内存副本三条保存路径生成相同尺寸和内容的背景；源文件和内存副本同时丢失时必须判定损坏并阻止保存。
- 注入预编码失败，确认 Esc 草稿按失败规则丢弃，`Cmd+S` 保留编辑器且重试会重新编码；注入存储失败时确认重试可复用健康编码结果。
- 快速保存同一讲义的多个 revision，确认预览和压平任务只保留最新 revision、最近讲义刷新合并、复制图片按提交顺序执行，且编辑器隐藏不等待剪贴板更新。
- 保存完成后的预览、压平、剪贴板和最近讲义刷新故障不得延迟隐藏或回滚正式讲义；有窗口时显示普通 Toast，所有窗口隐藏时仅记录日志且不延迟提示。
- 现有 `.rsboard`、`draft/latest`、启动恢复、导入导出、草稿 generation 匹配删除保持兼容。
- 每类固定语料分别验证 4K/8K 指标。在 8GB 参考机连续执行 30 轮 8K 截图、退出和保存，记录峰值常驻内存与临时内存；后台清空并空闲 10 秒后，RSS 不得高于预热基线加 `max(预热基线的 10%, 150 MiB)`，且 Instruments 不得显示持续增长、内存压力终止或分配失败。

## 8. 与 `plans/mvp.md` 的后续同步项

本计划落地后，`plans/mvp.md` 中以下描述会过时，本次只记录、不修改：

- `CaptureFrame.rgba_pixels`、背景上传为 egui texture，以及“捕获热路径不编码 PNG”。
- 新截图直到首次暂存或保存才编码背景的规则。
- `Esc -> Stashing -> 成功后关闭`、暂存期间拒绝 `F1`、失败回到编辑器并重试的状态机和验收标准。
- 素材库和全屏编辑器复用同一主窗口的隐含前提。
- 4K 快捷键到首帧 `<= 500ms`、暂存或首次保存 `<= 3s` 的旧指标。
- “退出应用时尽力暂存”的表现需要与后台队列的有界排空策略统一。

实现合并后应单独更新 MVP 的流程图、数据类型、状态机、模块职责、错误呈现和验收章节，避免两份计划长期冲突。

## 9. 参考实现与许可边界

可参考下列项目的架构思路：

- [macshot ScreenCaptureManager](https://github.com/sw33tLie/macshot/blob/main/macshot/Capture/ScreenCaptureManager.swift)：单次 ScreenCaptureKit 捕获。
- [macshot OverlayWindowController](https://github.com/sw33tLie/macshot/blob/main/macshot/UI/Overlay/OverlayWindowController.swift)：覆盖窗口预创建和显隐。
- [Flameshot ScreenGrabber](https://github.com/flameshot-org/flameshot/blob/master/src/utils/screengrabber.cpp)：截图后立即呈现的流程拆分。
- [TRex FrozenScreenSelectionOverlay](https://github.com/amebalabs/TRex/blob/main/Packages/TRexCore/Sources/TRexCore/FrozenScreenSelectionOverlay.swift)：原生冻结屏幕层。

这些参考说明高响应截图工具仍会先取得真实屏幕像素，性能来自单次 API、直接展示 `CGImage`、预热窗口、禁用动画以及把转换和持久化移出可见关键路径。macshot 和 Flameshot 使用 GPL 许可，只能借鉴公开架构思想，不复制其代码或可识别实现片段。
