#![forbid(unsafe_code)]
//! # viewicon
//!
//! 零依赖的 SVG 图标解析器。只覆盖"图标级" SVG（`viewBox` + 若干 `<path>`），
//! 不做完整 XML 解析，产出纯数据 [`Icon`]，由渲染端（iced canvas 管线等）消费。
//!
//! - [`path::parse_path_data`]：`d` 属性全命令集（`M m L l H h V v C c S s Q q T t A a Z z`），
//!   含隐式命令重复与全部数字字面格式；椭圆弧按 SVG 规范附录 F.6 从端点参数化
//!   转为中心参数化，再展开为圆心角 ≤ 90° 的三次贝塞尔段。
//! - [`svg::parse_svg`]：从 SVG 文本提取 `viewBox` / `fill` / `fill-rule`，
//!   合并全部 `<path>` 段（跳过 `fill="none"`）。
//!
//! 设计约束：解析一次、渲染端每帧零解析；`Segment` 全部 `Copy`，
//! 热路径（渲染端逐段遍历）无堆分配。

mod path;
mod svg;

pub use path::{parse_path_data, ParseError};
pub use svg::{parse_svg, SvgError};

/// 一条已展平的路径指令。曲线只保留三次/二次贝塞尔两种形式，弧已离线展开。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    /// 开启新子路径
    MoveTo(f32, f32),
    /// 直线到绝对坐标
    LineTo(f32, f32),
    /// 三次贝塞尔：c1x c1y c2x c2y x y（均为绝对坐标）
    CubicTo(f32, f32, f32, f32, f32, f32),
    /// 二次贝塞尔：cx cy x y（均为绝对坐标）
    QuadraticTo(f32, f32, f32, f32),
    /// 闭合当前子路径（当前点回到子路径起点）
    Close,
}

/// 填充规则（对应 SVG `fill-rule`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// 填充模式（对应 SVG `fill`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaintMode {
    /// `currentColor` 或未指定：渲染端给定颜色
    CurrentColor,
    /// 具体颜色（RGBA，0..1）
    Solid([f32; 4]),
    /// `fill="none"`：该元素不参与填充
    None,
}

/// 填充样式：模式 + 规则。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Paint {
    pub mode: PaintMode,
    pub rule: FillRule,
}

impl Paint {
    pub const CURRENT_COLOR: Paint = Paint {
        mode: PaintMode::CurrentColor,
        rule: FillRule::NonZero,
    };
}

impl Default for Paint {
    fn default() -> Self {
        Self::CURRENT_COLOR
    }
}

/// 解析后的图标：viewBox + 填充样式 + 展平路径段。
#[derive(Debug, Clone, PartialEq)]
pub struct Icon {
    /// (min_x, min_y, width, height)
    pub view_box: (f32, f32, f32, f32),
    pub paint: Paint,
    pub segments: Vec<Segment>,
}

impl Icon {
    /// 空 图标（viewBox 24×24，无路径段），可作 const 兜底值。
    pub const EMPTY: Icon = Icon {
        view_box: (0.0, 0.0, 24.0, 24.0),
        paint: Paint::CURRENT_COLOR,
        segments: Vec::new(),
    };
}
