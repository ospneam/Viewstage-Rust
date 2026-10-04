//! 笔画/橡皮擦的**起笔**与悬停光标：把屏幕输入变成画布坐标几何。
//!
//! # 为什么起笔算图形而不是应用逻辑
//!
//! 起笔要做的事全在几何侧：屏幕→画布逆变换、笔锋起始锥度（`apply_start_taper`）、
//! 笔速环形缓冲复位、橡皮擦尺寸按当前缩放换算。这些与「用户选了哪个工具」无关，
//! 是"一支笔落在纸上时纸上先出现什么"。应用层只该提供参数。
//!
//! # 参数而不是依赖
//!
//! [`PenParams`] 是纯数据。本模块不读配置、不认识 `Config`——配置由应用翻译成
//! [`PenParams`] 后传入，于是 viewgfx 不必依赖应用的类型。

use std::time::Instant;

use viewink::ink::stroke::DrawStroke;
use viewink::ink::tessellator::apply_start_taper;
use viewink::live::EraserCursor;

/// 一笔的起笔参数（由宿主从配置翻译而来）。
#[derive(Debug, Clone)]
pub struct PenParams {
    /// 笔色（CSS 十六进制，如 `#ef4444`）。
    pub color: String,
    /// 笔宽（**画布坐标**，即已除以当前缩放）。
    pub width: f32,
    /// 笔锋收尾最短宽度比。
    pub min_width_ratio: f32,
    /// 笔锋收尾时间窗（毫秒）。
    pub tail_duration: f32,
    /// 橡皮擦尺寸（**屏幕坐标**；本模块按当前缩放换算为画布宽度）。
    pub eraser_size_screen: f32,
}

/// 新笔画的预留点数。
///
/// 一次连续书写通常在 200 点以内，预留让整笔**零扩容**——扩容发生在笔画进行中
/// 的话会挤占帧预算，而书写正是延迟最敏感的路径。
const STROKE_POINT_RESERVE: usize = 200;

/// 起笔：构造笔画并写入共享实时状态。
///
/// `id` 由宿主分配（笔画 id 属于文档模型，不是渲染状态）。
///
/// `trail_color` 出参是预解析的 `(r, g, b)`——触控预测轨迹每帧都要用它，避免
/// 每帧 `from_str_radix`。
pub fn begin_pen_stroke(
    live: &viewink::live::LiveCanvasState,
    params: &PenParams,
    id: u64,
    canvas_x: f32,
    canvas_y: f32,
) -> (f32, f32, f32) {
    live.pen_speed_buffer.set([0.0; 3]);
    live.pen_speed_count.set(0);
    live.pen_speed_write.set(0);
    live.pen_stroke_start_time.set(Some(Instant::now()));
    live.pen_last_move_time.set(Some(Instant::now()));
    live.pen_total_segments.set(1);

    let mut s = DrawStroke::new(
        params.color.clone(),
        params.width,
        id,
        params.min_width_ratio,
        params.tail_duration,
    );
    s.points.reserve(STROKE_POINT_RESERVE);
    s.variable_widths.reserve(STROKE_POINT_RESERVE);
    s.variable_widths.push(apply_start_taper(params.width, params.width, 0));
    s.segment_times = Some(vec![0.0]);
    s.add_point(canvas_x, canvas_y, canvas_x, canvas_y);
    *live.current_stroke.borrow_mut() = Some(s);

    hex_to_f32_rgb(&params.color)
}

/// 起擦：构造橡皮擦笔画并写入共享实时状态。
///
/// `eraser_size_screen` 是**屏幕**坐标的橡皮擦直径，按当前缩放换算为画布宽度——
/// 擦除范围随缩放变化才是符合直觉的（放大画布后，同样大小的橡皮擦在屏幕上盖住
/// 更少的笔迹）。
pub fn begin_eraser_stroke(
    live: &viewink::live::LiveCanvasState,
    eraser_size_screen: f32,
    id: u64,
    canvas_x: f32,
    canvas_y: f32,
) {
    let base_eraser_size = eraser_size_screen / live.transform.scale.get();
    let mut eraser_stroke = DrawStroke::new_eraser(base_eraser_size, id);
    eraser_stroke.points.reserve(STROKE_POINT_RESERVE);
    eraser_stroke.variable_widths.reserve(STROKE_POINT_RESERVE);
    eraser_stroke.add_point(canvas_x, canvas_y, canvas_x, canvas_y);
    eraser_stroke.variable_widths.push(base_eraser_size);
    *live.current_eraser_stroke.borrow_mut() = Some(eraser_stroke);
}

/// 橡皮擦悬停光标：画布坐标 + 半径 + 边框宽度（GPU 动态层消费）。
///
/// 边框按缩放取倒数，使**屏幕上**的边框粗细恒为 2px——否则放大画布时光标圆环
/// 会细到看不见。
pub fn update_eraser_cursor(
    live: &viewink::live::LiveCanvasState,
    eraser_size_screen: f32,
    canvas_x: f32,
    canvas_y: f32,
) {
    let scale = live.transform.scale.get();
    live.eraser_cursor.set(Some(EraserCursor {
        cx: canvas_x,
        cy: canvas_y,
        radius: eraser_size_screen / 2.0 / scale,
        border: 2.0 / scale,
    }));
}

/// `#rrggbb` → 线性 `(r, g, b)`，各分量归一到 `0.0..=1.0`。
///
/// 解析失败一律回退白色（而不是黑色）：光标/轨迹用白色在深色与浅色背景上都不会
/// 消失，而黑色在深色背景上完全不可见。
pub fn hex_to_f32_rgb(hex: &str) -> (f32, f32, f32) {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return (1.0, 1.0, 1.0);
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(255) as f32 / 255.0;
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(255) as f32 / 255.0;
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(255) as f32 / 255.0;
    (r, g, b)
}
