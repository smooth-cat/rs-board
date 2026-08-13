# 方框相邻与交叉时的标签联合避让

## Summary

- 方框本体继续允许相邻、交叉和包含，只重排方框标签。
- 创建、粘贴、删除、移动、缩放、标签拖动、文字/字号/线宽变化及层级调整均重排相关冲突连通组。
- 移动、缩放、创建和文字编辑期间实时预览；提交后形成一次 revision 和一条撤销记录。
- 保留 [box-hit.html](/Users/linjiajian/Desktop/my/my-project/rs-board/plans/box-hit.html)，本次不再修改 HTML。

## Data And APIs

- `RectanglePayload` 新增 `preferred_label_anchor`：
  - `preferred_label_anchor` 保存默认或用户手拖位置。
  - `label_anchor` 保存求解后的实际位置。
  - 新方框两者初始均为 `Top / Outside / 0.0`。
- 将命令调整为 `SetRectangleLabelPlacement { element_id, preferred_anchor, actual_anchor }`，一次更新偏好与实际位置；自动避让时保持原偏好不变。
- 在 `common` 新增轻量的 `RectangleLabelScene`、`RectangleLabelSolution` 和 `solve_rectangle_label_reflow(before, after, primary_id, seed_ids)`；场景只复制方框数据，不复制画笔点。
- `app` 新增按 `ElementId` 覆盖的 `ElementPreviewSet`，替换当前单个 `released_preview_element`。
- 存档 schema 升至 `4`：
  - v3 的旧 `label_anchor` 同时迁移为 preferred 和 actual。
  - v2 继续推导旧锚点，再写入两个字段。
  - 摘要读取接受 v2、v3、v4，未来版本继续拒绝。
  - 迁移和打开文档时不主动重排，避免旧文档产生视觉漂移。

## Collision Solver

- 图节点包含所有方框，隐藏标签不参与放置但其方框本体仍是障碍。
- 两节点在以下情况连边：方框本体接触/相交、actual/preferred 标签与对方本体冲突，或双方标签冲突。
- 使用变更前后两张图的边集并集，从操作方框开始 BFS；因此障碍移开或删除后，被挤开的标签也会回到偏好位置。
- 标签障碍仅包括其他方框本体和方框标签；箭头、文字、画笔、序号继续忽略。
- 标签与外部方框的间距取该标签的 `anchor_offset_px`；标签之间取双方 offset 的最大值。边缘接触也算冲突，并使用 `0.01px` 数值余量。
- 在八条内外轨道上根据原始布局计算画布合法区间，再减去障碍投影区间；不使用 `fit_label_bounds_to_canvas` 后的平移框参与求解。
- 每个可用区间生成 preferred 投影点、current 投影点和两个端点；单标签最多保留 24 个候选。
- 使用宽度 128、最多 4096 状态的确定性 beam search。一次最多重排 32 个可见标签；超出时按 BFS 距离、当前操作框、`z_index` 降序、UUID 升序截取，其余标签作为固定障碍。
- 解的标签优先级为：当前操作框优先，其余按 `z_index` 降序、UUID 升序。
- 对每个标签按以下顺序比较候选：保持原轨并沿边滑动、同边换内外侧、换边、偏离 preferred 的距离、相对 current 的视觉位移。所有浮点比较使用 `total_cmp`，最终再按固定锚点顺序决胜。
- 首轮只接受零碰撞解；若外部固定标签阻塞，则最多扩展一次其 owner 后重新求解。
- 零碰撞无解时，专项尝试当前操作框的 `Top / Inside` 合法位置。
- 仍无解时，在所有轨道选择“安全间距重叠面积、碰撞对数量、偏好代价、稳定锚点序”最小的结果；标签仍必须完整位于画布内。
- 标签尺寸本身无法放入画布时，拒绝引发该尺寸的编辑并保持原文档不变。

## Editor Integration

- 方框移动和缩放先按方框本体及描边约束画布，再求标签位置，避免旧标签锚点反向限制方框几何。
- 创建方框时在按下指针后立即分配稳定 `ElementId`，保证整个拖动过程的求解顺序不闪烁。
- 每帧从持久文档和当前输入重新构造轻量场景，只求解一次；正文、内联文本框和选择控制点共用同一个 `ElementPreviewSet`。
- 手拖标签先更新临时 preferred，再联合求解 actual；邻框标签允许同时移动。
- 松手或离散操作时，批次顺序固定为基础命令在前、受影响标签按最终层级和 UUID 排序在后，并过滤所有无变化命令。
- 撤销和重做直接恢复批次保存的几何、preferred 与 actual，不在历史回放时重新运行求解器。
- 创建和粘贴的新增元素直接携带最终实际锚点；删除操作使用旧图找到需要恢复偏好的邻框。

## Test Plan

- 覆盖水平/垂直相邻、部分交叉、十字交叉、完全包含及画布四边。
- 验证优先沿原轨滑动、必要时换侧或换边、冲突解除后自动回到 preferred。
- 覆盖三个以上方框的连锁传播、隐藏标签方框、外部标签扩组及超过 32 个标签的稳定降级。
- 覆盖长中文换行、内部上侧兜底、最小重叠降级、安全间距和边缘接触。
- 使用固定 UUID 重复求解，验证结果与输入遍历顺序无关且连续帧无闪烁。
- 验证创建、粘贴、删除、移动、缩放、拖标签、文字显隐、字号和线宽变化均触发重排；非方框元素始终不参与。
- 验证实时预览与提交结果逐元素一致，一次操作只增加一次 revision，撤销/重做完整恢复所有联动标签。
- 验证 v2/v3 到 v4 的迁移、v4 JSON/snapshot 往返、摘要读取以及未知高版本拒绝。
- 最后运行 `cargo test --workspace` 和 `cargo clippy --workspace --all-targets -- -D warnings`。
