//! # viewcore-touch
//!
//! ViewStage 手势与坐标核心：指针事件 → 手势识别 → 坐标变换 → 惯性积分。
//!
//! ## 性能设计预算（高性能 / 高帧率 / 低功耗 / 低开销）
//!
//! - **零堆分配**：热路径无 `Vec` / `HashMap` / `String` / `clone`。事件、识别输出、
//!   帧批处理全部是固定容量 `Copy` 值，栈上完成。
//! - **固定内存**：指针表为 8 槽定长数组，O(8) 线性扫描替代哈希表——无哈希计算、
//!   无 rehash、缓存友好、分支可预测。
//! - **单入口状态机**：每个 OS 指针事件恰好一次 [`Recognizer::on_pointer`] 调用，
//!   内部一次完成指针表更新 + 捏合识别，无中间事件向量。
//! - **f32 全程**：与渲染 / GPU 同宽，消除旧实现逐事件的 f64↔f32 转换。
//! - **平方距离判据**：捏合激活容差在平方域判定，激活前 0 次 `sqrt`；
//!   每个实际产出的增量仅 1 次 `sqrt`。
//! - **无系统调用**：移除旧实现只写不读的死状态（曾导致每次 Down/Up 各一次
//!   `SystemTime::now()`）。
//! - **静止零开销**：惯性未激活时 [`Momentum::tick`] 立即返回 `None`（不做任何
//!   数学运算）；低于停止阈值立即收敛，不留长尾空转（省电）。
//! - **帧率无关**：惯性摩擦按实际帧间隔折算（基准 60Hz/16ms），
//!   60 / 120 / 144Hz 手感一致，掉帧不忽快忽慢。
//!
//! ## 模块
//!
//! - [`pointer`]：事件词汇表 + 固定容量指针表
//! - [`recognizer`]：单入口手势状态机（指针表 + 两指捏合 V2 增量语义）
//! - [`transform`]：屏幕 ↔ 画布坐标变换 + 捏合锚点应用
//! - [`momentum`]：帧率无关惯性

#![forbid(unsafe_code)]

pub mod momentum;
pub mod pinch;
pub mod pointer;
pub mod recognizer;
pub mod transform;

pub use momentum::Momentum;
pub use pinch::PinchDelta;
pub use pointer::{PointerEvent, PointerPhase, PointerSource};
pub use recognizer::{Gesture, Recognizer};
pub use transform::ScreenTransform;
