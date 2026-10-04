# 每帧合成效率（第八轮）

> 目标：降低"提交命令 → 上屏"这一段的固定开销，即**合成**本身，不涉及
> 镶嵌、上传或输入。

## 结论：把 6 个 render pass 压成 1 个

合成栈的绘制目标自始至终是同一个（同一 `color attachment`、无深度附件），
但旧结构把它切成了多个 render pass：

```
旧： ┌─ 直通 pass ─────┐  clear → 摄像头 → 批注
    └─ 层 pass #1 ────┐  Load → 工具栏（quad + image + text）
    └─ 层 pass #2 ────┐  Load → 画笔面板
    └─ … 每个非空层一个 pass
```

pass 边界不是免费的。核显是 **tile-based** 的：一个 pass 结束会强制把
tile buffer resolve/store 到内存，下一个 pass 重新载入。所以每多一个 pass
边界，就是一次全屏尺寸的额外显存往返。应用默认 `prefer_integrated_gpu`
（`WGPU_POWER_PREF=low`），也就是说这条路径**正好落在最在意 pass 数量的
硬件上**。

稳态 pass 数（工具栏常在，可选浮层按需）：

| 场景 | 旧 | 新 |
| --- | --- | --- |
| 只有工具栏 | 2 | **1** |
| 工具栏 + 画笔面板 | 3 | **1** |
| 工具栏 + 侧栏 + 画笔面板 | 4 | **1** |
| 全部浮层展开 | 6 | **1** |

## 为什么可以合并

三个前提，都已逐一核对：

1. **无深度附件**，全部绘制按顺序混合到同一 attachment —— 层与层的先后
   关系就是命令顺序，与 pass 边界无关。
2. **每个批次渲染器都自设 scissor**：`quad::render`（`quad.rs:97`）、
   `image::render`（`image/mod.rs:389`）、`text::render`（`text.rs:403`）、
   `primitive` 循环（`lib.rs:676`）都在绘制前调 `set_scissor_rect`。
   所以层与层之间不存在 scissor 状态泄漏——这是合并能成立的关键。
   直通通道同样各自设全屏 scissor（`CameraChannel::draw`、
   `AnnotationLayer::draw`）。
3. **每个渲染器都自设 pipeline / bind group**，管线状态不会从直通通道
   泄漏到 UI 批次（反之亦然）。

唯一的例外是 `triangle`：它自带 pass 语义（可能走 MSAA + resolve）。
层循环里**保留**了原来的 close/reopen 处理。而且本应用**根本不产生
triangle**——工具栏图标是 `viewicon` + `tiny-skia` 预栅格化后走 `image`
widget，触控事件层的 `canvas::Program::draw` 返回空 `Vec`。所以稳态下
`render()` 里的 `begin_render_pass` 只执行第 1 处。

## 顺带删掉的死代码

合并之后有两处逻辑变成不可达：

- **`frame_initialized` 标志**：原先用来决定"这一层要不要 clear"。现在帧
  pass 一律带 clear（`frame_load()`），标志无意义。
- **末尾的兜底 clear 分支**：`if !frame_initialized { ... }`。原先是
  "一层都没画时也要清一次屏"，现在帧 pass 本身就完成了这件事。

`frame_load` 闭包也随之简化——原先的 `initialized: bool` 参数只剩
`false` 一个调用点。

## 已核对为"无优化空间"的部分

避免下一轮重复排查：

| 项 | 结论 |
| --- | --- |
| 摄像头绘制 | 单个 `draw_indexed(0..6, 0, 0..1)` 全屏四边形，NV12 双平面在**同一个** shader 里采样。不是 2 次全屏绘制。 |
| 摄像头混合模式 | 不透明源走 `pipeline_rgba_replace`（REPLACE，旁路 dst 读取）。透明图片才用 ALPHA。 |
| 图标绘制 | 9 个图标进同一个 `image::Batch` → 1 次 draw，不是 9 次。 |
| 文字绘制 | 每层一个 `text::Batch` → 1 次 draw。 |
| 空层 | `layer.is_empty()` 提前跳过（第三轮已做）。触控事件层恒为空。 |
| `debug::render()` | `iced_debug` 未启用 `enable` feature（那需要 `iced_beacon`，非依赖），编译为返回 unit `Span` 的空函数，热循环里零成本。 |
| 批注 3 次 draw | 静态/动态/光标必须分槽——合槽会导致每帧重传静态几何。光标槽 `seg_count == 0` 时已跳过。 |
| 墨迹 overdraw | 墨迹画在已渲染的摄像头之上，每覆盖像素必然被着色两次。架构决定，除非引入深度做 early-Z（而 alpha 混合本就吃不到 early-Z）。 |

## 验证方式

本机无 `cargo`/`rustc`，未经编译验证。静态检查：全仓括号平衡、
`begin_frame_pass` / `MaybeUninit` / `frame_initialized` / `direct_pass`
等误替换残留全仓扫描（sed 批量替换曾把 `begin_render_pass` 一并改坏，已发现并修复）。

Windows 上请确认：

1. `cargo build --release` 通过（iced fork 的 `Renderer::render` 是结构性改动）；
2. **画面与改动前逐像素一致**——本改动只改 pass 划分，不改任何绘制调用、
   顺序、状态。若画面有变化，说明合并破坏了某个前提（最可能是 scissor 泄漏）；
3. 相机画面、笔迹、工具栏、侧栏、画笔面板、设置面板、菜单、HUD 的
   遮挡关系都正确（层序即命令序）；
4. 用 GPU 厂商工具（或 `[rstat]` 配合）确认 render pass 数从 6 降到 1。
