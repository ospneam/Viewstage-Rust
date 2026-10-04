use iced::alignment::Horizontal;
use iced::widget::slider::{
    Handle, HandleShape, Rail, Status as SliderStatus, Style as SliderStyle,
};
use iced::widget::{button, column, container, row, slider, text, Space};
use iced::{Background, Border, Color, Element, Length, Padding, Shadow};

use crate::Message;

// CSS 变量对应值（1:1 from theme.css dark 主题）
const COLOR_SURFACE_ELEVATED: Color = Color::from_rgb8(58, 58, 58);
const COLOR_SURFACE_STRONG: Color = Color::from_rgb8(58, 58, 58);
const COLOR_SURFACE_SOFT: Color = Color::from_rgb8(42, 42, 42);
const COLOR_HAIRLINE: Color = Color::from_rgba8(255, 255, 255, 25.0 / 255.0);
const COLOR_HAIRLINE_SOFT: Color = Color::from_rgba8(255, 255, 255, 13.0 / 255.0);
const COLOR_INK: Color = Color::from_rgb8(255, 255, 255);
const COLOR_BODY: Color = Color::from_rgb8(229, 229, 229);
const COLOR_BRAND_ACCENT: Color = Color::from_rgb8(59, 130, 246);
const COLOR_ON_PRIMARY: Color = Color::from_rgb8(17, 17, 17);

/// 设置面板容器样式（1:1 from .settings-panel）
fn panel_style() -> container::Style {
    container::Style {
        background: Some(COLOR_SURFACE_ELEVATED.into()),
        border: Border {
            radius: 16.0.into(),
            width: 1.0,
            color: COLOR_HAIRLINE,
        },
        shadow: Shadow {
            color: Color::from_rgba8(0, 0, 0, 0.4),
            offset: iced::Vector::new(0.0, 8.0),
            blur_radius: 32.0,
        },
        text_color: None,
        snap: false,
    }
}

/// 设置行中旋转按钮样式（1:1 from .settings-btn）
fn settings_btn_style(status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => COLOR_SURFACE_SOFT,
        _ => COLOR_SURFACE_SOFT,
    };
    let border_color = match status {
        button::Status::Hovered => COLOR_HAIRLINE,
        _ => COLOR_HAIRLINE,
    };
    button::Style {
        background: Some(bg.into()),
        text_color: COLOR_BODY,
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: border_color,
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// 选项按钮样式（1:1 from .option-btn）
fn option_btn_style(is_active: bool, _status: button::Status) -> button::Style {
    let (bg, text_color) = if is_active {
        (Background::Color(COLOR_INK), COLOR_ON_PRIMARY)
    } else {
        (Background::Color(Color::TRANSPARENT), COLOR_BODY)
    };
    let opacity = if is_active { 1.0 } else { 0.6 };
    button::Style {
        background: Some(bg),
        text_color: Color::from_rgba(text_color.r, text_color.g, text_color.b, opacity),
        border: Border {
            radius: 4.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// 重置按钮样式（1:1 from .settings-reset-btn）
fn reset_btn_style(status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => COLOR_SURFACE_ELEVATED,
        _ => COLOR_SURFACE_STRONG,
    };
    button::Style {
        background: Some(bg.into()),
        text_color: COLOR_BODY,
        border: Border {
            radius: 6.0.into(),
            width: 1.0,
            color: COLOR_HAIRLINE,
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// 设置滑块样式（1:1 from .settings-slider-row input[type="range"]）
/// Iced slider 的 handle 高度 = bounds.height = rail width，
/// 原版 CSS 中轨道 8px + thumb 20px（margin-top: -6px 超出轨道）无法实现，
/// 折中：rail width = 20 让 thumb 有正确高度，轨道稍粗但比例协调
fn settings_slider_style(status: SliderStatus) -> SliderStyle {
    let accent = COLOR_BRAND_ACCENT;
    let active_color = match status {
        SliderStatus::Hovered => Color::from_rgb8(79, 150, 246),
        SliderStatus::Dragged => Color::from_rgb8(39, 110, 226),
        _ => accent,
    };
    let border_color = match status {
        SliderStatus::Hovered => Color::from_rgb8(79, 150, 246),
        SliderStatus::Dragged => Color::from_rgb8(39, 110, 226),
        _ => accent,
    };
    SliderStyle {
        rail: Rail {
            backgrounds: (active_color.into(), COLOR_SURFACE_STRONG.into()),
            width: 8.0,
            border: Border {
                radius: 9999.0.into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
        },
        handle: Handle {
            shape: HandleShape::Rectangle {
                width: 28,
                border_radius: 10.0.into(),
            },
            background: Background::Color(COLOR_ON_PRIMARY),
            border_width: 2.0,
            border_color,
        },
    }
}

/// 滑块值标签样式（1:1 from .settings-slider-val）
fn slider_val_style() -> container::Style {
    container::Style {
        background: Some(Color::from_rgba8(59, 130, 246, 0.12).into()),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// 分隔线样式（1:1 from border-top: 1px solid var(--color-hairline-soft)）
fn separator_style() -> container::Style {
    container::Style {
        background: Some(COLOR_HAIRLINE_SOFT.into()),
        ..container::Style::default()
    }
}

pub fn view(app: &crate::App) -> Element<'_, Message> {
    // ===== 旋转行（1:1 from .settings-row: 旋转 ↺ ↻） =====
    let rotate_label = container(text("旋转").size(13).color(COLOR_BODY)).width(52);
    let btn_rotate_left = button(text("↺").size(16))
        .on_press(Message::RotateLeft)
        .width(36)
        .height(36)
        .style(|_, status| settings_btn_style(status));
    let btn_rotate_right = button(text("↻").size(16))
        .on_press(Message::RotateRight)
        .width(36)
        .height(36)
        .style(|_, status| settings_btn_style(status));
    let rotate_row = row![
        rotate_label,
        Space::new().width(Length::Fill),
        btn_rotate_left,
        btn_rotate_right
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);

    // ===== 分隔线 =====
    let sep1 = container(Space::new().width(Length::Fill).height(1)).style(|_| separator_style());

    // ===== 亮度滑块行（1:1 from .settings-slider-row） =====
    let brightness_label = container(text("亮度").size(13).color(COLOR_BODY)).width(52);
    let brightness_slider = slider(
        -100.0..=100.0,
        app.camera_brightness,
        Message::BrightnessChanged,
    )
    .width(Length::Fill)
    .style(|_, status| settings_slider_style(status));
    let brightness_val = container(
        text(format!("{:.0}", app.camera_brightness))
            .size(11)
            .color(COLOR_INK),
    )
    .padding([2, 6])
    .width(36)
    .align_x(Horizontal::Center)
    .style(|_| slider_val_style());
    let brightness_row = row![brightness_label, brightness_slider, brightness_val]
        .spacing(12)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill)
        .padding(Padding {
            top: 10.0,
            bottom: 10.0,
            left: 0.0,
            right: 0.0,
        });

    // ===== 分隔线 =====
    let sep2 = container(Space::new().width(Length::Fill).height(1)).style(|_| separator_style());

    // ===== 对比度滑块行（1:1 from .settings-slider-row） =====
    let contrast_label = container(text("对比度").size(13).color(COLOR_BODY)).width(52);
    let contrast_slider = slider(0.0..=300.0, app.camera_contrast, Message::ContrastChanged)
        .width(Length::Fill)
        .style(|_, status| settings_slider_style(status));
    let contrast_display = app.camera_contrast / 100.0;
    let contrast_val = container(
        text(format!("{:.2}", contrast_display))
            .size(11)
            .color(COLOR_INK),
    )
    .padding([2, 6])
    .width(36)
    .align_x(Horizontal::Center)
    .style(|_| slider_val_style());
    let contrast_row = row![contrast_label, contrast_slider, contrast_val]
        .spacing(12)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill)
        .padding(Padding {
            top: 10.0,
            bottom: 10.0,
            left: 0.0,
            right: 0.0,
        });

    // ===== 分隔线 =====
    let sep3 = container(Space::new().width(Length::Fill).height(1)).style(|_| separator_style());

    // ===== 黑白选项行（1:1 from .settings-row + .option-group） =====
    let grayscale_label = container(text("黑白").size(13).color(COLOR_BODY)).width(52);
    let btn_off = button(text("关闭").size(12))
        .on_press(Message::GrayscaleToggled(false))
        .padding([4, 12])
        .style(move |_, status| option_btn_style(!app.camera_grayscale, status));
    let btn_on = button(text("开启").size(12))
        .on_press(Message::GrayscaleToggled(true))
        .padding([4, 12])
        .style(move |_, status| option_btn_style(app.camera_grayscale, status));
    // 选项组容器（1:1 from .option-group: 2列网格，surface-strong背景，3px内边距，6px圆角）
    let option_group = container(row![btn_off, btn_on].spacing(0))
        .padding(3)
        .style(|_| container::Style {
            background: Some(COLOR_SURFACE_STRONG.into()),
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });
    let grayscale_row = row![
        grayscale_label,
        Space::new().width(Length::Fill),
        option_group
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill)
    .padding(Padding {
        top: 10.0,
        bottom: 10.0,
        left: 0.0,
        right: 0.0,
    });

    // ===== 分隔线 =====
    let sep4 = container(Space::new().width(Length::Fill).height(1)).style(|_| separator_style());

    // ===== 重置按钮行（1:1 from .settings-reset-row） =====
    let reset_btn = button(
        container(text("重置图片设置").size(11).color(COLOR_BODY))
            .width(Length::Fill)
            .align_x(Horizontal::Center),
    )
    .on_press(Message::ResetCameraSettings)
    .width(Length::Fill)
    .padding([4, 14])
    .style(|_, status| reset_btn_style(status));
    let reset_row = container(reset_btn).width(Length::Fill).padding(Padding {
        top: 6.0,
        bottom: 0.0,
        left: 0.0,
        right: 0.0,
    });

    // ===== 整体面板（1:1 from .settings-panel: flex column, gap 2px, padding sm） =====
    let content = column![
        rotate_row,
        sep1,
        brightness_row,
        sep2,
        contrast_row,
        sep3,
        grayscale_row,
        sep4,
        reset_row
    ]
    .spacing(2)
    .width(Length::Fill);

    container(content)
        .padding(8)
        .width(220)
        .style(|_| panel_style())
        .into()
}
