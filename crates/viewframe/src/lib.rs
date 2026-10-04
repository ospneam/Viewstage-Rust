//! # viewframe — 帧驱动层
//!
//! 从主 crate 隔离出的「一帧是怎么被驱动出来的」：呈现周期估计、vsync 跳帧
//! 档位、笔尖外推的量化周期、摄像头上屏网格、直通合成通道注册、每帧渲染
//! 计量。
//!
//! ## 为什么独立成一层
//!
//! 这一层此前散落在应用里（`src/app/pacing.rs` 与 `main.rs` 的数十个方法），
//! 混杂着消息路由与业务状态，导致两个问题：
//!
//! 1. **节拍标定无法回归测试。** 跳帧档位、量化周期、EMA 抗尖峰这些决策全
//!    部是纯函数，却因为寄生在 `App` 上而只能靠真机跑起来看 HUD 数字。
//! 2. **帧热路径与业务状态纠缠。** `canvas::Program::draw` 里有 130 行是渲染
//!    记账，却和「用户点了哪个按钮」共用同一个结构体。
//!
//! ## 职责边界
//!
//! - 本 crate：**决策**——什么时候重绘、跳几个 vsync、量化周期多长、上屏时刻
//!   落在哪个格点。全部是纯计算或 `Cell`/`RefCell` 状态。
//! - [`viewgfx`]：画布几何编排（镶嵌、静态快照、触控轨迹）。
//! - [`viewink`]：笔迹算法与 GPU 批注管线。
//! - [`viewcamera`]：像素与采集。
//! - 本 crate **不持有任何 GPU 设备对象**——通道注册只是把别人造好的
//!   `Box<dyn DirectChannel>` 交给 `iced_wgpu`。
//! - 本 crate **不知道应用的 `Message`**：重绘决策以 [`Redraw`] 返回，由宿主
//!   翻译成 `iced::widget::canvas::Action`。
//!
//! ## 热路径约束
//!
//! [`FrameDriver`] 的全部方法仅 `&self`（内部 `Cell`/`RefCell`/`Arc`），与
//! `canvas::Program` 的零消息签名兼容。稳态下禁止分配。

pub mod boot;
pub mod channels;
pub mod display;
pub mod grid;
pub mod metrics;
pub mod pacing;

use std::time::Instant;

pub use channels::Channels;
pub use grid::CameraGrid;
pub use metrics::{FrameMetrics, PerfSnapshot};
pub use pacing::{
    effective_period_ms, measured_refresh_hz, note_present, panel_period, skip_count,
    stroke_skip, throttle_wake_offset_us, PeriodEstimate, STROKE_MIN_HZ,
    STREAM_CONTINUITY_GAP, WORK_ESTIMATE_US,
};

/// 本次重绘请求的归宿。
///
/// 之所以不用 `iced::widget::canvas::Action<T>`：那个类型的 `Publish(T)` 变体会
/// 把宿主的 `Message` 拖进本 crate 的签名，而重绘请求根本不带消息——它是纯粹的
/// 「什么时候再画一次」。宿主在边界上把这个枚举翻译成 `Action`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redraw {
    /// 交给 vsync（本帧就渲染，不额外节流）。
    Now,
    /// 跳过若干个 vsync，在唤醒点请求重绘。
    ///
    /// 唤醒点取 vsync 区间**正中**，前后各有 `period/2` 裕度，绝不会因"差一点
    /// 赶上"而丢帧。
    At(Instant),
}

/// 每帧重绘决策的输入（由宿主提供，本 crate 不持有业务状态）。
#[derive(Debug, Clone, Copy)]
pub struct RedrawInputs {
    /// 书写 / 橡皮擦轨迹期间是否在重建动态几何（走 GPU 增量管路）。
    ///
    /// 与「笔已按下」的区别：后者是输入状态，这里还包含橡皮擦悬停圆环（它同样
    /// 每帧重建动态几何、同样吃 GPU）。
    pub stroke_rendering: bool,
    /// 「直接操作」中：连续跟手的交互，一律满 vsync。
    ///
    /// 平移 / 惯性 / 触摸属于此类。**书写不在其中**——书写虽然跟手，但它的 GPU
    /// 占用率是第一约束，宁可要一个稳定的 60-72fps 也不要 144fps 的满刷新率把
    /// 占用率推到 2 倍。由此引入的额外延迟由笔尖外推补偿（见
    /// [`FrameDriver::effective_frame_period_ms`]），两处是配套改动，不能只回退
    /// 其中一个。
    pub direct_manipulation: bool,
    /// 普通批注（无笔迹、无直接操作）的帧率上限，来自 `canvas.annotation_fps_cap`。
    pub annotation_fps_cap: u32,
}

/// 帧驱动门面：宿主每帧调用的唯一入口。
///
/// 所有方法仅 `&self`——内部状态全部 `Cell`/`RefCell`/`Arc`，与 canvas 零消息
/// 热路径兼容。
pub struct FrameDriver {
    metrics: FrameMetrics,
    channels: Channels,
    grid: CameraGrid,
}

impl FrameDriver {
    /// 创建帧驱动门面。
    pub fn new() -> Self {
        Self {
            metrics: FrameMetrics::new(),
            channels: Channels::new(),
            grid: CameraGrid::new(),
        }
    }

    /// 摄像头上屏网格（帧到达入槽、拍照冻结、切源清空）。
    pub fn camera(&self) -> &CameraGrid {
        &self.grid
    }

    /// 帧计量（帧起点、输入时刻、性能 HUD 采样）。
    pub fn metrics(&self) -> &FrameMetrics {
        &self.metrics
    }

    /// 直通合成通道注册闸门（幂等，每进程一次）。
    pub fn channels(&self) -> &Channels {
        &self.channels
    }

    /// 渲染节流决策：**呈现节奏与面板刷新率对齐**。
    ///
    /// 闸门放行之后按分支取目标上限：书写期 [`pacing::STROKE_MIN_HZ`]、直接操作
    /// 满 vsync、普通批注 `annotation_fps_cap`，都用跳帧档位实现。
    ///
    /// # 为什么必须自己做限幅
    ///
    /// 早先的注释称"直接操作时 vsync 本身就是节流器，额外机制只会添乱"。在当前
    /// Vulkan 后端这不成立——实测 `AutoVsync` 并未把帧率钉到 60Hz（60Hz 面板上跑到
    /// 74fps），所以 vsync 在这里**没有**充当节流器。
    ///
    /// # 曾经有一道「面板对齐闸门」，已删除
    ///
    /// 它对任何距上次 `draw` 不足一个面板周期的重绘请求返回
    /// `request_redraw_at(last + period)`。**实测它拦掉了 100% 的输入重绘**：
    ///
    /// ```text
    /// throttled=12  src=26/12/0/1    ← 输入触发 12 次，延后 12 次
    /// throttled=5   src=30/6/0/1     ← 输入 6 次，延后 5 次
    /// throttled=2   src=31/2/0/1     ← 输入 2 次，延后 2 次
    /// ```
    ///
    /// 也就是说每一个笔尖事件都被硬加最多 16.7ms 延迟——正是"跟不上手"。它当初的
    /// 理由是"本机稳定跑到 70~100fps，每秒 10~40 帧必然被显示端丢掉"。**那个数字
    /// 是错的**：它数的是 `Program::draw` 的调用次数，而 iced 的 canvas widget 在
    /// **layout 阶段**就调 `draw` 构建几何，不是每次呈现才调一次。在唯一呈现计数点
    /// （`iced_winit` 成功 `present` 之后）重新测量，真实呈现速率是：
    ///
    /// ```text
    /// 呈现/秒  min=25.3  中位=37.7  max=57.6     （60Hz 面板，从不超 60）
    /// layout/秒 中位=74
    /// ```
    ///
    /// **呈现从来没有超过面板刷新率**，所以这道闸门拦的是一个不存在的浪费，代价却
    /// 是全部输入的延迟。那 2.5 倍的 layout/呈现差值仍然存在（74 vs 38），但它不是
    /// "多画的帧"，而是同一帧内的重复重建——要省它得改"提交时机"，不是加闸门。
    pub fn paced_redraw(&self, input: RedrawInputs) -> Redraw {
        let refresh_hz = self.measured_refresh_hz();
        viewink::stats::set_redraw_source(viewink::stats::RedrawSource::Input);

        // 各分支的目标帧率上限。
        let skip = if input.stroke_rendering {
            pacing::stroke_skip(refresh_hz, pacing::STROKE_MIN_HZ)
        } else if input.direct_manipulation {
            return Redraw::Now;
        } else {
            pacing::skip_count(refresh_hz, input.annotation_fps_cap)
        };
        if skip == 0 {
            return Redraw::Now;
        }
        // 节流：跳 `skip` 个 vsync。唤醒点取 vsync 区间正中。
        let wake_us = pacing::throttle_wake_offset_us(refresh_hz, skip);
        let wake_at = self.metrics.last_frame_start() + std::time::Duration::from_micros(wake_us);
        let now = Instant::now();
        if now >= wake_at {
            return Redraw::Now;
        }
        viewink::stats::add_throttled_frame();
        Redraw::At(wake_at)
    }

    /// 当前**有效**的帧量化周期（毫秒）：帧内容实际上屏的间隔。
    ///
    /// 这是笔尖外推的预测时域，必须反映**节流后的**节奏而不是面板刷新率：144Hz
    /// 面板上跳 1 个 vsync → 实际 72fps → 量化周期 13.9ms。若仍按面板的 6.9ms 外推，
    /// 就只补了一半延迟，笔尖照样落后指针。
    ///
    /// 未测得刷新率时返回 0.0（调用方应视为"不外推"，保守退化）。
    pub fn effective_frame_period_ms(&self, stroke_rendering: bool) -> f32 {
        pacing::effective_period_ms(
            self.measured_refresh_hz(),
            stroke_rendering,
            pacing::STROKE_MIN_HZ,
        )
        .unwrap_or(0.0)
    }

    /// 实测面板刷新率（Hz）。未测得时返回 0.0（此时不节流、不外推）。
    ///
    /// 数据源是**呈现**周期估计，由 `iced_winit` 在成功 `present` 之后回调喂入。
    /// **不能**用 `Program::draw` 的调用间隔：iced 的 canvas widget 在 layout 阶段
    /// 就调 `draw`，那不是"呈现了一帧"。实测 layout 74/s 而呈现 37.7/s——用错数据
    /// 源会让笔尖外推的量化周期短一半（笔尖落后指针），也会让跳帧档位算错。
    pub fn measured_refresh_hz(&self) -> f64 {
        pacing::measured_refresh_hz()
    }

    /// 帧驱动步进（`canvas::Program::update` 的 `RedrawRequested` 分支调用）。
    ///
    /// 摄像头网格见 [`CameraGrid::step`]。
    pub fn step_camera_grid(&self, frozen: bool, frame_rate_mode: &str) -> Option<Instant> {
        self.grid.step(frozen, frame_rate_mode)
    }
}

impl Default for FrameDriver {
    fn default() -> Self {
        Self::new()
    }
}
