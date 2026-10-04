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
//! 1. **节拍标定无法回归测试。** 跳帧档位、量化周期、EMA 抗尖峰这些决策
//!    全部是纯函数，却因为寄生在 `App` 上而只能靠真机跑起来看 HUD 数字。
//! 2. **帧热路径与业务状态纠缠。** `canvas::Program::draw` 里有 130 行是
//!    渲染记账，却和「用户点了哪个按钮」共用同一个结构体。
//!
//! ## 职责边界
//!
//! - 本 crate：**决策**——什么时候重绘、跳几个 vsync、量化周期多长、上屏
//!   时刻落在哪个格点。全部是纯计算或 `Cell`/`RefCell` 状态。
//! - `viewink` / `viewgfx`：几何与图元（镶嵌、快照、轨迹）。
//! - `viewcamera`：像素与采集。
//! - 本 crate **不持有任何 GPU 设备对象**——通道注册只是把别人造好的
//!   `Box<dyn DirectChannel>` 交给 `iced_wgpu`。
//!
//! ## 热路径约束
//!
//! `FrameDriver` 的全部方法仅 `&self`（内部 `Cell`/`RefCell`/`Arc`），
//! 与 `canvas::Program` 的零消息签名兼容。稳态下禁止分配。

pub mod display;
pub mod pacing;

pub use display::refresh_hz;
pub use pacing::{
    effective_period_ms, measured_refresh_hz, note_present, panel_period, skip_count,
    stroke_skip, throttle_wake_offset_us, PeriodEstimate, STROKE_MIN_HZ,
    STREAM_CONTINUITY_GAP, WORK_ESTIMATE_US,
};
