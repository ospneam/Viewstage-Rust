//! 性能 HUD 与渲染仪表的应用侧出口。
//!
//! # 为什么要这层
//!
//! 应用此前直接调 `viewink::stats` 的 20 个函数：7 个读数 + 一段 55 行的
//! `format!`。前者让应用耦合到渲染层的诊断实现（统计口径一变，编译器不报错，
//! 只是数字悄悄变了）；后者把渲染指标排版塞进了 `view_inner`，而 `view_inner`
//! 本该只做"叠哪些浮层"。
//!
//! 收口之后：
//!
//! - 应用调 [`Instrument`] 上的**语义**方法，不出现 `viewink::stats` 字样；
//! - 指标排版在 [`text`] 里，统计口径与呈现格式挨在一起——改一处不会漏另一处。
//!
//! # 应用自有数据怎么办
//!
//! HUD 里有三项是**文档/笔迹**的数量（笔画数、擦除数、网格数），渲染层无权知道。
//! 它们由 [`HudInputs`] 的调用方填入——这个方向很重要：渲染层向应用**索取**
//! 业务计数，而不是反过来去读应用状态。

use std::time::Duration;

use viewink::stats;

use crate::metrics::PerfSnapshot;

/// 本帧的重绘来源。
///
/// 镜像 `viewink::stats::RedrawSource` 但收窄到应用实际会设置的那几个，
/// 这样新增变体不会意外漏一个映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedrawSource {
    /// 输入路径（`paced_redraw`）。
    Input,
    /// 惯性积分（`step_momentum`）。
    Momentum,
    /// 后台笔画几何计算完成。
    StrokeComputed,
}

impl From<RedrawSource> for stats::RedrawSource {
    fn from(s: RedrawSource) -> Self {
        match s {
            RedrawSource::Input => stats::RedrawSource::Input,
            RedrawSource::Momentum => stats::RedrawSource::Momentum,
            RedrawSource::StrokeComputed => stats::RedrawSource::StrokeComputed,
        }
    }
}

/// 渲染仪表的应用侧出口。
///
/// 全部方法仅 `&self` 或无状态，与 `canvas::Program` 的零消息热路径兼容。
/// 只在开发者模式（性能 HUD）开启时读取，开关本身由应用把持。
pub struct Instrument;

impl Instrument {
    /// 记账本帧的重绘来源。
    ///
    /// 不分类就无法判断"超出的帧"到底来自哪里——闸门装在输入路径上，但重绘还有
    /// 别的来源（摄像头消息导致的 view 变化、窗口事件、iced/winit 内部）。
    pub fn set_redraw_source(source: RedrawSource) {
        stats::set_redraw_source(source.into());
    }

    /// 记一次触控事件。
    pub fn add_touch_event() {
        stats::add_touch_event();
    }

    /// 记一次经框架原生 `Event::Touch` 真正起笔。
    pub fn add_touch_stroke_started() {
        stats::add_touch_stroke_started();
    }

    /// 记一帧到达的摄像头帧尺寸。
    pub fn note_camera_frame(width: u32, height: u32) {
        stats::note_camera_frame(width, height);
    }

    /// 记一次**源端**（MediaFoundation 时间戳）投递间隔。
    ///
    /// 与"我们收到帧的间隔"分开量：两者一致 → 抖动来自设备/驱动；源端均匀而我们
    /// 不均匀 → 我们丢了样本。
    pub fn note_camera_source_interval(delta: Duration) {
        stats::note_camera_source_interval(delta);
    }

    /// 记一次帧内容换上屏的间隔。
    pub fn note_camera_interval(delta: Duration) {
        stats::note_camera_interval(delta);
    }

    /// 记一次采集线程阻塞等待耗时。
    pub fn note_camera_poll(elapsed: Duration) {
        stats::note_camera_poll(elapsed);
    }

    /// 记一次 `view()` 构建耗时。
    pub fn note_view_cost(elapsed: Duration) {
        stats::add_view(elapsed);
    }
}

/// HUD 的输入。
///
/// 三项文档计数由应用填入——渲染层不知道笔迹模型。
#[derive(Debug, Clone, Copy)]
pub struct HudInputs {
    /// 渲染帧率 / 帧时间 / 内存（来自 [`PerfSnapshot`]）。
    pub perf: PerfSnapshot,
    /// 已完成笔画数。
    pub stroke_count: usize,
    /// 已完成橡皮擦数。
    pub eraser_count: usize,
    /// 可渲染网格条目数。
    pub mesh_count: usize,
}

/// 性能 HUD 的文本内容。
///
/// 排版住在这里而不是 `view_inner`，因为**统计口径与呈现格式必须一起改**：
/// 「驻留段数 × 64B = 显存」这类换算一旦只改一边，HUD 就会开始说谎。
pub fn text(inputs: HudInputs) -> String {
    let perf = inputs.perf;

    // 视口剔除效果：静态层实际绘制的笔画数 vs 总量
    let (visible, total) = stats::static_visible();
    // 驻留式上传：缓冲按整份文档增长（×64B 即显存），绘制按可见区间分批
    let (resident_segs, draw_ranges) = stats::static_resident();
    // 帧率稳定性：实测刷新周期 + 长帧数（>1.5 周期）+ 窗口内最坏帧。
    // `long > 0` 即表示存在肉眼可见的顿挫，是"稳定 60fps"的判定点。
    let (long_frames, worst_ms) = stats::frame_stability();
    // 帧预算：宿主在帧内可控的 CPU 开销（镶嵌 + 打包上传 + 剔除）对比实测刷新
    // 周期。逼近周期即必然出现长帧（错过 vsync 死线 = 整帧多等一个周期），
    // 这是"笔迹是否跟手"的唯一硬指标。
    let (prep_avg, prep_max, work_avg, work_max, refresh_ms) = stats::frame_budget();
    // 静态层离屏缓存是否命中（高密度文档才启用；0 = 密度不够或键在变）
    let raster = stats::raster_blit_used();
    // GPU 侧成本核算（估计值，非硬件计时）：墨迹片元 vs 全屏 blit 纹素。
    // 二者哪个大，决定了"还值不值得再往下压"。
    let (_, ink_frag, blit_texels, _) = stats::raster_cost();
    // 触控诊断：经框架原生 `Event::Touch` 真正起笔的次数（自定义 hook 管线已
    // 删除，"hook 装没装 / WM_POINTER / 入队 / drain"那组计数恒为 0，是误导性
    // 噪声，故不再显示）。
    let touch_strokes = stats::touch_strokes_started();

    format!(
        "FPS {:.0} | 帧 {:.1} ms\n内存 {:.1} MB\n笔画 {} | 擦除 {} | 网格 {}\n\
         静态可见 {}/{}\n驻留 {} 段 {:.1} MB | 绘制 {} 批\n\
         刷新 {:.1} Hz | 长帧 {} | 最坏 {:.1} ms\n\
         帧预算 {:.2}/{:.2} ms（{:.0}%）\n\
         离屏缓存 {} | 墨迹 {:.2} Mpx | blit {:.2} Mpx\n\
         触控起笔 {}",
        perf.fps,
        perf.frame_ms,
        perf.memory_mb,
        inputs.stroke_count,
        inputs.eraser_count,
        inputs.mesh_count,
        visible,
        total,
        resident_segs,
        resident_segs as f64 * 32.0 / (1024.0 * 1024.0),
        draw_ranges,
        1000.0 / stats::measured_refresh_ms().max(0.001),
        long_frames,
        worst_ms,
        work_avg + prep_avg,
        work_max + prep_max,
        if refresh_ms > 0.0 {
            (work_avg + prep_avg) * 100.0 / refresh_ms
        } else {
            0.0
        },
        if raster { "命中" } else { "未启用" },
        ink_frag as f64 / 1_000_000.0,
        blit_texels as f64 / 1_000_000.0,
        touch_strokes,
    )
}