//! 摄像头处理与连接引擎。
//!
//! 负责摄像头相关的「处理与连接」：设备连接（nokhwa 原生 NV12 直传 / RGB 回退）、
//! 帧处理（NV12 平面零拷贝切片、RGB→RGBA 降采样、拍照 NV12→RGBA）、USB HID 补光灯
//! 控制，以及摄像头/图片背景层的独立 wgpu 渲染管线（含着色器）。
//!
//! 窗口合成（App / iced 编排、取帧/拍照 worker、消息契约、App 字段与 handler）仍留在
//! 主 crate `viewstage`，通过本 crate 的公共 API 调用。

pub mod capture;
pub mod gpu_processor;
pub mod light;
pub mod render;

// 与主 crate 的引用路径对齐：摄像头渲染管线在 render 子树下，
// 但同时以 viewcamera::camera 暴露，避免主 crate 里出现嵌套替换歧义。
pub use render::camera;
