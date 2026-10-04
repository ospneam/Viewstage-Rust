//! `zephyra-iced`：把 zephyra 的 [`DisplayList`] 画到 iced 上。
//!
//! # 这个适配器的职责边界
//!
//! 它只做一件事：**把 [`DisplayList`] 里的命令逐条变成 iced 的绘制几何**。
//! 它不布局、不解释属性、不认识任何具体控件。
//!
//! # 为什么是「画 DisplayList」而不是「把 DisplayList 还原成 widget 树」
//!
//! 另一个思路是遍历 DisplayList，为每条命令生成对应的 iced 控件（Quad → `container`
//! + style，Image → `image`）。那个做法有两个问题：
//!
//! 1. **DisplayList 是绝对定位的扁平列表，iced 控件树是流式布局的。** 还原需要额外
//!    引入一层坐标系与重叠语义，而那层东西没有第二处实现可以对照，于是成了新的 bug 源。
//! 2. **它会悄悄把契约降级。** 控件树是「再走一遍 iced 的布局」，得到的绘制结果未必
//!    等于 DisplayList 里已经算好的几何——于是"两宿主产出相同 DisplayList"这条
//!    可验证的判据失效了。
//!
//! 直接绘制则没有这两层：DisplayList 里的矩形就是最终矩形。这也让迁移验证变得
//! 可以机械执行——同一棵树在 iced 与未来的 winit+wgpu 宿主下，`DisplayList` 必须
//! 逐字节相同（`zephyra` 已有断言），剩下的只是谁把它画到屏幕上。
//!
//! # 代价
//!
//! 走 iced 控件意味着丢失控件级能力（焦点、文本输入、无障碍树）。对工具栏这类
//! 纯展示 UI 无所谓；若将来需要，得另开一条控件树通道，而不是把这条改回去。

use std::sync::Arc;

use iced::widget::canvas::{self, Frame, Geometry, Program};
use iced::{Rectangle, Renderer, Size, Theme};
use iced_graphics::geometry::{Path, Stroke};

use zephyra::core::{Color, Rect};
use zephyra::paint::{Command, DisplayList, TextureRef};

/// 把 zephyra 的纹理句柄解析成 iced 的图像句柄。
///
/// # 宿主边界
///
/// 核心库刻意不认识 iced 的 `Handle`——它只持有一个不透明的 `TextureRef`（一段
/// 字符串 id）。由宿主决定 id 到实际纹理的映射，适配层因此不需要知道 `viewicon`
/// 或任何具体资产系统。
pub trait TextureResolver {
    /// 取纹理；`None` 表示该 id 未注册，对应命令会被跳过。
    fn resolve(&self, texture: &TextureRef) -> Option<iced::widget::image::Handle>;
}

/// 不解析任何纹理的解析器（缺省用；图像命令被静默跳过）。
///
/// 存在的意义是让「只有一个空 DisplayList」的用法不必造一个 resolver。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTextures;

impl TextureResolver for NoTextures {
    fn resolve(&self, _texture: &TextureRef) -> Option<iced::widget::image::Handle> {
        None
    }
}

/// 以闭包为解析器。
impl<F> TextureResolver for F
where
    F: Fn(&TextureRef) -> Option<iced::widget::image::Handle>,
{
    fn resolve(&self, texture: &TextureRef) -> Option<iced::widget::image::Handle> {
        self(texture)
    }
}

/// 一个待绘制的 [`DisplayList`]。
///
/// 持 `Arc` 是因为 iced 的 `Element` 是 `'static` 的：它比调用栈活得久，而
/// DisplayList 每帧可能被重建。共享一份引用比每帧深拷贝一份命令列表便宜得多
/// ——工具栏那种几百条命令的量级下，拷贝是可测量的开销。
pub struct DisplayListProgram<R = NoTextures> {
    list: Arc<DisplayList>,
    textures: R,
}

impl<R> DisplayListProgram<R> {
    /// 用共享的 DisplayList 与纹理解析器构造。
    pub fn new(list: Arc<DisplayList>, textures: R) -> Self {
        Self { list, textures }
    }

    /// 共享的 DisplayList。
    pub fn list(&self) -> &Arc<DisplayList> {
        &self.list
    }
}

impl<M, R: TextureResolver> Program<M> for DisplayListProgram<R> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());

        for item in self.list.iter() {
            match &item.command {
                Command::Quad {
                    rect,
                    radius,
                    fill,
                    stroke,
                } => {
                    let r = to_iced_rect(*rect, bounds);
                    // iced 的 `fill_rectangle` 只画直角，圆角必须走 `Path`。
                    // 这正是 zephyra 把 Quad 定成"带圆角的一等图元"所要的东西——
                    // 而 iced 这里没有对应的重载，于是适配层多一次路径构造。
                    //
                    // 半径为 0 时退回矩形路径：lyon 的 `rounded_rectangle` 对 0 半径
                    // 也成立，但显式走 `rectangle` 让"直角落"不进过曲线生成。
                    let path = if radius.radius > 0.0 {
                        Path::rounded_rectangle(
                            r.position(),
                            Size::new(r.width, r.height),
                            iced::border::Radius::from(radius.radius),
                        )
                    } else {
                        Path::rectangle(r.position(), Size::new(r.width, r.height))
                    };
                    if fill.a > 0.0 {
                        frame.fill(&path, to_iced_color(*fill));
                    }
                    if let Some((color, width)) = stroke {
                        if color.a > 0.0 && *width > 0.0 {
                            frame.stroke(&path, Stroke::default().with_color(to_iced_color(*color)).with_width(*width));
                        }
                    }
                }
                Command::Image { texture, dst, .. } => {
                    if let Some(handle) = self.textures.resolve(texture) {
                        frame.draw_image(to_iced_rect(*dst, bounds), &handle);
                    }
                }
                Command::PushClip(_) | Command::PopClip => {
                    // `with_clip` 是**作用域式**的（闭包内生效），而 DisplayList 里的
                    // PushClip/PopClip 是跨命令的显式配对。作用域与流式配对语义不同，
                    // 无法在一次遍历里直接落地。
                    //
                    // 这里刻意**不**画裁剪：漏裁剪的表现是"内容溢出边界仍可见"，
                    // 一眼能看出来；而错误地实现一套作用域状态机则可能把内容
                    // 裁掉或裁错，问题更隐蔽。等真有控件用到裁剪时再单独实现。
                }
                // Path / Text 暂不支持：zephyra 的这两条命令依赖宿主的三角化器与
                // 字形整形，iced 的 canvas 层没有对应入口。
                //
                // 这里**显式不画**而不是画个近似——一个画错的图标比一个缺失的图标
                // 更难排查。等真的用到时再补，届时走 iced 的 `Path`/`text` 原语。
                Command::Path { .. } | Command::Text { .. } => {}
            }
        }

        vec![frame.into_geometry()]
    }
}

/// zephyra 矩形 → iced 矩形。
///
/// iced 的坐标原点在元素左上角，与 zephyra 一致，所以这里只有尺寸换算。
/// 若将来引入缩放（HiDPI 或 canvas 的 DPR），这是**唯一**该出现倍率的地方——
/// 分散在多处换算正是「同一棵树两个宿主几何不一致」的典型成因。
fn to_iced_rect(rect: Rect, bounds: Rectangle) -> Rectangle {
    Rectangle {
        x: bounds.x + rect.x,
        y: bounds.y + rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// zephyra 颜色 → iced 颜色。
fn to_iced_color(color: Color) -> iced::Color {
    iced::Color {
        r: color.r as f32,
        g: color.g as f32,
        b: color.b as f32,
        a: color.a as f32,
    }
}

/// 造一个绘制给定 [`DisplayList`] 的 iced 元素。
///
/// 尺寸由调用方给定：DisplayList 是绝对定位的列表，本身不含"我该占多大"的意图，
/// 而 iced 要求元素在布局阶段就报出尺寸。
pub fn view<M, R: TextureResolver>(
    list: Arc<DisplayList>,
    textures: R,
    size: Size,
) -> canvas::Canvas<DisplayListProgram<R>, M> {
    canvas::Canvas::new(DisplayListProgram::new(list, textures))
        .width(size.width)
        .height(size.height)
}