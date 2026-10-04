use iced::alignment::{Horizontal, Vertical};
use iced::widget::{button, column, container, image, row, scrollable, stack, text, Space};
use iced::{Border, Color, ContentFit, Element, Length, Padding, Shadow};

use super::icons;
use crate::Message;

// CSS 变量对应值（1:1 from theme.css）
const COLOR_CANVAS: Color = Color::from_rgb8(26, 26, 26);
const COLOR_INK: Color = Color::from_rgb8(255, 255, 255);
const COLOR_HAIRLINE: Color = Color::from_rgba8(255, 255, 255, 25.0 / 255.0);
const COLOR_SURFACE_CARD: Color = Color::from_rgb8(42, 42, 42);
const COLOR_SURFACE_STRONG: Color = Color::from_rgb8(58, 58, 58);
const COLOR_MUTED: Color = Color::from_rgba8(255, 255, 255, 153.0 / 255.0);
const COLOR_BRAND_ACCENT: Color = Color::from_rgb8(59, 130, 246);
const COLOR_ERROR: Color = Color::from_rgb8(239, 68, 68);

/// 侧边栏容器样式（1:1 from .sidebar）
fn sidebar_style() -> container::Style {
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

/// 导入图片按钮样式（1:1 from .sidebar-import-btn）
fn import_btn_style(status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => COLOR_SURFACE_STRONG,
        _ => COLOR_SURFACE_CARD,
    };
    let border_color = match status {
        button::Status::Hovered => COLOR_BRAND_ACCENT,
        _ => COLOR_HAIRLINE,
    };
    button::Style {
        background: Some(bg.into()),
        text_color: COLOR_INK,
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: border_color,
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

/// 删除按钮样式（1:1 from .sidebar-btn-delete）
fn delete_btn_style(status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Color::from_rgba8(239, 68, 68, 0.9),
        _ => COLOR_ERROR,
    };
    button::Style {
        background: Some(bg.into()),
        text_color: Color::WHITE,
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

pub fn view(app: &crate::App) -> Element<'_, Message> {
    // sidebar-header（1:1 from .sidebar-header）
    let header = container(text("图片列表").size(14).color(COLOR_INK))
        .padding([8, 4])
        .width(Length::Fill)
        .align_x(Horizontal::Center)
        .style(|_| container::Style {
            border: Border {
                color: COLOR_HAIRLINE,
                width: 0.0,
                ..Border::default()
            },
            ..container::Style::default()
        });

    // 分隔线
    let header_sep =
        container(Space::new().width(Length::Fill).height(1)).style(|_| container::Style {
            background: Some(COLOR_HAIRLINE.into()),
            ..container::Style::default()
        });

    // sidebar-content（1:1 from .sidebar-content）
    let mut content = column![].spacing(8).width(Length::Fill);

    if app.image_list.is_empty() {
        // sidebar-empty（1:1 from .sidebar-empty）
        content = content.push(
            container(text("暂无图片").size(13).color(COLOR_MUTED))
                .width(Length::Fill)
                .align_x(Horizontal::Center)
                .padding(20),
        );
    } else {
        for (index, img_data) in app.image_list.iter().enumerate() {
            let is_active = app.current_image_index == Some(index);
            let idx = index;
            let thumb = image(img_data.handle.clone())
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(ContentFit::Contain);

            // 缩略图容器（16:9 比例）
            let thumb_container = container(thumb).width(Length::Fill).height(138);

            // 删除按钮（右上角覆盖）
            let delete_btn = button(icons::icon("trash", 14.0, COLOR_INK))
                .on_press(Message::SidebarImageDelete(idx))
                .padding(5)
                .style(|_, status| delete_btn_style(status));

            let delete_wrapper = container(delete_btn)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Right)
                .align_y(Vertical::Top)
                .padding(Padding {
                    top: 4.0,
                    right: 4.0,
                    bottom: 0.0,
                    left: 0.0,
                });

            // image-item: 外框 container 画蓝色描边，内层 button 透明
            let inner = stack![thumb_container, delete_wrapper,];
            let framed = container(
                button(inner)
                    .on_press(Message::SidebarImageSelected(idx))
                    .width(Length::Fill)
                    .padding(0)
                    .style(|_, _| button::Style {
                        background: None,
                        text_color: COLOR_INK,
                        border: Border::default(),
                        shadow: Shadow::default(),
                        snap: false,
                    }),
            )
            .width(Length::Fill)
            .padding(2.0)
            .style(move |_| container::Style {
                background: None,
                border: Border {
                    radius: 8.0.into(),
                    width: if is_active { 2.0 } else { 0.0 },
                    color: if is_active {
                        COLOR_BRAND_ACCENT
                    } else {
                        Color::TRANSPARENT
                    },
                },
                text_color: None,
                shadow: Shadow::default(),
                snap: false,
            });

            content = content.push(framed);
        }
    }

    // sidebar-import-btn（1:1 from .sidebar-import-btn: icon + text, centered）
    let icon = icons::icon("image-add", 16.0, COLOR_INK);

    let import_btn = button(
        row![icon, text("导入图片").size(13)]
            .spacing(6)
            .align_y(iced::Alignment::Center),
    )
    .on_press(Message::SidebarImportImage)
    .padding(10)
    .style(|_, status| import_btn_style(status));

    let import_btn_wrap = container(import_btn)
        .width(Length::Fill)
        .align_x(Horizontal::Center);

    // scrollable 内容区（fixed bottom: import button, 隐藏滚动条）
    let scroll_content = scrollable(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_, _| scrollable::Style {
            container: container::Style::default(),
            vertical_rail: scrollable::Rail {
                background: None,
                border: Border::default(),
                scroller: scrollable::Scroller {
                    background: Color::TRANSPARENT.into(),
                    border: Border::default(),
                },
            },
            horizontal_rail: scrollable::Rail {
                background: None,
                border: Border::default(),
                scroller: scrollable::Scroller {
                    background: Color::TRANSPARENT.into(),
                    border: Border::default(),
                },
            },
            gap: None,
            auto_scroll: scrollable::AutoScroll {
                background: Color::TRANSPARENT.into(),
                border: Border::default(),
                shadow: Shadow::default(),
                icon: Color::TRANSPARENT,
            },
        });

    // 整体布局（1:1 from .sidebar: flex-direction column, gap 12px, padding 12px, width 270px）
    let sidebar = column![
        header,
        header_sep,
        Space::new().height(8),
        scroll_content,
        import_btn_wrap,
    ]
    .spacing(8)
    .width(Length::Fill)
    .height(Length::Fill);

    container(sidebar)
        .width(270)
        .height(Length::Fill)
        .padding(12)
        .style(|_| sidebar_style())
        .into()
}
