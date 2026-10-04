use iced::alignment::{Horizontal, Vertical};
use iced::widget::slider::{
    Handle, HandleShape, Rail, Status as SliderStatus, Style as SliderStyle,
};
use iced::widget::{button, column, container, row, slider, stack, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length, Shadow};

use crate::app::state::DrawMode;
use crate::Message;

pub const PEN_COLORS: &[&str] = &[
    "#ef4444", "#f97316", "#eab308", "#22c55e", "#06b6d4", "#3b82f6", "#6366f1", "#a855f7",
    "#ec4899", "#f43f5e", "#14b8a6", "#64748b", "#1e293b", "#000000", "#ffffff",
];

pub const PEN_SIZE_PRESETS: &[f32] = &[2.0, 5.0, 10.0, 15.0, 21.0];
pub const ERASER_SIZE_PRESETS: &[f32] = &[5.0, 15.0, 25.0, 38.0, 50.0];

// CSS 变量对应值（1:1 from theme.css）
const COLOR_CANVAS: Color = Color::from_rgb8(26, 26, 26);
const COLOR_HAIRLINE: Color = Color::from_rgba8(255, 255, 255, 25.0 / 255.0);
const COLOR_MUTED: Color = Color::from_rgba8(255, 255, 255, 153.0 / 255.0);

fn hex_to_color(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return Color::WHITE;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(255) as f32 / 255.0;
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(255) as f32 / 255.0;
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(255) as f32 / 255.0;
    Color::from_rgba(r, g, b, 1.0)
}

/// 画笔圆点大小（1:1 from main.js: Math.round(value + 4)）
fn pen_dot_size(value: f32) -> f32 {
    (value + 4.0).min(28.0)
}

/// 橡皮擦圆点大小（1:1 from main_build_eraser_presets: dot = 4 + (value - minVal) / range * 24）
fn eraser_dot_size(value: f32) -> f32 {
    let min_val = ERASER_SIZE_PRESETS[0];
    let max_val = ERASER_SIZE_PRESETS[ERASER_SIZE_PRESETS.len() - 1];
    let range = max_val - min_val;
    4.0 + (value - min_val) / range * 24.0
}

/// 画笔控制面板容器样式（1:1 from .pen-control-panel）
fn panel_style() -> container::Style {
    container::Style {
        background: Some(COLOR_CANVAS.into()),
        border: Border {
            radius: 12.0.into(),
            width: 1.0,
            color: COLOR_HAIRLINE,
        },
        shadow: Shadow {
            color: Color::from_rgba8(0, 0, 0, 0.08),
            offset: iced::Vector::new(0.0, 4.0),
            blur_radius: 12.0,
        },
        text_color: None,
        snap: false,
    }
}

/// 尺寸预设按钮样式（1:1 from .size-preset-btn）
fn size_btn_style(is_active: bool) -> button::Style {
    button::Style {
        // 激活/非激活同色（视觉上仅靠边框/文字区分）
        background: Some(Color::from_rgb8(42, 42, 42).into()),
        text_color: Color::WHITE,
        border: Border {
            radius: 16.0.into(),
            width: 2.0,
            color: if is_active {
                Color::WHITE
            } else {
                COLOR_HAIRLINE
            },
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

pub fn view(app: &crate::App) -> Element<'_, Message> {
    match app.draw_mode {
        DrawMode::Comment => pen_panel_view(app),
        DrawMode::Eraser => eraser_panel_view(app),
        DrawMode::Move => column![].into(),
    }
}

/// 画笔面板（1:1 from .pen-control-panel flex-direction: row）
/// 水平排列：pen-size-presets | pen-color-buttons
fn pen_panel_view(app: &crate::App) -> Element<'_, Message> {
    let pen_color = hex_to_color(&app.get_pen_color());
    let current_size = PEN_SIZE_PRESETS
        .get(app.selected_size)
        .copied()
        .unwrap_or(5.0);

    // pen-size-presets（1:1 from .pen-size-presets: flex-direction: row, gap: 6px）
    let size_label = text(format!("{:.0}px", current_size))
        .size(11)
        .color(COLOR_MUTED);

    let mut size_row = row![].spacing(6).align_y(Alignment::Center);
    for (i, &size) in PEN_SIZE_PRESETS.iter().enumerate() {
        let is_active = i == app.selected_size;
        let dot_size = pen_dot_size(size);
        let dot_color = pen_color;

        let dot = container(Space::new().width(dot_size).height(dot_size)).style(move |_| {
            container::Style {
                background: Some(dot_color.into()),
                border: Border {
                    radius: (dot_size / 2.0).into(),
                    ..Border::default()
                },
                ..container::Style::default()
            }
        });

        let btn = button(
            container(dot)
                .width(32)
                .height(32)
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center),
        )
        .on_press(Message::SizeSelected(i))
        .padding(0)
        .style(move |_, _| size_btn_style(is_active));

        size_row = size_row.push(btn);
    }
    size_row = size_row.push(size_label);

    let size_presets = container(size_row)
        .padding([0, 2])
        .style(|_| container::Style::default());

    // pen-color-buttons（1:1 from .pen-color-buttons: grid 5×3, gap: 6px）
    let mut color_grid = column![].spacing(6);
    for row_idx in 0..3 {
        let mut color_row = row![].spacing(6);
        for col_idx in 0..5 {
            let i = row_idx * 5 + col_idx;
            if i < PEN_COLORS.len() {
                let hex = PEN_COLORS[i];
                let color = hex_to_color(hex);
                let is_active = i == app.selected_color;
                let is_dark = hex == "#000000";
                let is_light = hex == "#ffffff";

                let border_width = if is_active {
                    2.0
                } else if is_dark || is_light {
                    1.0
                } else {
                    0.0
                };

                let border_color = if is_active {
                    Color::WHITE
                } else if is_dark || is_light {
                    COLOR_HAIRLINE
                } else {
                    Color::TRANSPARENT
                };

                let btn = button(container(Space::new().width(24).height(24)))
                    .on_press(Message::ColorSelected(i))
                    .padding(0)
                    .style(move |_, _| button::Style {
                        background: Some(color.into()),
                        text_color: Color::WHITE,
                        border: Border {
                            radius: 12.0.into(),
                            width: border_width,
                            color: border_color,
                        },
                        shadow: Shadow::default(),
                        snap: false,
                    });

                color_row = color_row.push(btn);
            }
        }
        color_grid = color_grid.push(color_row);
    }

    // 画笔面板：水平排列 size-presets + color-buttons（1:1 from .pen-control-panel flex-direction: row, gap: 4px）
    let panel = row![size_presets, color_grid]
        .spacing(4)
        .align_y(Alignment::Center);

    container(panel)
        .padding([12, 16])
        .style(|_| panel_style())
        .into()
}

/// 橡皮擦面板（1:1 from .pen-control-panel 中 eraserSizePresets 部分）
/// 原始布局：size-preset-btns | size-label | separator | slider-track
/// 全部在同一行内
fn eraser_panel_view(app: &crate::App) -> Element<'_, Message> {
    let current_size = app.config.canvas.eraser_size;

    // 橡皮擦尺寸预设行
    let size_label = text(format!("{:.0}px", current_size))
        .size(11)
        .color(COLOR_MUTED);

    let mut size_row = row![].spacing(6).align_y(Alignment::Center);
    for (i, &size) in app.config.canvas.eraser_size_presets.iter().enumerate() {
        let is_active = i == app.selected_eraser_size;
        let dot_size = eraser_dot_size(size);
        // 1:1 from .size-preset-btn::after — dot-color = var(--color-muted) = rgba(255,255,255,0.6)
        let dot_color = COLOR_MUTED;

        let dot = container(Space::new().width(dot_size).height(dot_size)).style(move |_| {
            container::Style {
                background: Some(dot_color.into()),
                border: Border {
                    radius: (dot_size / 2.0).into(),
                    ..Border::default()
                },
                ..container::Style::default()
            }
        });

        let btn = button(
            container(dot)
                .width(32)
                .height(32)
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center),
        )
        .on_press(Message::EraserSizeSelected(i))
        .padding(0)
        .style(move |_, _| size_btn_style(is_active));

        size_row = size_row.push(btn);
    }
    size_row = size_row.push(size_label);

    // 分隔线（1:1 from .toolbar-separator: width 1px, height 24px, margin 0 6px）
    let sep = container(Space::new().width(1).height(24)).style(|_| container::Style {
        background: Some(COLOR_HAIRLINE.into()),
        ..container::Style::default()
    });

    // 滑块区（1:1 from .eraser-clear-track + .eraser-clear-slider + .eraser-hint-text）
    let slider_value = app.eraser_clear_slider;

    let clear_slider = slider(0.0..=100.0, slider_value, Message::EraserClearSliderChanged)
        .on_release(Message::EraserClearSliderReleased)
        .width(Length::Fixed(120.0))
        .height(32)
        .style(move |_theme, status| {
            let accent = Color::from_rgb8(59, 130, 246);
            let thumb_color = match status {
                SliderStatus::Hovered => Color::from_rgb8(79, 150, 246),
                SliderStatus::Dragged => Color::from_rgb8(39, 110, 226),
                _ => accent,
            };
            SliderStyle {
                rail: Rail {
                    backgrounds: (thumb_color.into(), Color::from_rgb8(58, 58, 58).into()),
                    width: 32.0,
                    border: Border {
                        radius: 6.0.into(),
                        width: 0.0,
                        color: Color::TRANSPARENT,
                    },
                },
                handle: Handle {
                    shape: HandleShape::Rectangle {
                        width: 36,
                        border_radius: 6.0.into(),
                    },
                    background: Background::Color(thumb_color),
                    border_width: 0.0,
                    border_color: Color::TRANSPARENT,
                },
            }
        });

    // 提示文字叠加在滑块轨道上方（1:1 from .eraser-hint-text: position: absolute, left: 50%, top: 50%, transform: translate(-50%, -50%)）
    let hint_text = container(text("拖动清空笔迹").size(11).color(COLOR_MUTED))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center);

    let slider_section: Element<Message> = if slider_value < 1.0 {
        stack![clear_slider, hint_text].into()
    } else {
        stack![clear_slider].into()
    };

    // 橡皮擦面板：全部在同一行（1:1 from HTML 结构中 eraserSizePresets 是 pen-size-presets 类，flex-direction: row）
    let panel = row![
        size_row,
        Space::new().width(6),
        sep,
        Space::new().width(6),
        slider_section
    ]
    .spacing(0)
    .align_y(Alignment::Center);

    container(panel)
        .padding([12, 16])
        .style(|_| panel_style())
        .into()
}
