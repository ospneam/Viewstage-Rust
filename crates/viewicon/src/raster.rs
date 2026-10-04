//! 图标栅格化：已解析路径 → GPU 纹理（含纹理缓存）。
//!
//! # 为什么自己栅格化而不走 iced 的 svg feature
//!
//! iced 的 `svg` 组件基于 resvg，功能完整但对本项目有两个问题：
//!
//! 1. **全局 UI MSAA 已为填充带宽关闭**（见 `viewframe::boot::settings`）。
//!    矢量图标走三角形填充时硬边在 16px 尺寸上表现为明显锯齿——纹理路径的边缘
//!    质量优于 MSAA 4x。
//! 2. **图标是静态内容**，却每帧重新三角化。改成首次组合时用 tiny-skia
//!    （解析式覆盖 AA）栅格化一次并缓存，之后每帧只是一个 image quad 采样——
//!    零三角形化。
//!
//! # 超采样
//!
//! 按 2× 逻辑分辨率栅格化，由 GPU 双线性降采样到逻辑尺寸。125% DPI 下这是
//! 1.6× 降采样，边缘平滑；额外再留 1px 边距，避免边界裁切与图集线性采样渗色。
//!
//! # 缓存键
//!
//! `(icon 数据指针, 逻辑尺寸位型, rgba8 量化色)`。用**指针**而非名字做身份是
//! 刻意的：名字要字符串哈希，而 `'static Icon` 的地址直接就是稳定身份。量化到
//! rgba8 是为了让 `Color` 的浮点误差落到同一个桶里——否则同一颜色因浮点尾数
//! 不同会各自占一个缓存条目。
//!
//! 失败与不可渲染的条目**也缓存**（`None`）：否则每帧都会重试一次栅格化。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use iced::widget::{canvas, image as image_widget};
use iced::{Color, Element, Point, Rectangle, Renderer, Theme};
use tiny_skia;

use crate::{FillRule, Icon, PaintMode, Segment};

/// 栅格化超采样倍数（逻辑分辨率的倍数）。
const RASTER_SS: f32 = 2.0;

/// 栅格缓存键：icon 数据指针（`'static Icon` 身份）+ 逻辑尺寸位型 + rgba8 量化色。
type CacheKey = (usize, u32, [u8; 4]);

static CACHE: OnceLock<RwLock<HashMap<CacheKey, Option<image_widget::Handle>>>> = OnceLock::new();

/// 取（并按需填充）图标纹理。
///
/// 进程内每个 `(icon, size, color)` 组合只栅格化一次。
///
/// **参数是 `&'static Icon` 而不是 `&Icon`**：缓存键取的是图标数据的地址，
/// 而非 `'static` 的图标可能位于栈上——函数返回后地址被复用，另一个图标就会
/// 命中它的缓存条目。静态内容配静态身份，这条不变量写进类型里。
pub fn handle(data: &'static Icon, size: f32, color: Color) -> Option<image_widget::Handle> {
    let key: CacheKey = (
        std::ptr::from_ref(data) as usize,
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
    let handle = rasterize(data, size, color);
    if let Ok(mut map) = cache.write() {
        map.insert(key, handle.clone());
    }
    handle
}

/// 渲染图标元素：纹理命中走 `image` quad，否则回退到 canvas 路径填充。
///
/// `size` 为正方形边长（逻辑像素）；`color` 由调用点给定，图标因此可随按钮的
/// 选中/hover 状态变色，而不是硬编码死色。
///
/// 回退路径覆盖两种情况：`fill="none"`（无笔画可填充）与栅格化失败（返回
/// `None`）。两者都**不 panic**——一个缺资源不该让整个界面崩掉。
pub fn element<'a, Message: 'a>(
    data: &'static Icon,
    size: f32,
    color: Color,
) -> Element<'a, Message> {
    if let Some(handle) = handle(data, size, color) {
        // FilterMethod 默认 Linear：2x 栅格 → 逻辑尺寸由 GPU 双线性降采样
        return image_widget(handle).width(size).height(size).into();
    }
    canvas(IconCanvas { icon: data, color })
        .width(size)
        .height(size)
        .into()
}

/// 把已解析路径段用 tiny-skia 栅格化为 straight-RGBA 纹理。
fn rasterize(icon: &Icon, size: f32, color: Color) -> Option<image_widget::Handle> {
    let (vb_x, vb_y, vb_w, vb_h) = icon.view_box;
    if vb_w <= 0.0 || vb_h <= 0.0 || size <= 0.0 {
        return None;
    }
    match icon.paint.mode {
        PaintMode::None => return None,
        PaintMode::CurrentColor => {}
        PaintMode::Solid(_) => {}
    }

    let content_px = (size * RASTER_SS).round().max(1.0);
    let px = content_px as u32 + 2; // 1px 边距 ×2
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

/// canvas 渲染兜底：把路径直接填充（覆盖 `fill="none"` 之外的所有栅格化失败）。
///
/// 与 [`rasterize`] 的区别是缩放基准不同——这里按 canvas `bounds` 适配，因此
/// 同样支持非零 `viewBox` 的 `min-x`/`min-y` 偏移。
struct IconCanvas {
    icon: &'static Icon,
    color: Color,
}

impl<Message> canvas::Program<Message> for IconCanvas {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());

        let (vb_x, vb_y, vb_w, vb_h) = self.icon.view_box;
        if vb_w > 0.0 && vb_h > 0.0 {
            let scale = (bounds.width / vb_w).min(bounds.height / vb_h);
            let ox = (bounds.width - vb_w * scale) * 0.5 - vb_x * scale;
            let oy = (bounds.height - vb_h * scale) * 0.5 - vb_y * scale;
            let map = |x: f32, y: f32| Point::new(ox + x * scale, oy + y * scale);

            let mut builder = canvas::path::Builder::new();
            for segment in &self.icon.segments {
                match *segment {
                    Segment::MoveTo(x, y) => builder.move_to(map(x, y)),
                    Segment::LineTo(x, y) => builder.line_to(map(x, y)),
                    Segment::CubicTo(x1, y1, x2, y2, x, y) => {
                        builder.bezier_curve_to(map(x1, y1), map(x2, y2), map(x, y))
                    }
                    Segment::QuadraticTo(cx, cy, x, y) => {
                        builder.quadratic_curve_to(map(cx, cy), map(x, y))
                    }
                    Segment::Close => builder.close(),
                }
            }
            let path = builder.build();

            let fill_color = match self.icon.paint.mode {
                PaintMode::None => return vec![frame.into_geometry()],
                PaintMode::CurrentColor => self.color,
                PaintMode::Solid(c) => Color {
                    r: c[0],
                    g: c[1],
                    b: c[2],
                    a: c[3],
                },
            };
            let mut fill = canvas::Fill::from(fill_color);
            fill.rule = match self.icon.paint.rule {
                FillRule::NonZero => canvas::fill::Rule::NonZero,
                FillRule::EvenOdd => canvas::fill::Rule::EvenOdd,
            };
            frame.fill(&path, fill);
        }

        vec![frame.into_geometry()]
    }
}
