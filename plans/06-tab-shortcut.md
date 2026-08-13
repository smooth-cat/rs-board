## 编辑界面支持 Tab 快捷键切换工具

tab / shift tab 按设置中维护的 “Tab 切换工具顺序” 列表在工具间切换。

1. 在编辑界面按 tab 切换到列表中 当前工具右边的 那个工具，如果已是最右，则切换到列表第一个
2. 按 shift + tab 切换到列表中 当前工具左边的 那个工具，如果已是最左，则切换到列表最后一个
3. 在编辑界面 tab 或 shift tab 不再具有默认的 “切换焦点” 功能
4. 用户可在设置中维护 Tab 列表：
   - 可依次添加 选择、方框、箭头、文字、画笔、序号 中的任意工具
   - 已加入列表的工具不可重复添加（添加按钮禁用）
   - 每个条目可单独移除（✕），移除后可重新添加
   - Tab 列表默认值为 方框、箭头
5. 当前工具不在列表中时，tab 跳到列表第一个，shift + tab 跳到列表最后一个；列表为空时 tab / shift tab 不切换工具
6. 编辑中保存设置后，当前编辑器立即使用新的 Tab 列表

## 实现说明

- 设置新增 `tab_order: Vec<EditorTool>`（snake_case 序列化，旧设置文件缺省时回退到默认列表），并保留版本号 1
- 编辑窗口是 immediate viewport，其输入不经过 `App::raw_input_hook`，因此在编辑器视图回调内拦截 Tab 事件：
  - 从 `InputState.events` 与 `raw.events` 移除纯 Tab / Shift+Tab 按下事件（带 ctrl / command 修饰的保留）
  - 通过 `Memory::move_focus(FocusDirection::None)` 取消 egui 已计算的焦点方向，从而禁用默认焦点切换
- 工具切换复用 `switch_tool`（先提交未完成的文字编辑），数字键 1-6 直接选择工具的逻辑不变
