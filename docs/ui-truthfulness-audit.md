# UI 真实性审计：设置项 vs 实际行为

> 目标：设置界面里**每一项**都必须真的改变程序行为。既不能有"改了没反应"的
> 假控件，也不能有"真实存在但用户看不到"的有效配置。
>
> 审计方法：对 `Config` 每个字段做全仓读取点扫描（排除 `config.rs` 自身的
> 定义/默认值/`sanitize`，以及 `app_settings.rs` / `oobe.rs` 的 UI 展示），
> 再对每个"零读取点"字段逐个手工确认。

## 一、已删除的假设置（8 项）

这些字段**只被写入并落盘，全仓没有任何读取点**——控件能动、配置文件会变、
程序行为不会变。

| 字段 | UI 上的说法 | 实际情况 |
| --- | --- | --- |
| `application.language` | "设置界面语言" | `src/locales/{zh-CN,en-US}.json` 存在但**从未被加载**，全仓无 i18n 读取点。切语言零可见变化。（UI 原文已自认"完整翻译资源尚未接入"） |
| `application.blackboard_enabled` | "控制主工具栏是否允许进入小黑板模式" | 随小黑板按钮一并移除（见下） |
| `canvas.pen_effect_mode` | "控制笔锋计算强度"（关闭/限制/完整） | 无读取点。笔锋永远按 `pen_min_width_ratio` + `pen_tail_duration` 全量计算，三档选项无一有效 |
| `theme.theme` | "选择界面主题风格"（深色/浅色） | 只被读取来决定**哪个按钮高亮**。所有 UI 颜色是各视图里硬编码的深色常量（`toolbar.rs` / `sidebar.rs` / `settings.rs` / `pen_panel.rs` / `oobe.rs` / `app_settings.rs` 共 73 处 `from_rgb8`），且 `Application` 收到的 `&Theme` 被 `_theme` 丢弃——**没有任何主题切换代码路径** |
| `theme.blur_enabled` | "为工具栏和侧边栏添加毛玻璃模糊效果" | 无读取点。且 iced 的 `Shadow::blur_radius` 是**阴影**模糊，不是背景毛玻璃——所描述的能力在当前架构下本就不存在 |
| `storage.auto_clear_days` | "自动清理周期" | 无读取点，没有任何清理执行器。（UI 原文已自认"清理执行器尚未实现"） |
| `developer.max_scale_image` | "图片最大缩放倍数"（2–10 滑块） | 无读取点。真正的上限是编译期常量 `viewcore_touch::ScreenTransform::MAX_SCALE = 10.0` |
| `developer.pinch_zoom_v2` | "启用新版捏合缩放算法" | 无读取点。只有一套捏合缩放实现 |

### 附带修掉的一个真 bug

`pen_min_width_ratio` 与 `pen_tail_duration` **在 `canvas` 和 `developer` 下各有一份**，
而 `begin_pen_stroke` 读的是 `canvas` 那份、旧设置页滑块写的是 `developer` 那份。
**开发者模式下把"笔锋最小宽度比"拖到底，笔锋完全不变。**

已删除 `developer` 下的重复副本，设置页改写 `canvas` 字段并新增
`PenTailDurationChanged`（原先根本没有 UI 入口）。

## 二、新暴露的真实设置（3 项）

审计同时发现反向问题：这些设置**确实生效**，但此前完全不在 UI 里。

| 字段 | 真实作用 | 消费点 |
| --- | --- | --- |
| `canvas.annotation_fps_cap` | 批注热路径渲染帧率上限（30–240）。**全应用最有效的性能旋钮**——144/165Hz 屏上批注 60fps 已足够顺滑，限帧线性降低 GPU 占用与功耗 | `App::paced_redraw` |
| `renderer.present_mode` | `ICED_PRESENT_MODE=mailbox` vs 默认 AutoVsync。垂直同步 = GPU 占用/功耗最低；无背压 = 延迟最低但核显持续满载 | `main()` 启动期 |
| `renderer.prefer_integrated_gpu` | `WGPU_POWER_PREF=low`，双显卡笔记本锁定核显避免唤醒独显 | `main()` 启动期 |

后两项在**创建 wgpu 实例之前**读取，因此运行期改动需重启——设置页顶部有
明确的"需重启生效"提示条，handler 也会回填 `settings_feedback`。

## 三、只读调试文本 → 真正的编辑器

`画笔预设粗细` / `橡皮擦粗细预设` 此前以 `format!("{:?}", presets)` 渲染成
`[2.0, 5.0, 10.0, 15.0, 21.0]` 这样的裸文本，**不可编辑**，却摆在一堆可交互
控件之间，看着像能点的设置。现改为每档一个滑块（`PenSizePresetChanged` /
`EraserSizePresetChanged`），并按 `sanitize` 的同一区间钳位。

`画布缩放范围` 改为如实展示编译期常量（10%–1000%），并注明"不可配置"——
而不是提供一个无效的滑块。

## 四、工具栏按钮移除

### 小黑板（`ToolButton::Blackboard`）

**移除理由**：进入黑板模式会 `strokes.clear()` + `history.clear()` + 冻结摄像头，
是**不可逆**的状态切换（全部笔迹与撤销栈一次性丢失，无确认、无撤销入口），
却藏在主工具栏一键可达。相机画面本身即可批注，功能被「批注模式」完全覆盖。

连带删除（否则全部不可达，成为死代码）：
`DrawMode::Blackboard`（5 处 `matches!` 改为 `== DrawMode::Comment`）、
`blackboard_open`、`CameraCommand::{Freeze, Unfreeze}`、
`Message::BlackboardSettingToggled`、`OobeAction::BlackboardSettingToggled`、
`config.application.blackboard_enabled`、`blackboard.svg` + 图标登记项。

> `should_freeze` 字段**保留**：拍照后它会置位以冻结画面（`CameraEvent::Captured`
> 路径），这是真实功能。原先的 `Unfreeze` 命令只剩 `enter_camera` 一处用途，
> 已改为由 `CameraCommand::Start` 隐含"要画面"语义。

### 文档（`ToolButton::File`）

**这个按钮完全无效**：handler 只翻转 `file_sidebar_visible`，而该字段**全仓只写不读**
（`grep` 命中 6 处全是赋值/初始化，无任何读取点）——没有任何代码据它渲染东西。
按下它唯一的实际效果是 `sidebar_visible = false`，即**把图片侧栏关掉**。

已删除按钮、`ToolButton::File` 变体、handler 与 `file_sidebar_visible` 字段。
图片库改由「图片」按钮（`ToolButton::Image`）开关，功能不受影响。

## 五、首次引导（OOBE）同步

引导原本 5 步，其中 3 步在问假问题：

- 第 0 步「选择语言」→ 删除（`language` 无读取点）
- 第 1 步「选择主题」→ 删除（`theme` 只决定按钮高亮）
- 第 2 步「性能设置」的「钢笔效果」三档 → 删除（`pen_effect_mode` 无读取点），
  **替换为真实的「批注帧率上限」滑块**——这才是用户真正关心的性能旋钮

现为 3 步：性能（预览帧率 + 批注帧率）→ 摄像头 → 完成。
步骤数由 `OOBE_LAST_STEP` 常量统一驱动，避免再出现硬编码步数与实际步骤脱节。

## 六、顺带清理

图标注册表（`src/views/icons.rs`）中 9 个零引用项已删除，对应 8 个 SVG 资产
一并移除。`file-earmark-image` 映射的 `file-earmark-image-fill.svg` 同样移除。
现注册表 21 项与资产目录 13 个文件一一对应，每个都有真实引用点。

`viewicon` 里的 `blackboard_evenodd_style_path` 测试改名为
`evenodd_style_path_with_many_arcs`：它只是借用一段复杂 path 数据做解析器测试，
与该图标是否发布无关，数据已内联在测试里。

## 七、遗留：仍未接入的功能（有据可查，非本次范围）

- **i18n**：`src/locales/*.json` 已存在但无加载代码。若要接入，需要一个
  `tr(key)` 层并把所有视图里的中文字面量替换为键。
- **主题系统**：需要把 73 处硬编码颜色抽成按主题解析的调色板，并让
  `Application::theme` 真正返回配置值。
- **图片清理执行器**：`auto_clear_days` 删除后，照片目录不会自动清理。
  实现它需要按日期扫描目录并删除。
- **缩放上限可配置**：需要把 `ScreenTransform::MAX_SCALE` 从常量改为运行时值
  （涉及 `viewcore-touch` 的零分配设计约束）。

## 验证方式

本机无 `cargo`/`rustc`，**未经编译验证**。静态检查已做：括号平衡（全仓）、
`Message`/`OobeAction` 变体与 handler 一一配平（55 / 7 项全部有 handler）、
`SettingsSection` 与页面分派配平、`ToolButton` 与 toolbar 按钮及 handler 配平、
import 逐文件使用计数、图标登记项与引用点双向核对、已删符号全仓扫描。

需在 Windows 上确认：`cargo build --release` 通过，
`cargo test -p viewstage` 通过（config.rs 新增 3 个测试验证旧配置兼容与钳位），
并人工过一遍设置页每一项是否真的有可见效果。
