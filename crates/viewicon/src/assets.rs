//! 随 crate 分发的 SVG 资源（编译期内嵌，运行时零 IO）。
//!
//! 这些文件与解析器/栅格化器同属一个 crate，所以由这里用 `include_str!` 暴露成
//! `&'static str`。应用侧的图标注册表引用这些常量，而**不**自己写跨目录的
//! `include_str!` 路径——那种路径会在目录层级调整时静默失效，且每个消费方都要
//! 重复一遍。
//!
//! 新增图标：把 `.svg` 放进 `assets/icons/`，在此加一条 `include_str!` 常量。

/// 相机。
pub const CAMERA: &str = include_str!("../assets/icons/camera.svg");
/// 关闭（资源文件名为 `dismiss`，`assets` 中无 `close.svg`）。
pub const DISMISS: &str = include_str!("../assets/icons/dismiss.svg");
/// 橡皮擦。
pub const ERASER: &str = include_str!("../assets/icons/eraser.svg");
/// 齿轮。
pub const GEAR: &str = include_str!("../assets/icons/gear.svg");
/// 图片。
pub const IMAGE: &str = include_str!("../assets/icons/image.svg");
/// 补光灯（关）。
pub const LIGHT_OFF: &str = include_str!("../assets/icons/light-off.svg");
/// 补光灯（开）。
pub const LIGHT: &str = include_str!("../assets/icons/light.svg");
/// 菜单。
pub const MENU: &str = include_str!("../assets/icons/menu.svg");
/// 最小化。
pub const MINIMIZE: &str = include_str!("../assets/icons/minimize.svg");
/// 移动。
pub const MOVE: &str = include_str!("../assets/icons/move.svg");
/// 画笔。
pub const PEN: &str = include_str!("../assets/icons/pen.svg");
/// 设置。
pub const SETTINGS: &str = include_str!("../assets/icons/settings.svg");
/// 撤销。
pub const UNDO: &str = include_str!("../assets/icons/undo.svg");
