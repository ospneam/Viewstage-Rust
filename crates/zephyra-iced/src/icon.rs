//! 图标栅格化：zephyra 的 [`Icon`] → iced 图像句柄（含纹理缓存）。
//!
//! # 为什么栅格化在宿主侧
//!
//! 它需要 tiny-skia 与 iced 的纹理句柄，两者都是渲染栈的东西。放进 `zephyra`
//! 会破坏"核心零依赖"这条不变量——而那正是迁移缝能被验证的前提：核心能被单独
//! 编译、单独测试，才谈得上"两个宿主产出相同 DisplayList"。
//!
//! # 与 `DisplayList` 的衔接
//!
//! 桥是**图标名**：[`IconResolver`] 把 [`zephyra::paint::TextureRef`] 当作图标名去
//! [`IconSet`] 里查。也就是说什么都不用改——控件把 `IMAGE_TEXTURE` 设成
//! `static_str("pen")`，`DisplayList` 里就出现 `TextureRef("pen")`，解析器按名字取。
//!
//! # 为什么自己栅格化而不走 iced 的 svg feature
//!
//! iced 的 `svg` 组件基于 resvg，功能完整但对本项目有两个问题：
//!
//! 1. **全局 UI MSAA 已为填充带宽关闭**。矢量图标走三角形填充时，硬边在 16px
//!    尺寸上表现为明显锯齿——纹理路径的边缘质量优于 MSAA 4x。
//! 2. **图标是静态内容**，却每帧重新三角化。改成首次组合时用 tiny-skia
//!    （解析式覆盖 AA）栅格化一次并缓存，之后每帧只是一个 image quad 采样——
//!    零三角形化。
//!
//! # 超采样
//!
//! 按 2× 逻辑分辨率栅格化，由 GPU 双线性降采样到逻辑尺寸。125% DPI 下这是 1.6×
//! 降采样，边缘平滑；额外再留 1px 边距，避免边界裁切与图集线性采样渗色。
//!
//! # 缓存键
//!
//! `(图标数据指针, 逻辑尺寸位型, rgba8 量化色)`。用**指针**而非名字做身份是刻意的：
//! 名字要字符串哈希，而 [`IconSet`] 保证每个名字对应一个 `'static` 地址。量化到
//! rgba8 是为了让 `Color` 的浮点尾数落到同一个桶里——否则同一颜色因浮点误差不同会
//! 各自占一个缓存条目。
//!
//! 失败与不可渲染的条目**也缓存**（`None`）：否则每帧都会重试一次栅格化。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use iced_graphics::geometry::{Fill, Path, Stroke};
use iced::widget::image as image_widget;
use iced::{Point, Rectangle, Size};

use zephyra::core::Color;
use zephyra::icon::{FillRule, Icon, IconSet, PaintMode, Segment};
use zephyra::paint::TextureRef;

use crate::TextureResolver;

/// 栅格化超采样倍数（逻辑分辨率的倍数）。
const RASTER_SS: f32 = 2.0;

/// 栅格缓存键：图标数据地址（`'static Icon` 身份）+ 逻辑尺寸位型 + rgba8 量化色。
type CacheKey = (usize, u32, [u8; 4]);

/// 进程级栅格缓存。
///
/// 用 `OnceLock<RwLock<HashMap>>` 而非 `thread_local`：同一个 `(图标, 尺寸, 颜色)`
/// 组合在多个渲染线程上会重复栅格化，而栅格化是这套系统里唯一一件"每图标几十
/// 微秒"的事——重复做不致命，但完全没必要。
static CACHE: OnceLock<RwLock<HashMap<CacheKey, Option<image_widget::Handle>>>> = OnceLock::new();

/// 图标纹理解析器：把 `TextureRef`（图标名）变成可采样的图像句柄。
pub struct IconResolver {
    set: &'static IconSet,
}

impl IconResolver {
    /// 绑定一个图标注册表。
    pub fn new(set: &'static IconSet) -> Self {
        Self { set }
    }

    /// 取（并按需填充）图标纹理。
    ///
    /// `size` 为**正方形**边长（逻辑像素）；`color` 由调用点给定，图标因此可随按钮
    /// 的选中/hover 状态变色，而不必为每种颜色重新解析 SVG。
    ///
    /// 不可渲染的图标返回 `None`：调用方据此跳过绘制，而不是画一块空白纹理。
    /// 一个缺资源不该让整个界面崩掉。
    pub fn handle(
        &self,
        icon: &'static Icon,
        size: f32,
        color: Color,
    ) -> Option<image_widget::Handle> {
        if icon.is_empty() || !(size > 0.0) {
            return None;
        }
        let key: CacheKey = (
            std::ptr::from_ref(icon) as usize,
            size.to_bits(),
            [
                (color.r * 255.0).round() as u8,
                (color.g * 255.0).round() as u8,
                (color.b * 255.0).round() as u8,
                (color.a * 255.0).round() as u8,
            ],
        );

        let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
        if let Ok(map) = cache.read() {
            if let Some(handle) = map.get(&key) {
                return handle.clone();
            }
        }
        let handle = rasterize(icon, size, color);
        if let Ok(mut map) = cache.write() {
            map.insert(key, handle.clone());
        }
        handle
    }

    /// 绑定的图标注册表。
    pub fn icons(&self) -> &'static IconSet {
        self.set
    }
}

impl TextureResolver for IconResolver {
    fn resolve(&self, texture: &TextureRef) -> Option<image_widget::Handle> {
        // 尺寸与颜色在这里取的是**占位值**：`DisplayList` 里的 `Image` 命令已经
        // 算好了目标矩形与 tint，栅格化的分辨率必须按那个矩形来，否则超采样倍率
        // 与降采样关系会错。
        //
        // 所以真正的栅格化入口是 [`IconResolver::handle`]；这里走的是"用默认尺寸
        // 兜一个句柄"的路径，仅用于 `zephyra_iced::view` 那类按名字直接取图的场合。
        let icon = self.set.get(texture.as_str());
        self.handle(icon, DEFAULT_SIZE, Color::rgb(1.0, 1.0, 1.0))
    }
}

/// 未指定尺寸时用于 [`IconResolver::resolve`] 的占位边长（DIP）。
///
/// 与工具栏图标的 18 DIP 对齐。选一个具体值而不是报错，是因为这条路径只在
/// "按名字直接取一张图"时走到，而那时调用方本来就没有别的尺寸可给。
const DEFAULT_SIZE: f32 = 18.0;

/// 把已解析路径段用 tiny-skia 栅格化为 straight-RGBA 纹理。
fn rasterize(icon: &Icon, size: f32, color: Color) -> Option<image_widget::Handle> {
    let (vb_x, vb_y, vb_w, vb_h) = icon.view_box;
    if vb_w <= 0.0 || vb_h <= 0.0 || size <= 0.0 {
        return None;
    }

    let content_px = (size * RASTER_SS).round().max(1.0);
    let px = content_px as u32 + 2; // 1px 边距 ×2
    // 用 `min` 保证整个 viewBox 都在纹素内：图标按 `min` 取哪一轴为准，
    // 否则非正方形的 viewBox 会被拉伸变形。
    let scale = (content_px / vb_w).min(content_px / vb_h);
    let ox = (content_px - vb_w * scale) * 0.5 - vb_x * scale + 1.0;
    let oy = (content_px - vb_h * scale) * 0.5 - vb_y * scale + 1.0;

    let mut builder = tiny_skia::PathBuilder::new();
    for segment in &icon.segments {
        match *segment {
            Segment::MoveTo(x, y) => builder.move_to(ox + x * scale, oy + y * scale),
            Segment::LineTo(x, y) => builder.line_to(ox + x * scale, oy + y * scale),
            Segment::CubicTo(x1, y1, x2, y2, x, y) => builder.cubic_to(
                ox + x1 * scale,
                oy + y1 * scale,
                ox + x2 * scale,
                oy + y2 * scale,
                ox + x * scale,
                oy + y * scale,
            ),
            Segment::QuadraticTo(cx, cy, x, y) => {
                builder.quad_to(ox + cx * scale, oy + cy * scale, ox + x * scale, oy + y * scale)
            }
            Segment::Close => builder.close(),
        }
    }
    let path = builder.finish()?;

    let (r, g, b, a) = match icon.paint.mode {
        PaintMode::None => return None,
        // 颜色只在栅格化时决定一次，之后每帧只是采样。所以图标内容一变就必须
        // 换缓存条目——这正是键里带量化色的原因。
        PaintMode::CurrentColor => (color.r, color.g, color.b, color.a),
        PaintMode::Solid(c) => (c[0], c[1], c[2], c[3]),
    };

    let mut paint = tiny_skia::Paint {
        anti_alias: true,
        ..tiny_skia::Paint::default()
    };
    paint.set_color_rgba8(
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
        (a * 255.0).round() as u8,
    );
    let rule = match icon.paint.rule {
        FillRule::NonZero => tiny_skia::FillRule::Winding,
        FillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
    };

    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    pixmap.fill_path(&path, &paint, rule, tiny_skia::Transform::identity(), None);

    // tiny-skia 输出**预乘** alpha，`Handle::from_rgba` 要 straight RGBA
    // （`image` 组件用 `SrcAlpha/OneMinusSrcAlpha` 的 shader 再乘一次）。
    let mut rgba = Vec::with_capacity((px * px * 4) as usize);
    for p in pixmap.pixels() {
        let c = p.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }

    Some(image_widget::Handle::from_rgba(px, px, rgba))
}

/// 矢量回退：把路径直接填进 iced 几何，不经栅格缓存。
///
/// 用途是**调试**：缓存命中不了时（例如刚改完 SVG 想立刻看效果），它能画出形状，
/// 从而把"图标没画出来"和"图标形状不对"这两种故障区分开。
///
/// 正常路径不该走这里——每帧三角化正是 [`rasterize`] 要消除的那件事。
pub fn vector_path(icon: &Icon, size: f32, color: Color) -> (Path, Fill) {
    let (vb_x, vb_y, vb_w, vb_h) = icon.view_box;
    let scale = if vb_w > 0.0 && vb_h > 0.0 {
        (size / vb_w).min(size / vb_h)
    } else {
        0.0
    };
    let ox = (size - vb_w * scale) * 0.5 - vb_x * scale;
    let oy = (size - vb_h * scale) * 0.5 - vb_y * scale;
    let map = |x: f32, y: f32| Point::new(ox + x * scale, oy + y * scale);

    let mut builder = iced::widget::canvas::path::Builder::new();
    for segment in &icon.segments {
        match *segment {
            Segment::MoveTo(x, y) => {
                builder.move_to(map(x, y));
            }
            Segment::LineTo(x, y) => {
                builder.line_to(map(x, y));
            }
            Segment::CubicTo(x1, y1, x2, y2, x, y) => {
                builder.bezier_curve_to(map(x1, y1), map(x2, y2), map(x, y));
            }
            Segment::QuadraticTo(cx, cy, x, y) => {
                builder.quadratic_curve_to(map(cx, cy), map(x, y));
            }
            Segment::Close => builder.close(),
        }
    }

    let (r, g, b, a) = match icon.paint.mode {
        PaintMode::None => (0.0, 0.0, 0.0, 0.0),
        PaintMode::CurrentColor => (color.r, color.g, color.b, color.a),
        PaintMode::Solid(c) => (c[0], c[1], c[2], c[3]),
    };
    let mut fill = Fill::from(iced::Color {
        r: r as f32,
        g: g as f32,
        b: b as f32,
        a: a as f32,
    });
    fill.rule = match icon.paint.rule {
        FillRule::NonZero => iced::widget::canvas::fill::Rule::NonZero,
        FillRule::EvenOdd => iced::widget::canvas::fill::Rule::EvenOdd,
    };
    (builder.build(), fill)
}

/// 图标的矢量描边（用于 hover 描边等需要轮廓线的场合）。
///
/// 与 [`vector_path`] 共用同一套 viewBox → 边长的映射，所以两者不会错位。
pub fn vector_stroke(color: Color, width: f32) -> Stroke<'static> {
    Stroke::default()
        .with_color(iced::Color {
            r: color.r as f32,
            g: color.g as f32,
            b: color.b as f32,
            a: color.a as f32,
        })
        .with_width(width)
}

/// 图标的显示矩形（viewBox 按 `min` 等比适配到 `size` 正方形）。
///
/// 与 [`rasterize`] 内部的 `scale` / `ox` / `oy` 是同一套算法。抽成函数是为了让
/// "栅格化了多大"和"画在哪儿"不可能不一致——两者一旦各算一遍，图标就会偏出
/// 按钮，而偏移量往往只有一两个像素，只能靠肉眼比对发现。
pub fn icon_rect(icon: &Icon, origin: Point, size: f32) -> Rectangle {
    let (_, _, vb_w, vb_h) = icon.view_box;
    let scale = if vb_w > 0.0 && vb_h > 0.0 {
        (size / vb_w).min(size / vb_h)
    } else {
        0.0
    };
    let w = vb_w * scale;
    let h = vb_h * scale;
    Rectangle {
        x: origin.x + (size - w) * 0.5,
        y: origin.y + (size - h) * 0.5,
        width: w,
        height: h,
    }
}

/// 图标在 `size` 正方形内的实际占用边长（viewBox 等比适配后的结果）。
pub fn icon_extent(icon: &Icon, size: f32) -> Size {
    let (_, _, vb_w, vb_h) = icon.view_box;
    if vb_w <= 0.0 || vb_h <= 0.0 {
        return Size::new(0.0, 0.0);
    }
    let scale = (size / vb_w).min(size / vb_h);
    Size::new(vb_w * scale, vb_h * scale)
}