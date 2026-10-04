# 渲染延迟与流程简化（第六轮）

> 目标：降低"事件 → 像素"的延迟，并减少每帧固定流程里的多余步骤。
> 方法：先建立每帧成本模型，再逐段核对是否存在**与渲染节拍无关**的时间源。

## 每帧流程（改动后）

```
输入/相机帧
  ├─ 触控/笔 → sys::touch hook → InvalidateRect → RedrawRequested（零消息）
  │     └─ canvas::Program::update → drain_touch_zero_msg() → step_momentum()
  └─ 消息（工具栏点击 / 相机帧 / 笔画完成）→ update() → view() 重建 → 重绘
                                                          ↓
Renderer::draw
  ├─ App::draw：写 viewport / last_frame_start / perf 采样
  ├─ prepare(encoder, viewport)
  │   ├─ CameraChannel：变换 uniform（变化才写 belt）+ 帧纹理（代数才重传）
  │   └─ AnnotationLayer：变换 uniform + 静态层（快照/视口门控）
  │                      + 动态层（指针指纹门控）+ 光标槽（指纹门控）
  ├─ render(encoder, target)
  │   ├─ 直通 pass：LoadOp::Clear(底色) → 摄像头 → 批注
  │   └─ iced 层 pass × N（空层跳过）
  └─ present：belt.finish() → queue.submit(1 次) → belt.recall()
```

---

## P3-1 Windows 计时器粒度 15.625ms　→ 已修复（本轮最大项）

**这是全应用最大的一处延迟来源，且此前完全没人设过。**

Windows 默认系统计时器粒度是 **15.625ms**，`std::thread::sleep` /
`WaitForSingleObject` 全部向上取整到下一个系统节拍。应用里的每一处短睡眠
都在被这个粒度放大：

| 位置 | 代码写的 | 实际睡眠 | 后果 |
| --- | --- | --- | --- |
| 相机推帧间隔（"自适应"档） | `sleep(33ms)` | ~46.9ms | 预览从 30fps 掉到 **~21fps**，节拍抖动 |
| 帧通道满时的重试退避 | `sleep(1ms)` | ~15.6ms | 主线程稍有积压就把采集线程卡住两帧 |
| 惯性定时器 | `every(16ms)` | ~31.25ms | 惯性只有 32Hz，肉眼可见卡顿 |
| `request_redraw_at` 补帧 | 亚 16ms 目标 | 15.6ms 量化 | 批注帧率上限（默认 60）**根本打不准** |

**修法**：启动时 `timeBeginPeriod(1)`，退出时 `timeEndPeriod(1)` 配对还原。
`windows-sys` 的 `Win32_System_Threading` 特性已在依赖里，无需新增依赖。

`timeBeginPeriod` 是引用计数的——若 winit/iced 内部已请求，这里只是计数 +1。
代价是全局 1ms 粒度会略微增加电源管理器唤醒频率，对交互式应用可接受。

> 放在 `simplelog` 初始化之后、`iced_winit::run` 之前：必须早于任何
> sleep / 定时器建立，否则头几帧仍走粗粒度节拍。

## P3-2 惯性由 16ms 定时器驱动 → 与 vsync 拍频　→ 已改为帧驱动

原实现用 `iced::time::every(Duration::from_millis(16))` 发 `Message::FrameTick`：

- **16ms vs 60Hz 的 16.667ms 存在拍频**。每个拍周期里总有一帧落在 vsync
  截止线之后，被迫多等一整帧 → 惯性动画周期性"顿一下"。
- 叠加 P3-1 的 15.625ms 粒度，`every(16ms)` 实际是 31.25ms → 惯性只有 32Hz。
- 每帧多一条消息往返：消息 → `update()` → **强制 `view()` 全量重建**。

**修法**：删除 `FrameTick` 消息、`needs_frame_tick` 与整条定时器订阅，
把积分挂到**已有的 `RedrawRequested` 零消息路径**上（`step_momentum`）：

- 积分时钟改用相邻两渲染帧的真实间隔，与渲染节拍同源；
- 每帧 `Action::request_redraw()` → 惯性逐帧锁 vsync；
- 惯性停止即不再请求重绘，帧流自然停住（与原 `needs_frame_tick` 的按需
  启停等价，但少一条消息、少一次 widget 树重建）。

安全性已核对：惯性只在松手时启动，而 `handle_pointer_down` 末尾就有
`momentum.stop()`，故"触摸与惯性叠加位移"不会发生；触控与惯性在时间上互斥，
`step_momentum` 顺序执行即可，无需二选一。

## P3-3 底色被建模成一条什么都不画的通道　→ 已简化

`BackdropChannel` 原本是一个 45 行的 `DirectChannel`：`init`/`prepare`/`draw`
**全部空实现**，唯一作用是让 `clear_color()` 返回一个 `[f32;4]`。每帧代价：

- 一次 `Box<dyn>` 堆分配对象；
- 两次虚调用（prepare/draw，均 no-op）；
- `Renderer::render` 里一次 `find_map` 遍历通道列表把它找出来。

**修法**：底色本质是**渲染器的一帧属性**，不是通道。改为
`iced_wgpu::set_frame_clear_color([f32;4])` + 一个 `AtomicU64`（8bit/通道
位域）。读取是一次 relaxed load：零分配、零虚调用、零遍历。
`DirectChannel::clear_color` 这个可选方法一并从 trait 删除。

顺带把 pack/unpack 抽成纯函数并配了 4 个测试——**纯黑底 `[0,0,0,1]` 与
"未声明"同形**（打包值为 0）这个限制被显式记录在测试里，以免将来有人要
"显式纯黑底"时被静默吞掉。宿主只用白板灰 (26,26,26)，不受影响。

## P3-4 帧率节流的毫秒整除　→ 已改为微秒

```rust
Duration::from_millis(1000 / fps as u64)   // 60 → 16ms ⇒ 实际节流到 62.5fps
Duration::from_micros(1_000_000 / fps as u64)  // 60 → 16666µs
```

毫秒整除让 60fps 档实际是 62.5fps，且 16ms 与 16.667ms 有 0.667ms 恒定
相位差，累积后帧会周期性错过 vsync 截止线。微秒精度把偏差压到 6µs 量级。

---

## 已评估、决定不改

| 候选 | 判断 |
| --- | --- |
| **相机帧走零消息路径**（像触控那样 `InvalidateRect`，跳过 `view()` 重建） | 收益最大：相机 30–60fps 下每帧都在做**全量 widget 树重建**。但背景层是由 `view_inner` 喂的（`update_camera_params` → `BackgroundCache` → `CameraLayer`），要零消息就得让 `CameraChannel::prepare` 直接读共享帧槽，牵动 `main.rs` → `viewgfx` → `viewcamera` 三层。相机是核心功能，改错就是黑屏，且本机无法编译验证。**下一轮做，需在 Windows 上先跑通基线。** |
| **合并直通 pass 与首个 iced 层 pass** | 现在是 1（直通）+ N（层）个 render pass。核显是 tile-based，pass 边界会触发 tile store。但层循环是上游代码，且中途会为 triangle/image 重新开 pass，合并需要重构该循环。收益不确定（每帧省一个 pass 边界），风险高。 |
| `ImageEntry` 的 RGBA 存了两份 + 列表无上限 | `load_image_from_rgba` 里 `Handle::from_rgba(w, h, (*rgba).clone())` 造成 1080p 下**每张照片两份 8.3MB**；`image_list` 无淘汰，50 张即 ~830MB，且侧栏每帧 O(N) 建缩略图 widget。**但**去重需要 `Handle::from_bytes(Arc<Bytes>)`（未在本机验证 iced 0.14 是否提供），淘汰则是产品决策（自动删用户照片）。**建议下一轮：先加内存占用统计与条目上限提示，再决定淘汰策略。** |
| 拍照后主线程的 8MB memcpy | `load_image_from_rgba` 在主线程克隆整帧。移到后台线程需要 `image::Handle: Send`（未验证）。~0.5–1ms 尖峰，优先级低于上面两条。 |

## 已核对无问题（避免重复排查）

- 相机纹理按 `generation` 门控重传，pan/zoom 只重写 uniform（走 belt），
  不重传像素；
- 批注静态层按"快照指针 + 视口位级"门控，动态层/光标槽按指针指纹门控；
- 悬停光标槽在无橡皮擦光标时零上传；
- 全仓无 `map_async` / `device.poll` / 回读，即无 GPU 同步等待点；
- 直通通道注册表每帧只做一次 relaxed 原子读（`HAS_PENDING` 快路径）。

## 验证方式

本机无 `cargo`/`rustc`，**未经编译验证**。静态检查已做：全仓括号平衡、
`Message` 变体与 handler 一一配平（54 项）、`DirectChannel` 实现者确认只剩
2 个（均已不含 `clear_color`）、已删符号全仓扫描、import 使用计数。

需在 Windows 上确认：

1. `cargo build --release` 通过（含 iced fork 的 `DirectChannel` trait 变更）；
2. `cargo test -p iced_wgpu -p viewstage` 通过（新增 4 个位域打包测试）；
3. **相机预览帧率**：设"自适应"档，用 `[rstat]` 或外部计时确认实际接近
   30fps 而非 21fps——这是 P3-1 最直接的验证点；
4. **惯性滚动**：拖拽画布松手，动画应完全平滑无周期性顿挫（原来每隔
   ~16 帧必顿一次）；
5. **书写延迟与笔迹平滑度**无回归。
