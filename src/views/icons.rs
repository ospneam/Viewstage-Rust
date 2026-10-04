//! 自研图标渲染管线：viewicon 解析 + tiny-skia 预栅格化纹理，替代 iced svg（resvg）组件。
//!
//! 资源分两类：
//! - `Svg`：`src/assets/icons/` 下的文件，`include_str!` 编译期内嵌——运行时零 IO、
//!   零文件监听，进程内只解析一次（OnceLock）；
//! - `Path`：暂无对应资源文件的图标，沿用原始 path data，走同一解析器渲染。
//!   后续补入 `src/assets/icons/<name>.svg` 后把条目改为 `Svg` 即可。
//!
//! 渲染由 [`icon`] 提供：icon 为静态内容，首次调用时按 2× 逻辑分辨率用
//! tiny-skia（解析式覆盖 AA）栅格化为纹理并缓存（name+size+color 维度），
//! 之后每帧只是一个 image quad 采样——零三角形化、零 MSAA 依赖。
//! 不走 canvas 几何填充的原因：全局 UI MSAA 已为填充带宽关闭（2026-09-27），
//! 三角形硬边在 icon 上表现为明显锯齿；纹理路径的边缘质量优于 MSAA 4x。
//! 颜色由调用点给定（可跟随按钮选中/hover 状态，而不再硬编码死色）。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use iced::widget::{canvas, image as image_widget};
use iced::{Color, Element, Point, Rectangle, Renderer, Theme};
use viewicon::{FillRule, Icon, PaintMode, Segment};

/// 图标源。
enum IconSource {
    /// `src/assets/icons/` 下的 SVG 文件内容（编译期内嵌）
    Svg(&'static str),
    /// 暂无对应文件，内嵌原始 path data（viewBox 按 0 0 24 24）
    Path(&'static str),
}

/// 名字 → 图标源注册表。代码中使用的名字必须在此登记。
const ICONS: &[(&str, IconSource)] = &[
    // ---- 资源文件（src/assets/icons）----
    ("camera", IconSource::Svg(include_str!("../assets/icons/camera.svg"))),
    // close → dismiss.svg（assets 中无 close.svg，dismiss 即关闭图标）
    ("close", IconSource::Svg(include_str!("../assets/icons/dismiss.svg"))),
    ("eraser", IconSource::Svg(include_str!("../assets/icons/eraser.svg"))),
    ("gear", IconSource::Svg(include_str!("../assets/icons/gear.svg"))),
    ("image", IconSource::Svg(include_str!("../assets/icons/image.svg"))),
    ("light", IconSource::Svg(include_str!("../assets/icons/light.svg"))),
    ("light-off", IconSource::Svg(include_str!("../assets/icons/light-off.svg"))),
    ("menu", IconSource::Svg(include_str!("../assets/icons/menu.svg"))),
    ("minimize", IconSource::Svg(include_str!("../assets/icons/minimize.svg"))),
    ("move", IconSource::Svg(include_str!("../assets/icons/move.svg"))),
    ("pen", IconSource::Svg(include_str!("../assets/icons/pen.svg"))),
    ("settings", IconSource::Svg(include_str!("../assets/icons/settings.svg"))),
    ("undo", IconSource::Svg(include_str!("../assets/icons/undo.svg"))),
    // ---- 暂无资源文件，沿用内嵌 path data（与旧版显示一致）----
    ("camera-fill", IconSource::Path("M13.9247 2.50391C14.7225 2.50391 15.4607 2.92638 15.8647 3.61428L16.6793 5.00086H18.75C20.5449 5.00086 22 6.45593 22 8.25086V17.7509C22 19.5458 20.5449 21.0009 18.75 21.0009H5.25C3.45507 21.0009 2 19.5458 2 17.7509V8.25086C2 6.45593 3.45507 5.00086 5.25 5.00086H7.33042L8.205 3.57656C8.61425 2.91006 9.34026 2.50391 10.1224 2.50391H13.9247ZM13.9247 4.00391H10.1224C9.89892 4.00391 9.6892 4.10337 9.54793 4.2717L9.48325 4.36146L8.38913 6.14331C8.25271 6.36547 8.01071 6.50086 7.75 6.50086H5.25C4.2835 6.50086 3.5 7.28436 3.5 8.25086V17.7509C3.5 18.7174 4.2835 19.5009 5.25 19.5009H18.75C19.7165 19.5009 20.5 18.7174 20.5 17.7509V8.25086C20.5 7.28436 19.7165 6.50086 18.75 6.50086H16.25C15.9841 6.50086 15.738 6.36003 15.6033 6.13073L14.5714 4.37403C14.4367 4.14473 14.1906 4.00391 13.9247 4.00391ZM12 8.00086C14.4853 8.00086 16.5 10.0156 16.5 12.5009C16.5 14.9861 14.4853 17.0009 12 17.0009C9.51472 17.0009 7.5 14.9861 7.5 12.5009C7.5 10.0156 9.51472 8.00086 12 8.00086ZM12 9.50086C10.3431 9.50086 9 10.844 9 12.5009C9 14.1577 10.3431 15.5009 12 15.5009C13.6569 15.5009 15 14.1577 15 12.5009C15 10.844 13.6569 9.50086 12 9.50086Z")),
    ("image-add", IconSource::Path("M17.75 3C19.5449 3 21 4.45507 21 6.25V11C21 11.4142 20.6642 11.75 20.25 11.75C19.8358 11.75 19.5 11.4142 19.5 11V6.25C19.5 5.2835 18.7165 4.5 17.75 4.5H6.25C5.2835 4.5 4.5 5.2835 4.5 6.25V17.75C4.5 17.9584 4.53643 18.1583 4.60326 18.3437L10.4258 12.643C11.2589 11.8273 12.5675 11.7885 13.4458 12.5266L13.5742 12.6431L15.8084 14.8267C16.1045 15.1165 16.1096 15.5914 15.8198 15.8875C15.53 16.1836 15.0551 16.1887 14.759 15.8989L12.5247 13.7148C12.2596 13.4553 11.8501 13.4316 11.5588 13.644L11.4752 13.7148L5.66845 19.4011C5.8504 19.4651 6.04613 19.5 6.25 19.5H14.25C14.6642 19.5 15 19.8358 15 20.25C15 20.6642 14.6642 21 14.25 21H6.25C4.45507 21 3 19.5449 3 17.75V6.25C3 4.45507 4.45507 3 6.25 3H17.75ZM20.25 13C20.6642 13 21 13.3358 21 13.75V16.5H23.75C24.1642 16.5 24.5 16.8358 24.5 17.25C24.5 17.6642 24.1642 18 23.75 18H21V20.75C21 21.1642 20.6642 21.5 20.25 21.5C19.8358 21.5 19.5 21.1642 19.5 20.75V18H16.75C16.3358 18 16 17.6642 16 17.25C16 16.8358 16.3358 16.5 16.75 16.5H19.5V13.75C19.5 13.3358 19.8358 13 20.25 13ZM15.2521 6.5C16.4959 6.5 17.5042 7.50831 17.5042 8.75212C17.5042 9.99592 16.4959 11.0042 15.2521 11.0042C14.0083 11.0042 13 9.99592 13 8.75212C13 7.50831 14.0083 6.5 15.2521 6.5ZM15.2521 8C14.8367 8 14.5 8.33673 14.5 8.75212C14.5 9.1675 14.8367 9.50423 15.2521 9.50423C15.6675 9.50423 16.0042 9.1675 16.0042 8.75212C16.0042 8.33673 15.6675 8 15.2521 8Z")),
    ("trash", IconSource::Path("M7.75012 3.5C7.75012 3.08579 8.08591 2.75 8.50012 2.75H15.5001C15.9143 2.75 16.2501 3.08579 16.2501 3.5C16.2501 3.91421 15.9143 4.25 15.5001 4.25H8.50012C8.08591 4.25 7.75012 3.91421 7.75012 3.5ZM4.50012 5.75C4.50012 5.33579 4.83591 5 5.25012 5H18.7501C19.1643 5 19.5001 5.33579 19.5001 5.75C19.5001 6.16421 19.1643 6.5 18.7501 6.5H5.25012C4.83591 6.5 4.50012 6.16421 4.50012 5.75ZM6.10869 7.5L6.58608 18.3293C6.63119 19.2567 7.39093 20 8.31929 20H15.6827C16.611 20 17.3708 19.2567 17.4159 18.3293L17.8933 7.5H16.3889L15.9126 18.2829C15.9004 18.5359 15.6888 18.7396 15.4355 18.7414L15.4075 18.7415H14.4091L14.4233 7.5H12.9234L12.9092 18.7415H11.0928L11.0786 7.5H9.57867L9.59285 18.7415H8.59448C8.3414 18.7397 8.13004 18.5362 8.11786 18.2834L7.61315 7.5H6.10869Z")),
    ("application", IconSource::Path("M4 3H20C20.5523 3 21 3.44772 21 4V8C21 8.55228 20.5523 9 20 9H4C3.44772 9 3 8.55228 3 8V4C3 3.44772 3.44772 3 4 3ZM4.5 4.5V7.5H19.5V4.5H4.5ZM4 11H20C20.5523 11 21 11.4477 21 12V20C21 20.5523 20.5523 21 20 21H4C3.44772 21 3 20.5523 3 20V12C3 11.4477 3.44772 11 4 11ZM4.5 12.5V19.5H19.5V12.5H4.5ZM7 5.25C7.41421 5.25 7.75 5.58579 7.75 6C7.75 6.41421 7.41421 6.75 7 6.75C6.58579 6.75 6.25 6.41421 6.25 6C6.25 5.58579 6.58579 5.25 7 5.25ZM7 14.25C7.41421 14.25 7.75 14.5858 7.75 15V17C7.75 17.4142 7.41421 17.75 7 17.75C6.58579 17.75 6.25 17.4142 6.25 17V15C6.25 14.5858 6.58579 14.25 7 14.25Z")),
    ("storage", IconSource::Path("M12 2C7.02944 2 3 3.79086 3 6V18C3 20.2091 7.02944 22 12 22C16.9706 22 21 20.2091 21 18V6C21 3.79086 16.9706 2 12 2ZM12 3.5C16.4183 3.5 19.5 4.97715 19.5 6C19.5 7.02285 16.4183 8.5 12 8.5C7.58172 8.5 4.5 7.02285 4.5 6C4.5 4.97715 7.58172 3.5 12 3.5ZM4.5 8.3455C6.12225 9.35448 8.82288 10 12 10C15.1771 10 17.8778 9.35448 19.5 8.3455V12C19.5 13.0228 16.4183 14.5 12 14.5C7.58172 14.5 4.5 13.0228 4.5 12V8.3455ZM4.5 14.3455C6.12225 15.3545 8.82288 16 12 16C15.1771 16 17.8778 15.3545 19.5 14.3455V18C19.5 19.0228 16.4183 20.5 12 20.5C7.58172 20.5 4.5 19.0228 4.5 18V14.3455Z")),
    ("canvas", IconSource::Path("M4 3H20C20.5523 3 21 3.44772 21 4V20C21 20.5523 20.5523 21 20 21H4C3.44772 21 3 20.5523 3 20V4C3 3.44772 3.44772 3 4 3ZM4.5 4.5V19.5H19.5V4.5H4.5ZM8 6.5C8.41421 6.5 8.75 6.83579 8.75 7.25V8.25H9.75C10.1642 8.25 10.5 8.58579 10.5 9C10.5 9.41421 10.1642 9.75 9.75 9.75H8.75V10.75C8.75 11.1642 8.41421 11.5 8 11.5C7.58579 11.5 7.25 11.1642 7.25 10.75V9.75H6.25C5.83579 9.75 5.5 9.41421 5.5 9C5.5 8.58579 5.83579 8.25 6.25 8.25H7.25V7.25C7.25 6.83579 7.58579 6.5 8 6.5ZM15.75 14.25C16.1642 14.25 16.5 14.5858 16.5 15C16.5 15.4142 16.1642 15.75 15.75 15.75H13.25C12.8358 15.75 12.5 15.4142 12.5 15C12.5 14.5858 12.8358 14.25 13.25 14.25H15.75Z")),
    ("source", IconSource::Path("M4.25 5H14.75C16.5449 5 18 6.45507 18 8.25V15.75C18 17.5449 16.5449 19 14.75 19H4.25C2.45507 19 1 17.5449 1 15.75V8.25C1 6.45507 2.45507 5 4.25 5ZM4.25 6.5C3.2835 6.5 2.5 7.2835 2.5 8.25V15.75C2.5 16.7165 3.2835 17.5 4.25 17.5H14.75C15.7165 17.5 16.5 16.7165 16.5 15.75V8.25C16.5 7.2835 15.7165 6.5 14.75 6.5H4.25ZM19.2803 8.96967L22.7803 5.46967C23.0732 5.17678 23.5481 5.17678 23.841 5.46967C23.9816 5.61032 24.0607 5.80109 24.0607 6V18C24.0607 18.4142 23.7249 18.75 23.3107 18.75C23.1118 18.75 22.921 18.671 22.7803 18.5303L19.2803 15.0303C19.1397 14.8897 19.0607 14.6989 19.0607 14.5V9.5C19.0607 9.30109 19.1397 9.11032 19.2803 8.96967ZM20.5607 9.81066V14.1893L22.5607 16.1893V7.81066L20.5607 9.81066Z")),
    ("about", IconSource::Path("M12 2C17.5228 2 22 6.47715 22 12C22 17.5228 17.5228 22 12 22C6.47715 22 2 17.5228 2 12C2 6.47715 6.47715 2 12 2ZM12 3.5C7.30558 3.5 3.5 7.30558 3.5 12C3.5 16.6944 7.30558 20.5 12 20.5C16.6944 20.5 20.5 16.6944 20.5 12C20.5 7.30558 16.6944 3.5 12 3.5ZM12 10C12.4142 10 12.75 10.3358 12.75 10.75V16.25C12.75 16.6642 12.4142 17 12 17C11.5858 17 11.25 16.6642 11.25 16.25V10.75C11.25 10.3358 11.5858 10 12 10ZM12 6.75C12.5523 6.75 13 7.19772 13 7.75C13 8.30228 12.5523 8.75 12 8.75C11.4477 8.75 11 8.30228 11 7.75C11 7.19772 11.4477 6.75 12 6.75Z")),
];

/// 空图标兜底：未知名字渲染为空白，不 panic。
static EMPTY_ICON: Icon = Icon::EMPTY;

/// 取已解析的图标数据。进程内首次调用时一次性解析全部注册表，之后零开销查询。
pub fn icon_data(name: &str) -> &'static Icon {
    static CACHE: OnceLock<HashMap<&'static str, Icon>> = OnceLock::new();
    let map = CACHE.get_or_init(|| {
        let mut map = HashMap::with_capacity(ICONS.len());
        for (name, source) in ICONS {
            let parsed: Result<viewicon::Icon, String> = match source {
                IconSource::Svg(text) => viewicon::parse_svg(text).map_err(|e| e.to_string()),
                IconSource::Path(d) => viewicon::parse_path_data(d)
                    .map(|segments| viewicon::Icon {
                        view_box: (0.0, 0.0, 24.0, 24.0),
                        paint: viewicon::Paint::CURRENT_COLOR,
                        segments,
                    })
                    .map_err(|e| e.to_string()),
            };
            match parsed {
                Ok(icon) => {
                    map.insert(*name, icon);
                }
                Err(err) => {
                    #[cfg(debug_assertions)]
                    eprintln!("[icons] 解析失败 name={name} err={err:?}");
                    #[cfg(not(debug_assertions))]
                    let _ = err;
                }
            }
        }
        map
    });
    map.get(name).unwrap_or(&EMPTY_ICON)
}

/// 渲染图标：viewicon 解析 + tiny-skia 预栅格化纹理（不再走 resvg / canvas 几何）。
///
/// `size` 为正方形边长（逻辑像素）；`color` 由调用点给定，
/// 图标可随按钮选中/hover 状态变色。
/// 首次组合时栅格化（每 name+size+color 一次），之后命中缓存零开销。
pub fn icon<'a, Message: 'a>(name: &str, size: f32, color: Color) -> Element<'a, Message> {
    let data = icon_data(name);
    if let Some(handle) = raster_handle(data, size, color) {
        // FilterMethod 默认 Linear：2x 栅格 → 逻辑尺寸由 GPU 双线性降采样，
        // 125% DPI 下为 1.6x 降采样，边缘平滑
        return image_widget(handle).width(size).height(size).into();
    }
    // 兜底：PaintMode::None / 空路径 / 栅格化失败 → canvas 空绘制（不 panic）
    canvas(IconCanvas { icon: data, color })
        .width(size)
        .height(size)
        .into()
}

/// 栅格缓存键：icon 数据指针（'static Icon 身份）+ 逻辑尺寸位型 + rgba8 量化色。
type CacheKey = (usize, u32, [u8; 4]);

/// 预栅格化纹理缓存：进程内每 (icon, size, color) 只栅格化一次。
/// 失败/不可渲染条目也缓存（None），避免每帧重复尝试。
fn raster_handle(data: &Icon, size: f32, color: Color) -> Option<iced::widget::image::Handle> {
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

    static CACHE: OnceLock<RwLock<HashMap<CacheKey, Option<iced::widget::image::Handle>>>> = OnceLock::new();
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

/// 把已解析路径段用 tiny-skia 栅格化为 straight-RGBA 纹理。
/// 2× 逻辑分辨率 + 1px 边距（避免边界裁切与图集线性采样渗色）。
const RASTER_SS: f32 = 2.0;

fn rasterize(icon: &Icon, size: f32, color: Color) -> Option<iced::widget::image::Handle> {
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

    // tiny-skia 输出预乘 alpha；iced Handle::from_rgba 收 straight RGBA
    // （image 管线 SrcAlpha/OneMinusSrcAlpha 混合，shader 无预乘）
    let mut rgba = Vec::with_capacity((px * px * 4) as usize);
    for p in pixmap.pixels() {
        let c = p.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }

    Some(iced::widget::image::Handle::from_rgba(px, px, rgba))
}

/// canvas 渲染单元（仅兜底路径使用）：把已解析的路径段缩放到 widget bounds 并填充。
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
            // viewBox → bounds 等比缩放 + 居中（viewBox 支持 min-x/min-y 偏移）
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
