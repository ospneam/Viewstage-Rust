//! viewink —— 笔迹（手写/标注）引擎。
//!
//! 与前端（iced widget、指针事件、订阅 worker）彻底解耦，只承载：
//! - `ink`：笔画数据模型、笔锋镶嵌算法、GPU 线段网格构建、撤销历史；
//! - `live`：渲染层与 App 之间的实时共享状态（分层渲染通道）；
//! - `render`：批注层独立 wgpu 渲染管线（GPU 胶囊扩展，annotation.wgsl）；
//! - `gpu`：批注/摄像头共用的 GPU 原语（线段表、视图变换、缓冲辅助）；
//! - `stats`：渲染统计。
//!
//! 主包 `viewstage` 通过 `viewink::*` 调用上述能力，自身只保留事件编排与 UI。

pub mod gpu;
pub mod ink;
pub mod live;
pub mod render;
pub mod stats;

// 便于调用方以 viewink::annotation / viewink::live / viewink::gpu 等短路径引用
pub use render::annotation;

// 高频类型的便捷再导出，减少调用方 `use` 路径长度
pub use gpu::{Globals, StrokeGeometry, StrokeSeg, ViewTransform};
pub use ink::history::{DrawCommand, HistoryManager, UndoneCommand};
pub use ink::stroke::{DrawStroke, StrokeBounds, StrokePoint, StrokeType};
pub use live::{EraserCursor, LiveCanvasState, LiveTransform};
pub use render::annotation::{AnnotationLayer, AnnotationPipeline, StaticStrokeRef};
