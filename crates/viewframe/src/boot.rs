//! 渲染器启动配置：呈现节拍回调、呈现模式、GPU 偏好、UI 抗锯齿。
//!
//! 这些都必须在**任何窗口/渲染器创建之前**落实——呈现模式与环境变量都只在
//! 适配器初始化时被读一次，节拍回调必须在第一帧呈现之前注册好，否则前几帧的
//! 呈现周期估计会缺失。

/// 注册呈现节拍基准：让 `iced_winit` 在每次**成功呈现**之后回调本进程。
///
/// 必须在 `run()` 之前、且必须是"呈现"而不是别的时刻：`Program::draw` 的调用次数
/// 是 **layout 次数**（iced 的 canvas widget 在 layout 阶段就调 `draw` 构建几何），
/// 实测 layout 74/s 而真实呈现 37.7/s。面板周期、跳帧档位、笔尖外推的量化周期全
/// 部依赖这个估计，用错数据源会让笔尖落后指针。
pub fn install_present_hook() {
    iced_winit::frame_pacing::set_present_hook(crate::pacing::note_present);
}

/// 透传呈现模式到 `ICED_PRESENT_MODE`，由
/// `iced_wgpu::settings::present_mode_from_env` 解析成 wgpu `PresentMode`。
///
/// **环境里已有值时不覆盖。** 无条件执行会把继承来的 `ICED_PRESENT_MODE` 覆盖成
/// 配置值——于是"改环境变量来 A/B 呈现模式"这件事根本做不到（实测设 mailbox 仍跑
/// AutoVsync，日志里子进程看到的是 `Some("vsync")`）。而这恰恰是排查呈现/撕裂问题
/// 时唯一的逃生口：配置文件是持久状态，改它要重启、且会污染用户设置。
///
/// 白名单与 `present_mode_from_env` 保持一致；拼错的键必须响亮地失败，不能悄悄退回
/// vsync（那正是"设了没生效"的来源）。
pub fn apply_present_mode(present_mode: &str) {
    let from_env = std::env::var("ICED_PRESENT_MODE")
        .ok()
        .filter(|v| !v.trim().is_empty());
    if let Some(from_env) = from_env {
        eprintln!(
            "[viewstage] ICED_PRESENT_MODE 已在环境中（{from_env}），\
             优先于配置值 {present_mode}"
        );
    } else if matches!(
        present_mode,
        "immediate" | "no_vsync" | "mailbox" | "vsync" | "fifo" | "fifo_relaxed"
    ) {
        std::env::set_var("ICED_PRESENT_MODE", present_mode);
    } else {
        eprintln!("[viewstage] 未知的 present_mode={present_mode}，使用 iced 默认 AutoVsync");
    }
}

/// 双显卡机器锁定核显，避免唤醒独显（`WGPU_POWER_PREF=low`）。
///
/// 同样只在环境里没有值时设置，理由同 [`apply_present_mode`]。
pub fn apply_power_pref(prefer_integrated_gpu: bool) {
    if prefer_integrated_gpu && std::env::var("WGPU_POWER_PREF").is_err() {
        std::env::set_var("WGPU_POWER_PREF", "low");
    }
}

/// iced 的运行时设置。
///
/// **关闭 UI 三角形 MSAA**（iced 默认 `true` → MSAA 4x）：全部 UI 几何每帧 4 倍
/// 采样 + resolve 读回，在核显/教育大屏 GPU 上是确定的每帧填充带宽大头。白板批注
/// 层走自有 SDF 抗锯齿管线不受影响，文字走 glyph atlas 采样也不受影响；仅大色块
/// 三角形边缘可能有轻微差异（圆角/边框在 quad shader 内做 SDF，不依赖 MSAA）。
pub fn settings() -> iced::Settings {
    iced::Settings {
        antialiasing: false,
        ..iced::Settings::default()
    }
}
