
## DComp 关键事实（Viewstage 合成层）
- **IDCompositionTarget/Visual 的 COM 引用必须保活到窗口销毁**——target 被 Drop 即拆除 visual 与 HWND 绑定，窗口全白。这是 P0 spike 数日排查的最终根因（2026-10-01 确认）。
- DComp 设备创建：本机 D3D12 queue 路线 E_NOINTERFACE，用 IDXGIDevice 或 null rendering device（实测均可合成）。
- v1 CPU surface 本机 dcomp.dll 缺陷（EndDraw segfault）；surface 更新走 D2D v2 路线。
- Present1 必须传 DXGI_PRESENT_PARAMETERS 默认结构体，nullptr 会 AV。
- 参考实现：crates/dcomp-rs-min（无 wgpu 的最小 DComp 链路，约 200 行）。

## P2 接线要点（iced_wgpu DComp surface）
- iced_wgpu `Surface` 枚举：Wgpu | Dcomp（`window/dcomp_surface.rs`）；VIEWSTAGE_DCOMP 环境变量门控（非"0"即启用）。
- 门控启用时**必须强制 Backends::DX12 且在 Instance 创建前**（as_hal::<Dx12> 依赖；Vulkan 后端拿不到队列）。
- iced_winit conversion.rs 已接 `with_no_redirection_bitmap(gate)`；winit 0.30.13 原生支持。
- resize 流程：device.poll(Wait) → ResizeBuffers(0,w,h,UNKNOWN,flag 0) → 重导入后缓冲。
- rwh 0.6.2：变体名 Win32；用 `RawWindowHandle::from(WindowHandle)` 取裸句柄。

## GPU 架构事实（2026-10-01 优化轮）
- DirectChannel trait 有 clear_color() 声明机制：BackdropChannel 以 LoadOp::Clear 提供底色（硬件清屏），勿再给它加回 shader/draw。
- CameraCommand::Freeze 是黑板模式相机冻结的正式命令；黑板关闭与设置面板禁用黑板必须 Unfreeze。
- 批注管线（viewink/render/annotation.rs）已是终极增量形态：chunks 增量上传 + 静态层 256px 瓦片光栅缓存 + blit + 全指纹门控，VIEWSTAGE_STATIC_BLIT=0 可退回几何重放做 A/B。段重放 GPU 成本可忽略，别再"优化"它。
- P3 相机 NV12 DComp overlay 的硬前提：iced 全管线输出预乘 alpha（UI 交换链须透明）。文本/quad shader 直通 alpha 会 fringe。
- 测量：安装版参考 exe（C:/Program Files\ViewStage）是空壳（无 WebView2 子进程、零 GPU 活动），不可作 GPU 基线；GPU 引擎计数器按 pid 过滤 + tasklist 快照差分归因（typeperf 无限采样必须省略 -sc）。

## 批注管线事实修正（2026-10-01 实测轮）
- upload_dynamic 现为三分支：stable_unchanged（只写尾层，书写时绝大多数帧）/ append_only（新满批块偏移增量）/ 全量兜底。**书写时动态上传恒定 ~46KB/s，与笔画长度无关**——若再看到随笔画增长的上传量，说明增量路径又坏了。
- VIEWSTAGE_STATIC_BLIT 语义：默认开启（未设置=开），=0 才退回几何重放。静态层渲染 = 256px 瓦片 blit（O(内容瓦片)），不是几何重放。
- 批注延迟链已到头：RawTouch* 同步递归处理、尾层重建 1-2μs/次、request_redraw_at 取 min 无抖动。剩余优化空间在 P3 相机 overlay（预乘 alpha 前提），不在批注层。

## 输入管线事实（2026-10-01 延迟专项）
- iced 运行时对**每批消息无条件 build_user_interfaces**（全 widget 树重建+layout）——任何高频消息路径都是延迟源；零消息通道（canvas::Program::update 直写共享状态）是唯一低延迟路径。
- UserInterface<'a> 与 program 生命周期绑定：旧 UI 存活期间不能 &mut program ⇒ fork 层"跳过重建"结构性不可行（已验证并回退）。
- 笔/触控管线（现状）：WH_CALLWNDPROC hook（UI 线程）入队 + InvalidateRect → RedrawRequested 内 drain_touch_zero_msg（Down/Move 零消息直写）→ Up 延迟 TouchFinalize 消息（每笔画一次）。手握 &mut 的操作（finalize/undo/mesh）必须走消息。
- bash 下 cargo test/build 链接失败：GNU link 抢占 MSVC link.exe + SDK LIB 缺失 ⇒ 用 tmp-probe/run_tests.bat（vcvars64 + cargo test %*）。
- paced_redraw 语义：is_drawing 满刷新率（延迟优先）；60fps cap 仅悬停/平移。
