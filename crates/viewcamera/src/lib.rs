//! 摄像头处理与连接引擎。
//!
//! 只保留**没有**搬去 zephyra-camera 的那部分：USB HID 补光灯控制，以及摄像头/
//! 图片背景层的 wgpu 渲染管线。
//!
//! 已迁走的：
//! - 设备采集（nokhwa）与帧处理（NV12 平面切片、RGB→RGBA 降采样、拍照转换）
//!   -> `zephyra-camera` 子包，feature 选择性编译，见 crates/zephyra-camera/src/lib.rs
//!
//! 留下的理由：
//! - 补光灯是 Windows 专有、且针对特定厂商硬件的私有协议，既不是框架词汇也不是
//!   通用能力；
//! - 渲染管线与笔迹渲染共用同一份每帧状态（`viewink::gpu::Globals` /
//!   `ViewTransform` / `ColorPalette`），搬走就得连带搬走那套共享状态。
//!
//! 窗口合成（App / iced 编排、取帧/拍照 worker、消息契约、App 字段与 handler）仍留在
//! 主 crate `viewstage`，通过本 crate 的公共 API 调用。

pub mod light;
pub mod render;

// 与主 crate 的引用路径对齐：摄像头渲染管线在 render 子树下，
// 但同时以 viewcamera::camera 暴露，避免主 crate 里出现嵌套替换歧义。
pub use render::camera;
